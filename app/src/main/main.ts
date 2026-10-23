// Electron main process: owns the worker process, native dialogs, and the
// preview protocol. The renderer is sandboxed and can only use the IPC
// channels registered below.

import { app, BrowserWindow, dialog, ipcMain, net, protocol, shell } from "electron";
import { spawn } from "node:child_process";
import { realpathSync } from "node:fs";
import { join, resolve, sep } from "node:path";
import { pathToFileURL } from "node:url";
import type { Classification } from "../shared/types";
import {
  assertClassification,
  assertInt,
  assertJobId,
  assertSerial,
  GrantedPaths,
  safeRelative,
  toWorkerJob,
  ValidationError,
} from "./validation";
import { locateWorker, WorkerClient } from "./worker";

protocol.registerSchemesAsPrivileged([{ scheme: "prmedia", privileges: { standard: true, secure: true, supportFetchAPI: true } }]);

const granted = new GrantedPaths();
let caseRoot: string | null = null;
let worker: WorkerClient;
let win: BrowserWindow | null = null;

function handle(channel: string, fn: (...args: unknown[]) => Promise<unknown> | unknown): void {
  ipcMain.handle(channel, async (event, ...args) => {
    // Only our own window's top frame may call.
    if (!win || event.sender !== win.webContents || event.senderFrame !== win.webContents.mainFrame) {
      throw new Error("unauthorized sender");
    }
    try {
      return await fn(...args);
    } catch (e) {
      if (e instanceof ValidationError) throw new Error(`Rejected: ${e.message}`);
      throw e;
    }
  });
}

function requireCase(): string {
  if (!caseRoot) throw new ValidationError("open or create a case first");
  return caseRoot;
}

async function chooseDir(title: string, create: boolean): Promise<string | null> {
  const r = await dialog.showOpenDialog(win!, {
    title,
    properties: create ? ["openDirectory", "createDirectory", "promptToCreate"] : ["openDirectory"],
  });
  return r.canceled || r.filePaths.length === 0 ? null : r.filePaths[0];
}

function registerIpc(): void {
  handle("case:choose", async () => {
    const dir = await chooseDir("Choose or create a case folder (outside the evidence)", true);
    if (!dir) return null;
    const res = await worker.call<{ root: string }>("case.open", { path: dir });
    caseRoot = res.root;
    return res;
  });
  handle("drives:list", async () => {
    const r = await worker.call<{ elevated: boolean; drives: { id: string }[] }>("drives.list", {});
    // Only drives the worker listed may be scanned.
    for (const d of r.drives) granted.grant("drive", d.id);
    return r;
  });
  handle("app:relaunchAdmin", () => {
    if (process.platform !== "win32") throw new ValidationError("only available on Windows");
    // Fixed command: start this same executable elevated (UAC prompt), then quit.
    const exe = process.execPath.replace(/'/g, "''");
    const args = process.defaultApp ? ` -ArgumentList '${app.getAppPath().replace(/'/g, "''")}'` : "";
    spawn("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", `Start-Process -FilePath '${exe}'${args} -Verb RunAs`], {
      detached: true,
      windowsHide: true,
      stdio: "ignore",
    }).unref();
    setTimeout(() => app.quit(), 500);
  });
  handle("image:choose", async () => {
    const r = await dialog.showOpenDialog(win!, {
      title: "Choose a raw storage image (opened read-only)",
      properties: ["openFile"],
      filters: [
        { name: "Raw images", extensions: ["img", "dd", "raw", "bin", "001", "iso"] },
        { name: "All files", extensions: ["*"] },
      ],
    });
    if (r.canceled || r.filePaths.length === 0) return null;
    granted.grant("image", r.filePaths[0]);
    return r.filePaths[0];
  });
  handle("folder:choose", async () => {
    const d = await chooseDir("Choose a backup or copy folder to search", false);
    if (d) granted.grant("folder", d);
    return d;
  });
  handle("export:choose", async () => {
    const d = await chooseDir("Choose an empty export folder (separate from the evidence)", true);
    if (d) granted.grant("export", d);
    return d;
  });
  handle("adb:status", () => worker.call("adb.status", {}));
  handle("adb:devices", () => worker.call("adb.devices", {}));
  handle("adb:info", (serial) => worker.call("adb.deviceInfo", { serial: assertSerial(serial) }));
  handle("adb:partitions", (serial, confirmation) => {
    const s = assertSerial(serial);
    if (typeof confirmation !== "string") throw new ValidationError("confirmation required");
    return worker.call("adb.listPartitions", { serial: s, confirmation });
  });
  handle("job:start", (job) => {
    requireCase();
    return worker.call("job.start", { params: toWorkerJob(job, granted) });
  });
  handle("job:resume", (id) => worker.call("job.resume", { job_id: assertJobId(id) }));
  handle("job:cancel", (id) => worker.call("job.cancel", { job_id: assertJobId(id) }));
  handle("job:list", () => (caseRoot ? worker.call("job.list", {}) : []));
  handle("results:finalize", () => worker.call("results.finalize", {}));
  handle("results:list", (offset, limit, classification) => {
    const params: Record<string, unknown> = { offset: assertInt(offset, 0, 1e9), limit: assertInt(limit, 1, 2000) };
    if (classification !== null) params.classification = assertClassification(classification);
    return worker.call("results.list", params);
  });
  handle("export:run", (dest, classes, includeDuplicates) => {
    if (!Array.isArray(classes) || classes.length === 0) throw new ValidationError("choose at least one classification");
    const cls: Classification[] = classes.map(assertClassification);
    return worker.call("export.run", {
      dest: granted.require("export", dest),
      classifications: cls,
      include_duplicates: includeDuplicates === true,
    });
  });
  handle("report:generate", () => worker.call("report.generate", {}));
  handle("report:open", async () => {
    const root = requireCase();
    const err = await shell.openPath(join(root, "report.html"));
    if (err) throw new Error(err);
  });
}

/** Serve recovered/acquired files from the open case only. */
function registerPreviewProtocol(): void {
  protocol.handle("prmedia", async (request) => {
    const root = caseRoot;
    if (!root) return new Response("no case", { status: 404 });
    const url = new URL(request.url);
    const rel = safeRelative(decodeURIComponent(url.pathname.replace(/^\/+/, "")));
    if (url.host !== "case" || !rel) return new Response("bad path", { status: 400 });
    let real: string;
    try {
      real = realpathSync(resolve(root, rel));
    } catch {
      return new Response("not found", { status: 404 });
    }
    const rootReal = realpathSync(root);
    if (!real.startsWith(rootReal + sep)) return new Response("forbidden", { status: 403 });
    return net.fetch(pathToFileURL(real).toString());
  });
}

function createWindow(): void {
  win = new BrowserWindow({
    width: 1360,
    height: 880,
    minWidth: 980,
    minHeight: 640,
    title: "PhotoRecovery",
    backgroundColor: "#111418",
    webPreferences: {
      preload: join(__dirname, "..", "preload", "preload.js"),
      contextIsolation: true,
      sandbox: true,
      nodeIntegration: false,
      webSecurity: true,
      spellcheck: false,
    },
  });
  win.removeMenu();
  // No navigation away from the app and no new windows.
  win.webContents.on("will-navigate", (e) => e.preventDefault());
  win.webContents.setWindowOpenHandler(() => ({ action: "deny" }));
  win.loadFile(join(__dirname, "..", "..", "renderer", "renderer", "index.html"));
}

app.on("web-contents-created", (_e, contents) => {
  contents.on("will-attach-webview", (e) => e.preventDefault());
});

app.whenReady().then(() => {
  worker = new WorkerClient(locateWorker(process.resourcesPath, app.getAppPath()));
  worker.on("event", (ev) => win?.webContents.send("worker:event", ev));
  worker.start();
  registerIpc();
  registerPreviewProtocol();
  createWindow();
});

app.on("window-all-closed", () => {
  worker?.stop();
  app.quit();
});

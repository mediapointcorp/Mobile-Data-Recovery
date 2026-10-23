// Electron main process: owns the worker process and native dialogs. The
// renderer is sandboxed and can only use the IPC channels registered here.

import { spawn } from "node:child_process";
import { app, BrowserWindow, dialog, ipcMain, shell } from "electron";
import { join } from "node:path";
import { assertInt, Grants, toResultsQuery, toScanRequest, ValidationError } from "./validation";
import { locateWorker, WorkerClient } from "./worker";

const grants = new Grants();
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
      if (e instanceof ValidationError) throw new Error(e.message);
      throw e;
    }
  });
}

async function chooseDir(title: string, create: boolean): Promise<string | null> {
  const r = await dialog.showOpenDialog(win!, {
    title,
    properties: create ? ["openDirectory", "createDirectory", "promptToCreate"] : ["openDirectory"],
  });
  return r.canceled || r.filePaths.length === 0 ? null : r.filePaths[0];
}

function registerIpc(): void {
  handle("drives:list", async () => {
    const r = await worker.call<{ elevated: boolean; drives: { id: string }[] }>("drives.list", {});
    for (const d of r.drives) grants.grant("drive", d.id);
    return r;
  });
  handle("folder:search", () => chooseDir("Where were the files?", false));
  handle("folder:recover", async () => {
    const d = await chooseDir("Recover the files to… (choose a folder on a DIFFERENT drive)", true);
    if (d) grants.grant("recover", d);
    return d;
  });
  handle("scan:start", (req) => worker.call("scan.start", toScanRequest(req, grants) as unknown as Record<string, unknown>));
  handle("scan:cancel", () => worker.call("scan.cancel", {}));
  handle("scan:results", (q) => worker.call("scan.results", toResultsQuery(q) as unknown as Record<string, unknown>));
  handle("scan:preview", async (id) => {
    const r = await worker.call<{ jpeg_base64: string }>("scan.preview", { id: assertInt(id, 1, Number.MAX_SAFE_INTEGER) });
    return `data:image/jpeg;base64,${r.jpeg_base64}`;
  });
  handle("scan:recover", (ids, dest, keep) => {
    if (!Array.isArray(ids) || ids.length === 0 || ids.length > 1_000_000) throw new ValidationError("select the files to recover");
    return worker.call("scan.recover", {
      ids: ids.map((i) => assertInt(i, 1, Number.MAX_SAFE_INTEGER)),
      dest: grants.require("recover", dest),
      keep_folders: keep === true,
    });
  });
  handle("shell:openFolder", async (path) => {
    const err = await shell.openPath(grants.require("recover", path));
    if (err) throw new Error(err);
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
}

function createWindow(): void {
  win = new BrowserWindow({
    width: 1280,
    height: 820,
    minWidth: 960,
    minHeight: 600,
    title: "PhotoRecovery",
    backgroundColor: "#f3f4f6",
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
  createWindow();
});

app.on("window-all-closed", () => {
  worker?.stop();
  app.quit();
});

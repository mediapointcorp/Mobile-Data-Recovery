// Renderer: Recuva-style wizard → scan → results. Plain DOM + TypeScript.
// All file names and paths are inserted with textContent, never as HTML.

import type { Category, DriveInfo, FoundFile, PhotoRecoveryApi, ResultsQuery, ScanRequest, State, WorkerEvent } from "../shared/types.js";
import { CATEGORIES, STATE_LABELS } from "../shared/types.js";

declare global {
  interface Window {
    photoRecovery: PhotoRecoveryApi;
  }
}

const api = window.photoRecovery;
const PAGE = 1000;

function $(id: string): HTMLElement {
  const e = document.getElementById(id);
  if (!e) throw new Error(`missing #${id}`);
  return e;
}

function input(id: string): HTMLInputElement {
  return $(id) as HTMLInputElement;
}

function h<K extends keyof HTMLElementTagNameMap>(tag: K, props: { class?: string; text?: string; title?: string } = {}, children: (Node | string)[] = []): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  if (props.class) el.className = props.class;
  if (props.text !== undefined) el.textContent = props.text;
  if (props.title) el.title = props.title;
  for (const c of children) el.append(c);
  return el;
}

let toastTimer: number | undefined;
function toast(msg: string, error = false, action?: { label: string; run: () => void }): void {
  const t = $("toast");
  t.replaceChildren(h("span", { text: msg }));
  if (action) {
    const b = h("button", { text: action.label });
    b.addEventListener("click", action.run);
    t.append(b);
  }
  t.className = error ? "toast error" : "toast";
  window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => t.classList.add("hidden"), error ? 12000 : 8000);
}

function errText(e: unknown): string {
  return e instanceof Error ? e.message.replace(/^Error invoking remote method '[^']+': (Error: )?/, "") : String(e);
}

async function guard<T>(p: Promise<T>): Promise<T | undefined> {
  try {
    return await p;
  } catch (e) {
    toast(errText(e), true);
    return undefined;
  }
}

function fmtBytes(n: number | null): string {
  if (n === null) return "";
  if (n < 1024) return `${n} B`;
  if (n < 1048576) return `${(n / 1024).toFixed(0)} KB`;
  if (n < 1073741824) return `${(n / 1048576).toFixed(1)} MB`;
  return `${(n / 1073741824).toFixed(2)} GB`;
}

function fmtDate(s: string | null): string {
  if (!s) return "";
  return s.replace("T", " ").replace(/Z$/, "").slice(0, 16);
}

function show(screen: "wizard" | "scanning" | "results"): void {
  for (const s of ["wizard", "scanning", "results"]) $(s).classList.toggle("hidden", s !== screen);
  $("btn-new").classList.toggle("hidden", screen !== "results");
}

// ---------- wizard ----------

let drives: DriveInfo[] = [];
let elevated = true;

const types = $("types");
for (const c of CATEGORIES) {
  const r = h("input");
  r.type = "radio";
  r.name = "type";
  r.value = c.id;
  r.checked = c.id === "pictures";
  types.append(h("label", { class: "choice" }, [r, h("div", {}, [h("b", { text: c.label }), h("span", { text: c.hint })])]));
}

function selectedType(): Category {
  return (document.querySelector<HTMLInputElement>('input[name="type"]:checked')?.value ?? "pictures") as Category;
}

function selectedLocation(): string {
  return document.querySelector<HTMLInputElement>('input[name="loc"]:checked')?.value ?? "all";
}

function scannable(d: DriveInfo): boolean {
  return d.kind === "volume" && d.drive_type !== "network" && d.drive_type !== "optical";
}

async function loadDrives(): Promise<void> {
  const r = await guard(api.drives());
  if (!r) return;
  drives = r.drives.filter(scannable);
  elevated = r.elevated;
  const sel = $("drive-select") as HTMLSelectElement;
  sel.replaceChildren(
    ...drives.map((d) => {
      const kind = [d.drive_type === "removable" ? "USB / memory card" : d.ssd ? "SSD" : d.ssd === false ? "Hard disk" : "", d.filesystem ?? "RAW"].filter(Boolean).join(", ");
      const o = h("option", { text: `${d.id} ${d.label ?? ""} — ${fmtBytes(d.size)} (${kind})` });
      o.value = d.id;
      return o;
    }),
  );
  const banner = $("admin-banner");
  banner.classList.toggle("hidden", elevated);
  banner.replaceChildren(h("span", { text: "PhotoRecovery needs administrator rights to read drives. It only reads them — nothing is changed." }));
  if (!elevated && api.platform() === "win32") {
    const b = h("button", { class: "primary", text: "Restart as administrator" });
    b.addEventListener("click", () => void guard(api.relaunchAsAdmin()));
    banner.append(b);
  }
  updateNotes();
}

function targetDrives(): string[] {
  const loc = selectedLocation();
  if (loc === "drive") return [($("drive-select") as HTMLSelectElement).value].filter(Boolean);
  if (loc === "folder") {
    const m = /^([A-Za-z]:)/.exec(input("folder-input").value.trim());
    return m ? drives.filter((d) => d.id.toUpperCase() === m[1].toUpperCase()).map((d) => d.id) : [];
  }
  return drives.map((d) => d.id);
}

function updateNotes(): void {
  const notes = $("drive-notes");
  notes.replaceChildren();
  for (const d of drives.filter((d) => targetDrives().includes(d.id))) {
    for (const n of d.notes) notes.append(h("div", { text: `${d.id} ${n}` }));
  }
}

document.querySelectorAll('input[name="loc"]').forEach((r) => r.addEventListener("change", updateNotes));
$("drive-select").addEventListener("change", () => {
  (document.querySelector('input[name="loc"][value="drive"]') as HTMLInputElement).checked = true;
  updateNotes();
});
input("folder-input").addEventListener("input", () => {
  (document.querySelector('input[name="loc"][value="folder"]') as HTMLInputElement).checked = true;
  updateNotes();
});
$("btn-browse").addEventListener("click", async () => {
  const f = await guard(api.chooseFolderToSearch());
  if (!f) return;
  input("folder-input").value = f;
  (document.querySelector('input[name="loc"][value="folder"]') as HTMLInputElement).checked = true;
  updateNotes();
});

$("btn-start").addEventListener("click", async () => {
  const loc = selectedLocation();
  const req: ScanRequest = {
    drives: targetDrives(),
    category: selectedType(),
    deep_scan: input("opt-deep").checked,
    folder: loc === "folder" ? input("folder-input").value.trim() || null : null,
    recycle_bin_only: loc === "recycle",
  };
  if (req.drives.length === 0) return toast(loc === "folder" ? "Type or browse to a folder like C:\\Users\\You\\Pictures" : "No drives available to scan.", true);
  if (!elevated) return toast("Restart PhotoRecovery as administrator to scan drives.", true);
  $("bar").style.width = "0%";
  $("scan-msg").textContent = "Starting…";
  show("scanning");
  if (!(await guard(api.startScan(req)))) show("wizard");
});

$("btn-cancel").addEventListener("click", () => void guard(api.cancelScan()));
$("btn-new").addEventListener("click", () => {
  show("wizard");
  void loadDrives();
});

// ---------- results ----------

let rows: FoundFile[] = [];
let total = 0;
let sort: ResultsQuery["sort"] = "state";
let descending = false;
const checked = new Set<number>();
let current: number | null = null;

function states(): State[] {
  return [...document.querySelectorAll<HTMLInputElement>(".st:checked")].map((c) => c.value as State);
}

async function loadResults(reset: boolean): Promise<void> {
  if (reset) rows = [];
  const text = input("filter").value.trim();
  const r = await guard(api.results({ offset: rows.length, limit: PAGE, sort, descending, text: text || null, states: states() }));
  if (!r) return;
  total = r.total;
  rows = rows.concat(r.items);
  $("found-label").textContent = `${r.all} file${r.all === 1 ? "" : "s"} found${r.total !== r.all ? ` · ${r.total} shown` : ""}`;
  if (r.summary) {
    $("notes").replaceChildren(...r.summary.notes.map((n) => h("div", { text: n })));
  }
  renderTable();
}

function renderTable(): void {
  const tbody = $("table").querySelector("tbody")!;
  tbody.replaceChildren(...rows.map(row));
  if (rows.length === 0) {
    const td = h("td", { text: "No files match. Try “All files”, another location, Deep Scan, or tick Unrecoverable." });
    td.colSpan = 7;
    tbody.append(h("tr", {}, [td]));
  }
  $("more").classList.toggle("hidden", rows.length >= total);
  document.querySelectorAll<HTMLElement>("th[data-sort]").forEach((th) => {
    th.classList.toggle("sorted", th.dataset.sort === sort);
    th.classList.toggle("desc", th.dataset.sort === sort && descending);
  });
  updateSelected();
}

function row(f: FoundFile): HTMLTableRowElement {
  const cb = h("input");
  cb.type = "checkbox";
  cb.checked = checked.has(f.id);
  cb.disabled = f.state === "unrecoverable";
  cb.addEventListener("click", (e) => e.stopPropagation());
  cb.addEventListener("change", () => {
    if (cb.checked) checked.add(f.id);
    else checked.delete(f.id);
    updateSelected();
  });
  const tr = h("tr", {}, [
    h("td", { class: "c" }, [cb]),
    h("td", { text: f.name, title: f.name }),
    h("td", { text: f.path, title: f.path }),
    h("td", { text: fmtDate(f.modified) }),
    h("td", { class: "r", text: fmtBytes(f.size) }),
    h("td", {}, [h("i", { class: `dot ${f.state}` }), STATE_LABELS[f.state]]),
    h("td", { text: f.comment, title: f.comment }),
  ]);
  if (current === f.id) tr.classList.add("selected");
  tr.addEventListener("click", () => {
    current = f.id;
    document.querySelectorAll("tr.selected").forEach((r) => r.classList.remove("selected"));
    tr.classList.add("selected");
    void showInfo(f);
  });
  tr.addEventListener("dblclick", () => {
    if (f.state !== "unrecoverable") {
      cb.checked = !cb.checked;
      cb.dispatchEvent(new Event("change"));
    }
  });
  return tr;
}

async function showInfo(f: FoundFile): Promise<void> {
  const info = $("info");
  info.replaceChildren();
  const add = (k: string, v: string | null) => {
    if (v) info.append(h("dt", { text: k }), h("dd", { text: v }));
  };
  add("Filename", f.name);
  add("Path", f.path);
  add("Size", `${fmtBytes(f.size)} (${f.size.toLocaleString()} bytes)`);
  add("State", STATE_LABELS[f.state]);
  add("Comment", f.comment);
  add("Modified", fmtDate(f.modified));
  add("Created", fmtDate(f.created));
  add("Found by", f.origin === "deep_scan" ? "Deep scan" : f.origin === "recycle_bin" ? "Recycle Bin" : `${f.filesystem} file record`);
  if (f.width && f.height) add("Dimensions", `${f.width} × ${f.height}`);
  const pv = $("preview");
  if (f.category !== "pictures" || f.state === "unrecoverable") {
    pv.replaceChildren(h("span", { class: "muted", text: f.state === "unrecoverable" ? "No preview: the data is gone" : "No preview for this file type" }));
    return;
  }
  pv.replaceChildren(h("span", { class: "muted", text: "Loading preview…" }));
  try {
    const url = await api.preview(f.id);
    if (current !== f.id) return;
    const img = h("img");
    img.alt = f.name;
    img.src = url;
    pv.replaceChildren(img);
  } catch (e) {
    if (current === f.id) pv.replaceChildren(h("span", { class: "muted", text: `No preview: ${errText(e)}` }));
  }
}

function updateSelected(): void {
  $("selected-label").textContent = checked.size ? `${checked.size} selected` : "Tick files to recover";
  ($("btn-recover") as HTMLButtonElement).disabled = checked.size === 0;
}

document.querySelectorAll<HTMLElement>("th[data-sort]").forEach((th) =>
  th.addEventListener("click", () => {
    const s = th.dataset.sort as ResultsQuery["sort"];
    descending = s === sort ? !descending : false;
    sort = s;
    void loadResults(true);
  }),
);
document.querySelectorAll(".st").forEach((c) => c.addEventListener("change", () => void loadResults(true)));
let filterTimer: number | undefined;
input("filter").addEventListener("input", () => {
  window.clearTimeout(filterTimer);
  filterTimer = window.setTimeout(() => void loadResults(true), 250);
});
$("btn-more").addEventListener("click", () => void loadResults(false));
input("check-all").addEventListener("change", () => {
  for (const f of rows) {
    if (f.state === "unrecoverable") continue;
    if (input("check-all").checked) checked.add(f.id);
    else checked.delete(f.id);
  }
  renderTable();
});

$("btn-recover").addEventListener("click", async () => {
  const dest = await guard(api.chooseRecoverFolder());
  if (!dest) return;
  const r = await guard(api.recover([...checked], dest, input("opt-keep").checked));
  if (!r) return;
  const failed = r.failed.length ? ` ${r.failed.length} failed.` : "";
  toast(`Recovered ${r.recovered} file${r.recovered === 1 ? "" : "s"} (${fmtBytes(r.bytes)}) to ${r.destination}.${failed}`, r.failed.length > 0, {
    label: "Open folder",
    run: () => void guard(api.openFolder(dest)),
  });
});

// ---------- events ----------

api.onEvent((ev: WorkerEvent) => {
  if (ev.event === "scan.progress") {
    const pct = ev.total > 0 ? Math.min(100, (ev.done / ev.total) * 100) : 0;
    $("bar").style.width = `${pct.toFixed(1)}%`;
    const phase = ev.phase === "deep" ? "Deep scan (searching free space)" : "Reading file records";
    $("scan-msg").textContent = `${ev.drive} — ${phase} ${pct ? `${pct.toFixed(0)}%` : ""}${ev.message ? ` · ${ev.message}` : ""}`;
  } else if (ev.event === "scan.finished") {
    if (ev.error) {
      toast(ev.error, true);
      show("wizard");
      return;
    }
    checked.clear();
    current = null;
    $("preview").replaceChildren(h("span", { class: "muted", text: "Select a file to see a preview" }));
    $("info").replaceChildren();
    show("results");
    void loadResults(true);
    if (ev.cancelled) toast("Scan cancelled — showing what was found so far.");
  } else if (ev.event === "worker.exit") {
    toast(ev.message, true);
  }
});

void loadDrives();
updateSelected();

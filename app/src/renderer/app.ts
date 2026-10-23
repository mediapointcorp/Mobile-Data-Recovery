// Renderer: plain DOM + TypeScript. All untrusted strings (file names,
// EXIF values, paths) are inserted with textContent, never as HTML.

import type {
  Classification,
  DriveInfo,
  JobInfo,
  ListedDevice,
  PhotoRecoveryApi,
  RecoveryResult,
  StartJob,
  WorkerEvent,
} from "../shared/types.js";
import { CLASSIFICATIONS, CLASSIFICATION_LABELS } from "../shared/types.js";

declare global {
  interface Window {
    photoRecovery: PhotoRecoveryApi;
  }
}

const api = window.photoRecovery;
const PAGE = 500;
/** Formats Chromium can display (serialised `ImageFormat` names). */
const PREVIEWABLE = ["jpeg", "png", "webp", "gif", "bmp", "avif"];

function $(id: string): HTMLElement {
  const e = document.getElementById(id);
  if (!e) throw new Error(`missing #${id}`);
  return e;
}

function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: { class?: string; text?: string; title?: string } = {},
  children: (Node | string)[] = [],
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  if (props.class) el.className = props.class;
  if (props.text !== undefined) el.textContent = props.text;
  if (props.title) el.title = props.title;
  for (const c of children) el.append(c);
  return el;
}

let toastTimer: number | undefined;
function toast(msg: string, error = false): void {
  const t = $("toast");
  t.textContent = msg;
  t.className = error ? "toast error" : "toast";
  window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => t.classList.add("hidden"), error ? 9000 : 4000);
}

async function guard<T>(p: Promise<T>): Promise<T | undefined> {
  try {
    return await p;
  } catch (e) {
    toast(e instanceof Error ? e.message.replace(/^Error invoking remote method '[^']+': (Error: )?/, "") : String(e), true);
    return undefined;
  }
}

function fmtBytes(n: number | null): string {
  if (n === null) return "—";
  if (n < 1024) return `${n} B`;
  if (n < 1048576) return `${(n / 1024).toFixed(1)} KiB`;
  if (n < 1073741824) return `${(n / 1048576).toFixed(1)} MiB`;
  return `${(n / 1073741824).toFixed(2)} GiB`;
}

// ---------- navigation ----------

function show(view: string): void {
  document.querySelectorAll<HTMLElement>(".view").forEach((v) => v.classList.toggle("hidden", v.id !== `view-${view}`));
  document.querySelectorAll<HTMLElement>(".nav").forEach((b) => b.classList.toggle("active", b.dataset.view === view));
  if (view === "jobs") void refreshJobs();
  if (view === "pc") void refreshDrives();
  if (view === "results" && results.length === 0) void loadResults(true);
}

document.querySelectorAll<HTMLButtonElement>(".nav").forEach((b) => b.addEventListener("click", () => show(b.dataset.view ?? "device")));

// ---------- case ----------

let caseOpen = false;
$("btn-case").addEventListener("click", async () => {
  const r = await guard(api.chooseCase());
  if (!r) return;
  caseOpen = true;
  $("case-label").textContent = `Case: ${r.root}`;
  results = [];
  void refreshJobs();
  void refreshDrives();
  toast("Case opened");
});

function needCase(): boolean {
  if (!caseOpen) toast("Open or create a case folder first (it must not be inside the evidence).", true);
  return caseOpen;
}

// ---------- this PC ----------

async function refreshDrives(): Promise<void> {
  const r = await guard(api.drives());
  const tbody = $("drive-table").querySelector("tbody")!;
  tbody.replaceChildren();
  const banner = $("admin-banner");
  if (!r) return;
  banner.replaceChildren();
  banner.classList.toggle("hidden", r.elevated);
  if (!r.elevated) {
    const btn = h("button", { class: "primary", text: "Restart as administrator" });
    btn.addEventListener("click", () => void guard(api.relaunchAsAdmin()));
    banner.append(
      h("p", { text: "Scanning a drive needs administrator rights (Windows only lets administrators read a disk directly). PhotoRecovery only reads the drive; it never writes to it." }),
      ...(api.platform() === "win32" ? [btn] : []),
    );
  }
  if (r.drives.length === 0) {
    tbody.append(h("tr", {}, [h("td", { text: "No drives found." })]));
  }
  for (const d of r.drives) tbody.append(driveRow(d, r.elevated));
}

function driveRow(d: DriveInfo, elevated: boolean): HTMLTableRowElement {
  const kind: string[] = [];
  if (d.kind === "disk") kind.push("whole disk");
  if (d.drive_type && d.drive_type !== "disk") kind.push(d.drive_type);
  if (d.ssd === true) kind.push("SSD");
  if (d.ssd === false) kind.push("HDD");
  if (d.filesystem) kind.push(d.filesystem);
  const notes = h("td");
  for (const n of d.notes) notes.append(h("div", { class: "muted small", text: n }));
  if (d.holds_case) notes.append(h("div", { class: "chip bad", text: "Your case folder is on this drive — choose a case folder on another drive to scan it." }));
  const act = h("td");
  const scannable = d.drive_type !== "network" && d.drive_type !== "optical";
  if (scannable) {
    const b = h("button", { class: "primary", text: "Scan for deleted photos" });
    b.disabled = d.holds_case || !elevated;
    b.addEventListener("click", () => {
      if (!needCase()) return;
      void startJob({
        kind: "drive_scan",
        drive: d.id,
        all_space: (document.getElementById("opt-all-space") as HTMLInputElement).checked,
        include_existing: (document.getElementById("opt-existing") as HTMLInputElement).checked,
      });
    });
    act.append(b);
  }
  const name = [d.label, d.is_system ? "(Windows)" : ""].filter(Boolean).join(" ");
  return h("tr", {}, [
    h("td", { class: "mono", text: d.id }),
    h("td", { text: name }),
    h("td", { text: kind.join(" · ") }),
    h("td", { text: d.size !== null ? `${fmtBytes(d.size)}${d.free !== null ? ` (${fmtBytes(d.free)} free)` : ""}` : "?" }),
    notes,
    act,
  ]);
}

$("btn-refresh-drives").addEventListener("click", () => void refreshDrives());

// ---------- device ----------

async function refreshAdb(): Promise<void> {
  const s = await guard(api.adbStatus());
  const el = $("adb-status");
  el.replaceChildren();
  if (!s) return;
  if (s.found) {
    el.append(h("span", { class: "chip ok", text: "adb found" }), h("span", { class: "muted", text: ` ${s.version ?? ""}` }));
  } else {
    el.append(
      h("span", { class: "chip bad", text: "adb not found" }),
      h("p", { text: "Install Android SDK Platform-Tools and make sure adb is on PATH (see docs/USB_ADB_SETUP.md)." }),
    );
  }
}

async function refreshDevices(): Promise<void> {
  const devs = await guard(api.devices());
  const tbody = $("device-table").querySelector("tbody")!;
  tbody.replaceChildren();
  if (!devs) return;
  if (devs.length === 0) {
    tbody.append(h("tr", {}, [h("td", { text: "No devices. Connect via USB, enable USB debugging, and accept the prompt on the phone." })]));
  }
  for (const d of devs) tbody.append(deviceRow(d));
}

function deviceRow(d: ListedDevice): HTMLTableRowElement {
  const stateChip = h("span", {
    class: `chip ${d.state === "device" ? "ok" : "warn"}`,
    text: d.state === "unauthorized" ? "unauthorized — accept the USB debugging prompt" : d.state,
  });
  const actions = h("td");
  if (d.state === "device") {
    const info = h("button", { text: "Details" });
    info.addEventListener("click", () => void showDevice(d.serial));
    const acq = h("button", { class: "primary", text: "Copy accessible photos" });
    acq.addEventListener("click", () => {
      if (!needCase()) return;
      void startJob({ kind: "device_acquire", serial: d.serial });
    });
    actions.append(info, " ", acq);
  }
  return h("tr", {}, [h("td", { class: "mono", text: d.serial }), h("td", {}, [stateChip]), h("td", { text: d.model ?? "" }), actions]);
}

async function showDevice(serial: string): Promise<void> {
  const r = await guard(api.deviceInfo(serial));
  if (!r) return;
  const el = $("device-detail");
  el.classList.remove("hidden");
  el.replaceChildren();
  const info = r.info as Record<string, unknown>;
  const caps = r.capabilities as Record<string, unknown>;
  const dl = h("dl");
  const add = (k: string, v: unknown) => {
    if (v === null || v === undefined || v === "") return;
    dl.append(h("dt", { text: k }), h("dd", { text: String(v) }));
  };
  add("Brand / model", `${info.brand ?? ""} ${info.model ?? ""}`);
  add("Android", `${info.android_version ?? "?"} (SDK ${info.sdk ?? "?"})`);
  add("Security patch", info.security_patch);
  add("realme device", info.is_realme ? "yes" : "no");
  add("Vendor UI", info.vendor_ui);
  add("Encryption", `${caps.crypto_state ?? "?"} / ${caps.crypto_type ?? "?"}`);
  add("Bootloader locked", caps.bootloader_locked);
  add("Verified boot", caps.verified_boot_state);
  add("su present", caps.su_present ? "yes (not used unless you enable root mode)" : "no");
  add("Shared storage free", caps.shared_storage_free_kib ? fmtBytes(Number(caps.shared_storage_free_kib) * 1024) : null);
  add("SD cards", (caps.external_volumes as string[] | undefined)?.join(", "));
  const lim = h("ul", { class: "limits" });
  for (const l of (caps.limitations as string[]) ?? []) lim.append(h("li", { text: l }));
  el.append(h("h2", { text: `Device ${serial}` }), dl, h("h2", { text: "What this connection can and cannot do" }), lim);
  (document.getElementById("root-serial") as HTMLInputElement).value = serial;
}

$("btn-refresh-devices").addEventListener("click", () => void refreshDevices());

$("btn-root-list").addEventListener("click", async () => {
  const serial = (document.getElementById("root-serial") as HTMLInputElement).value.trim();
  const confirmation = (document.getElementById("root-confirm") as HTMLInputElement).value;
  const parts = await guard(api.listPartitions(serial, confirmation));
  if (!parts) return;
  const sel = document.getElementById("root-partition") as HTMLSelectElement;
  sel.replaceChildren(...parts.map((p) => h("option", { text: p })));
  toast(`${parts.length} partitions listed`);
});

$("btn-root-acquire").addEventListener("click", () => {
  if (!needCase()) return;
  const serial = (document.getElementById("root-serial") as HTMLInputElement).value.trim();
  const confirmation = (document.getElementById("root-confirm") as HTMLInputElement).value;
  const partition = (document.getElementById("root-partition") as HTMLSelectElement).value;
  if (!partition) return toast("List partitions first", true);
  void startJob({ kind: "partition_acquire", serial, partition, confirmation });
});

// ---------- image & folder ----------

let imagePath: string | null = null;
let folderPath: string | null = null;
let exportPath: string | null = null;

$("btn-pick-image").addEventListener("click", async () => {
  imagePath = (await guard(api.chooseImage())) ?? imagePath;
  $("image-path").textContent = imagePath ?? "";
});
$("btn-start-image").addEventListener("click", () => {
  if (!needCase()) return;
  if (!imagePath) return toast("Choose an image first", true);
  const formats = [...document.querySelectorAll<HTMLInputElement>(".opt-format")].filter((c) => c.checked).map((c) => c.value);
  void startJob({
    kind: "image_scan",
    image: imagePath,
    working_copy: (document.getElementById("opt-working-copy") as HTMLInputElement).checked,
    filesystem_analysis: (document.getElementById("opt-fs") as HTMLInputElement).checked,
    carve: (document.getElementById("opt-carve") as HTMLInputElement).checked,
    formats,
  });
});
$("btn-pick-folder").addEventListener("click", async () => {
  folderPath = (await guard(api.chooseFolder())) ?? folderPath;
  $("folder-path").textContent = folderPath ?? "";
});
$("btn-start-folder").addEventListener("click", () => {
  if (!needCase()) return;
  if (!folderPath) return toast("Choose a folder first", true);
  void startJob({ kind: "local_scan", dir: folderPath });
});

async function startJob(job: StartJob): Promise<void> {
  const r = await guard(api.startJob(job));
  if (!r) return;
  toast(`Job started: ${r.job_id}`);
  show("jobs");
}

// ---------- jobs ----------

const progress = new Map<string, { phase: string; done: number; total: number; message: string }>();

async function refreshJobs(): Promise<void> {
  if (!caseOpen) return;
  const jobs = await guard(api.listJobs());
  if (!jobs) return;
  const list = $("jobs-list");
  list.replaceChildren(...jobs.slice().reverse().map(jobCard));
  const running = jobs.filter((j) => j.running).length;
  const badge = $("jobs-badge");
  badge.textContent = String(running);
  badge.classList.toggle("hidden", running === 0);
}

function jobCard(j: JobInfo): HTMLElement {
  const statusClass = j.status === "completed" ? "ok" : j.status === "failed" ? "bad" : "warn";
  const head = h("div", { class: "head" }, [
    h("span", { class: "id mono", text: j.id }),
    h("span", { class: "chip", text: j.params.kind.replace("_", " ") }),
    h("span", { class: `chip ${statusClass}`, text: j.running ? "running" : j.status }),
    h("span", { class: "muted", text: `${j.result_count ?? 0} results` }),
  ]);
  const card = h("div", { class: "job" }, [head]);
  card.dataset.job = j.id;
  const bar = h("div", { class: "progress" }, [h("div")]);
  const msg = h("div", { class: "muted mono" });
  card.append(bar, msg);
  updateProgress(card, progress.get(j.id));
  const actions = h("div", { class: "row" });
  if (j.running) {
    const c = h("button", { text: "Cancel" });
    c.addEventListener("click", () => void guard(api.cancelJob(j.id)));
    actions.append(c);
  } else if (j.status !== "completed") {
    const r = h("button", { class: "primary", text: "Resume" });
    r.addEventListener("click", async () => {
      if (await guard(api.resumeJob(j.id))) void refreshJobs();
    });
    actions.append(r);
  }
  card.append(actions);
  if (j.error) card.append(h("div", { class: "chip bad", text: j.error }));
  if (j.limitations.length) {
    const ul = h("ul", { class: "limits" });
    for (const l of j.limitations.slice(0, 30)) ul.append(h("li", { text: l }));
    card.append(h("div", { class: "muted", text: "Limitations:" }), ul);
  }
  return card;
}

function updateProgress(card: HTMLElement, p?: { phase: string; done: number; total: number; message: string }): void {
  const bar = card.querySelector<HTMLElement>(".progress > div");
  const msg = card.querySelector<HTMLElement>(".muted.mono");
  if (!bar || !msg) return;
  if (!p) {
    bar.parentElement!.classList.add("hidden");
    return;
  }
  bar.parentElement!.classList.remove("hidden");
  bar.style.width = p.total > 0 ? `${Math.min(100, (p.done / p.total) * 100).toFixed(1)}%` : "100%";
  msg.textContent = `${p.phase}: ${p.total > 0 ? `${((p.done / p.total) * 100).toFixed(1)}% · ` : ""}${p.message}`;
}

$("btn-refresh-jobs").addEventListener("click", () => void refreshJobs());

api.onEvent((ev: WorkerEvent) => {
  if (ev.event === "job.progress") {
    progress.set(ev.job_id, ev);
    const card = document.querySelector<HTMLElement>(`.job[data-job="${CSS.escape(ev.job_id)}"]`);
    if (card) updateProgress(card, ev);
    else void refreshJobs();
  } else if (ev.event === "job.finished") {
    progress.delete(ev.job_id);
    toast(`Job ${ev.status}${ev.error ? `: ${ev.error}` : ""}`, ev.status === "failed");
    void refreshJobs();
    results = [];
  } else if (ev.event === "worker.exit") {
    toast(ev.message, true);
  }
});

// ---------- results ----------

let results: RecoveryResult[] = [];
let total = 0;

function fillSelect(id: string, values: [string, string][]): void {
  const sel = document.getElementById(id) as HTMLSelectElement;
  for (const [value, label] of values) {
    const o = h("option", { text: label });
    o.value = value;
    sel.append(o);
  }
}
fillSelect("f-class", CLASSIFICATIONS.map((c) => [c, CLASSIFICATION_LABELS[c]]));
fillSelect("f-format", ["jpeg", "png", "webp", "heic", "avif", "gif", "bmp", "dng"].map((f) => [f, f.toUpperCase()]));
fillSelect(
  "f-valid",
  ["valid", "valid_with_warnings", "truncated", "corrupt", "structure_only", "decode_failed", "not_validated"].map((v) => [v, v.replace(/_/g, " ")]),
);

async function loadResults(reset: boolean): Promise<void> {
  if (!caseOpen) return;
  if (reset) {
    results = [];
    await guard(api.finalize());
  }
  const cls = ((document.getElementById("f-class") as HTMLSelectElement).value || null) as Classification | null;
  const page = await guard(api.listResults(results.length, PAGE, cls));
  if (!page) return;
  total = page.total;
  results = results.concat(page.items);
  renderResults();
}

function filtered(): RecoveryResult[] {
  const fmt = (document.getElementById("f-format") as HTMLSelectElement).value;
  const val = (document.getElementById("f-valid") as HTMLSelectElement).value;
  const hideDup = (document.getElementById("f-hide-dup") as HTMLInputElement).checked;
  const q = (document.getElementById("f-search") as HTMLInputElement).value.trim().toLowerCase();
  const sort = (document.getElementById("f-sort") as HTMLSelectElement).value;
  const out = results.filter(
    (r) =>
      (!fmt || r.format === fmt) &&
      (!val || r.validation === val) &&
      (!hideDup || !r.associations.some((a) => a.relation === "duplicate_of")) &&
      (!q || [r.id, r.source_path ?? "", r.sha256 ?? "", r.output_path ?? ""].some((s) => s.toLowerCase().includes(q))),
  );
  const key: Record<string, (r: RecoveryResult) => number | string> = {
    class: (r) => CLASSIFICATIONS.indexOf(r.classification),
    size: (r) => -(r.candidate_length ?? 0),
    pixels: (r) => -((r.width ?? 0) * (r.height ?? 0)),
    offset: (r) => r.source_offset ?? Number.MAX_SAFE_INTEGER,
    path: (r) => r.source_path ?? "",
  };
  const k = key[sort] ?? key.class;
  return out.sort((a, b) => {
    const x = k(a);
    const y = k(b);
    return x < y ? -1 : x > y ? 1 : 0;
  });
}

function validationChip(r: RecoveryResult): HTMLElement {
  const cls = r.validation === "valid" ? "ok" : r.validation === "valid_with_warnings" || r.validation === "structure_only" ? "warn" : "bad";
  return h("span", { class: `chip ${cls}`, text: r.validation.replace(/_/g, " ") });
}

function renderResults(): void {
  const list = filtered();
  $("results-summary").textContent = `${results.length} of ${total} loaded · ${list.length} shown`;
  const grid = $("results-grid");
  grid.replaceChildren(...list.slice(0, 3000).map(tile));
  $("btn-more").classList.toggle("hidden", results.length >= total);
}

function tile(r: RecoveryResult): HTMLElement {
  const thumb = h("div", { class: "thumb" });
  if (r.output_path && PREVIEWABLE.includes(r.format)) {
    const img = h("img");
    img.loading = "lazy";
    img.alt = "";
    img.src = api.previewUrl(r.output_path);
    img.addEventListener("error", () => thumb.replaceChildren(h("span", { text: "no preview" })));
    thumb.append(img);
  } else {
    thumb.append(h("span", { text: r.output_path ? `${r.format.toUpperCase()} (no preview)` : "no data" }));
  }
  const name = r.source_path?.split("/").pop() ?? (r.source_offset !== null ? `@${r.source_offset}` : r.id.slice(0, 8));
  const meta = h("div", { class: "meta" }, [
    h("div", { class: "mono", text: name }),
    h("span", { class: "chip", text: CLASSIFICATION_LABELS[r.classification] }),
    validationChip(r),
    ...(r.metadata.recycle_bin_path ? [h("span", { class: "chip warn", text: "Recycle Bin" })] : []),
    h("div", { class: "muted", text: `${r.format.toUpperCase()} · ${r.width && r.height ? `${r.width}×${r.height}` : "?"} · ${fmtBytes(r.candidate_length)}` }),
  ]);
  const t = h("div", { class: "tile" }, [thumb, meta]);
  t.tabIndex = 0;
  t.addEventListener("click", () => showDetail(r));
  t.addEventListener("keydown", (e) => {
    if (e.key === "Enter") showDetail(r);
  });
  return t;
}

function showDetail(r: RecoveryResult): void {
  const d = $("detail");
  d.classList.remove("hidden");
  d.replaceChildren();
  const close = h("button", { text: "Close" });
  close.addEventListener("click", () => d.classList.add("hidden"));
  d.append(close);
  if (r.output_path && PREVIEWABLE.includes(r.format)) {
    const img = h("img");
    img.alt = "";
    img.src = api.previewUrl(r.output_path);
    d.append(img);
  }
  d.append(h("h2", { text: CLASSIFICATION_LABELS[r.classification] }));
  const dl = h("dl");
  const add = (k: string, v: unknown) => {
    if (v === null || v === undefined || v === "") return;
    dl.append(h("dt", { text: k }), h("dd", { text: String(v) }));
  };
  add("ID", r.id);
  add("Source", r.source_id);
  add("Method", r.method.replace(/_/g, " "));
  add("Format", r.format);
  add("Source path", r.source_path);
  add("Source offset", r.source_offset);
  add("Length", r.candidate_length !== null ? `${r.candidate_length} (${fmtBytes(r.candidate_length)})` : null);
  add("Dimensions", r.width && r.height ? `${r.width} × ${r.height}` : null);
  add("Validation", r.validation);
  add("SHA-256", r.sha256);
  add("dHash", r.perceptual_hash);
  add("Output", r.output_path);
  for (const [k, v] of Object.entries(r.metadata)) add(k, v);
  d.append(dl);
  const section = (title: string, items: string[]) => {
    if (!items.length) return;
    const ul = h("ul", { class: "limits" });
    for (const i of items) ul.append(h("li", { text: i }));
    d.append(h("h2", { text: title }), ul);
  };
  section("Warnings", r.warnings);
  section("Limitations", r.limitations);
  section("Diagnostics", r.diagnostics);
  if (r.associations.length) {
    const ul = h("ul", { class: "limits" });
    for (const a of r.associations) {
      const target = results.find((x) => x.id === a.result_id);
      const li = h("li", { text: `${a.relation.replace(/_/g, " ")} ${a.result_id.slice(0, 8)} — ${a.basis}` });
      if (target) {
        li.style.cursor = "pointer";
        li.addEventListener("click", () => showDetail(target));
      }
      ul.append(li);
    }
    d.append(h("h2", { text: "Associations" }), ul);
  }
}

$("btn-finalize").addEventListener("click", () => void loadResults(true));
$("btn-more").addEventListener("click", () => void loadResults(false));
$("f-class").addEventListener("change", () => void loadResults(true));
for (const id of ["f-format", "f-valid", "f-sort", "f-hide-dup"]) $(id).addEventListener("change", renderResults);
$("f-search").addEventListener("input", renderResults);

// ---------- export & report ----------

const exportClasses = $("export-classes");
for (const c of CLASSIFICATIONS) {
  if (c === "unavailable_source_data") continue;
  const cb = h("input");
  cb.type = "checkbox";
  cb.value = c;
  cb.checked = c !== "existing_duplicate";
  exportClasses.append(h("label", {}, [cb, ` ${CLASSIFICATION_LABELS[c]}`]));
}

$("btn-pick-export").addEventListener("click", async () => {
  exportPath = (await guard(api.chooseExportFolder())) ?? exportPath;
  $("export-path").textContent = exportPath ?? "";
});

$("btn-export").addEventListener("click", async () => {
  if (!needCase()) return;
  if (!exportPath) return toast("Choose an export destination", true);
  const classes = [...exportClasses.querySelectorAll<HTMLInputElement>("input:checked")].map((c) => c.value as Classification);
  await guard(api.finalize());
  const r = await guard(api.exportResults(exportPath, classes, (document.getElementById("export-dups") as HTMLInputElement).checked));
  if (!r) return;
  $("export-result").textContent = `Exported ${r.exported} files (${r.verification_failures} verification failures, ${r.skipped_no_file} without data) to ${r.destination}`;
});

$("btn-report").addEventListener("click", async () => {
  if (!needCase()) return;
  const r = await guard(api.generateReport());
  if (r) $("report-result").textContent = `Written: ${r.html} and ${r.json}`;
});
$("btn-open-report").addEventListener("click", () => void guard(api.openReport()));

// ---------- init ----------

void refreshDrives();
void refreshAdb();
void refreshDevices();

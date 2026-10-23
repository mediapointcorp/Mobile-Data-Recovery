// Preload: the only bridge between the sandboxed renderer and the main
// process. Exposes a fixed, typed API — no generic IPC or Node access.

import { contextBridge, ipcRenderer } from "electron";
import type { Classification, PhotoRecoveryApi, StartJob, WorkerEvent } from "../shared/types";

const api: PhotoRecoveryApi = {
  chooseCase: () => ipcRenderer.invoke("case:choose"),
  chooseImage: () => ipcRenderer.invoke("image:choose"),
  chooseFolder: () => ipcRenderer.invoke("folder:choose"),
  chooseExportFolder: () => ipcRenderer.invoke("export:choose"),
  adbStatus: () => ipcRenderer.invoke("adb:status"),
  devices: () => ipcRenderer.invoke("adb:devices"),
  deviceInfo: (serial: string) => ipcRenderer.invoke("adb:info", serial),
  listPartitions: (serial: string, confirmation: string) => ipcRenderer.invoke("adb:partitions", serial, confirmation),
  startJob: (job: StartJob) => ipcRenderer.invoke("job:start", job),
  resumeJob: (id: string) => ipcRenderer.invoke("job:resume", id),
  cancelJob: (id: string) => ipcRenderer.invoke("job:cancel", id),
  listJobs: () => ipcRenderer.invoke("job:list"),
  finalize: () => ipcRenderer.invoke("results:finalize"),
  listResults: (offset: number, limit: number, c: Classification | null) => ipcRenderer.invoke("results:list", offset, limit, c),
  exportResults: (dest: string, classes: Classification[], dup: boolean) => ipcRenderer.invoke("export:run", dest, classes, dup),
  generateReport: () => ipcRenderer.invoke("report:generate"),
  openReport: () => ipcRenderer.invoke("report:open"),
  previewUrl: (outputPath: string) => `prmedia://case/${outputPath.split("/").map(encodeURIComponent).join("/")}`,
  onEvent: (cb: (ev: WorkerEvent) => void) => {
    ipcRenderer.on("worker:event", (_e, ev: WorkerEvent) => cb(ev));
  },
};

contextBridge.exposeInMainWorld("photoRecovery", api);

// Preload: the only bridge between the sandboxed renderer and the main
// process. Exposes a fixed, typed API — no generic IPC or Node access.

import { contextBridge, ipcRenderer } from "electron";
import type { PhotoRecoveryApi, ResultsQuery, ScanRequest, WorkerEvent } from "../shared/types";

const api: PhotoRecoveryApi = {
  drives: () => ipcRenderer.invoke("drives:list"),
  chooseFolderToSearch: () => ipcRenderer.invoke("folder:search"),
  chooseRecoverFolder: () => ipcRenderer.invoke("folder:recover"),
  startScan: (req: ScanRequest) => ipcRenderer.invoke("scan:start", req),
  cancelScan: () => ipcRenderer.invoke("scan:cancel"),
  results: (q: ResultsQuery) => ipcRenderer.invoke("scan:results", q),
  preview: (id: number) => ipcRenderer.invoke("scan:preview", id),
  recover: (ids: number[], dest: string, keep: boolean) => ipcRenderer.invoke("scan:recover", ids, dest, keep),
  openFolder: (path: string) => ipcRenderer.invoke("shell:openFolder", path),
  relaunchAsAdmin: () => ipcRenderer.invoke("app:relaunchAdmin"),
  platform: () => process.platform,
  onEvent: (cb: (ev: WorkerEvent) => void) => {
    ipcRenderer.on("worker:event", (_e, ev: WorkerEvent) => cb(ev));
  },
};

contextBridge.exposeInMainWorld("photoRecovery", api);

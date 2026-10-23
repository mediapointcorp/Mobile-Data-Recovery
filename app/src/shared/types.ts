// Types mirrored from the Rust worker (crates/worker/src/recover.rs, drives.rs).

export type Category = "all" | "pictures" | "music" | "documents" | "video" | "compressed" | "emails";

export const CATEGORIES: { id: Category; label: string; hint: string }[] = [
  { id: "all", label: "All files", hint: "Show every deleted file" },
  { id: "pictures", label: "Pictures", hint: "Photos and images (JPG, PNG, HEIC, RAW…)" },
  { id: "music", label: "Music", hint: "MP3, WAV, FLAC, M4A…" },
  { id: "documents", label: "Documents", hint: "Word, Excel, PowerPoint, PDF, text…" },
  { id: "video", label: "Video", hint: "MP4, MOV, AVI, MKV…" },
  { id: "compressed", label: "Compressed", hint: "ZIP, RAR, 7z…" },
  { id: "emails", label: "Emails", hint: "Outlook PST/MSG, EML…" },
];

export type State = "excellent" | "poor" | "very_poor" | "unrecoverable";

export const STATE_LABELS: Record<State, string> = {
  excellent: "Excellent",
  poor: "Poor",
  very_poor: "Very poor",
  unrecoverable: "Unrecoverable",
};

export interface DriveInfo {
  id: string;
  device_path: string;
  kind: "volume" | "disk";
  label: string | null;
  filesystem: string | null;
  size: number | null;
  free: number | null;
  drive_type: string;
  ssd: boolean | null;
  trim: boolean | null;
  is_system: boolean;
  notes: string[];
}

export interface FoundFile {
  id: number;
  drive: string;
  name: string;
  path: string;
  size: number;
  modified: string | null;
  created: string | null;
  state: State;
  comment: string;
  category: Category | null;
  origin: "deleted" | "recycle_bin" | "deep_scan";
  filesystem: string;
  width: number | null;
  height: number | null;
}

export interface ScanSummary {
  found: number;
  by_state: Record<string, number>;
  notes: string[];
  seconds: number;
}

export interface ScanRequest {
  drives: string[];
  category: Category;
  deep_scan: boolean;
  folder: string | null;
  recycle_bin_only: boolean;
}

export interface ResultsQuery {
  offset: number;
  limit: number;
  sort: "name" | "path" | "size" | "modified" | "state";
  descending: boolean;
  text: string | null;
  states: State[] | null;
}

export interface RecoverSummary {
  recovered: number;
  failed: string[];
  bytes: number;
  destination: string;
}

export type WorkerEvent =
  | { event: "ready"; tool: string; version: string; protocol: number }
  | { event: "scan.progress"; drive: string; phase: string; done: number; total: number; message: string }
  | { event: "scan.finished"; cancelled?: boolean; summary?: ScanSummary; error?: string }
  | { event: "recover.progress"; done: number; total: number }
  | { event: "worker.exit"; code: number | null; message: string };

/** API exposed by the preload script on `window.photoRecovery`. */
export interface PhotoRecoveryApi {
  drives(): Promise<{ elevated: boolean; drives: DriveInfo[] }>;
  chooseFolderToSearch(): Promise<string | null>;
  chooseRecoverFolder(): Promise<string | null>;
  startScan(req: ScanRequest): Promise<boolean>;
  cancelScan(): Promise<boolean>;
  results(q: ResultsQuery): Promise<{ total: number; all: number; items: FoundFile[]; summary: ScanSummary | null }>;
  preview(id: number): Promise<string>;
  recover(ids: number[], dest: string, keepFolders: boolean): Promise<RecoverSummary>;
  openFolder(path: string): Promise<void>;
  relaunchAsAdmin(): Promise<void>;
  platform(): string;
  onEvent(cb: (ev: WorkerEvent) => void): void;
}

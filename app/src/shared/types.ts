// Types mirrored from the Rust worker (crates/model, crates/worker).

export type Classification =
  | "existing_original"
  | "existing_duplicate"
  | "deleted_file_record"
  | "carved_image"
  | "thumbnail"
  | "partial_image"
  | "unverified_candidate"
  | "unavailable_source_data";

export const CLASSIFICATIONS: readonly Classification[] = [
  "existing_original",
  "existing_duplicate",
  "deleted_file_record",
  "carved_image",
  "thumbnail",
  "partial_image",
  "unverified_candidate",
  "unavailable_source_data",
];

export const CLASSIFICATION_LABELS: Record<Classification, string> = {
  existing_original: "Existing original",
  existing_duplicate: "Existing duplicate",
  deleted_file_record: "Deleted-file record",
  carved_image: "Carved image",
  thumbnail: "Thumbnail",
  partial_image: "Partial image",
  unverified_candidate: "Unverified candidate",
  unavailable_source_data: "Unavailable source data",
};

export type ValidationStatus =
  | "valid"
  | "valid_with_warnings"
  | "truncated"
  | "corrupt"
  | "structure_only"
  | "decode_failed"
  | "not_validated";

export interface Association {
  result_id: string;
  relation: "thumbnail_of" | "has_thumbnail" | "duplicate_of" | "similar_to" | "embedded_in";
  basis: string;
}

export interface RecoveryResult {
  id: string;
  source_id: string;
  method: string;
  format: string;
  source_offset: number | null;
  source_path: string | null;
  candidate_length: number | null;
  width: number | null;
  height: number | null;
  sha256: string | null;
  perceptual_hash: string | null;
  validation: ValidationStatus;
  classification: Classification;
  output_path: string | null;
  warnings: string[];
  limitations: string[];
  diagnostics: string[];
  associations: Association[];
  metadata: Record<string, string>;
}

export interface ListedDevice {
  serial: string;
  state: string;
  model: string | null;
  product: string | null;
  transport_id: string | null;
}

export type JobStatus = "created" | "running" | "cancelled" | "completed" | "failed";

export interface JobInfo {
  id: string;
  status: JobStatus;
  created_at: string;
  updated_at: string;
  runs: number;
  error: string | null;
  limitations: string[];
  params: { kind: string; [k: string]: unknown };
  running?: boolean;
  result_count?: number;
}

export type WorkerEvent =
  | { event: "ready"; tool: string; version: string; protocol: number }
  | { event: "job.progress"; job_id: string; phase: string; done: number; total: number; message: string }
  | { event: "job.finished"; job_id: string; status: JobStatus; error: string | null }
  | { event: "worker.exit"; code: number | null; message: string };

/** Job parameters the UI may submit. Paths are tokens returned by dialogs. */
export type StartJob =
  | { kind: "image_scan"; image: string; working_copy: boolean; formats: string[]; filesystem_analysis: boolean; carve: boolean }
  | { kind: "local_scan"; dir: string }
  | { kind: "device_acquire"; serial: string }
  | { kind: "partition_acquire"; serial: string; partition: string; confirmation: string };

/** API exposed by the preload script on `window.photoRecovery`. */
export interface PhotoRecoveryApi {
  chooseCase(): Promise<{ root: string } | null>;
  chooseImage(): Promise<string | null>;
  chooseFolder(): Promise<string | null>;
  chooseExportFolder(): Promise<string | null>;
  adbStatus(): Promise<{ found: boolean; version?: string | null; message?: string }>;
  devices(): Promise<ListedDevice[]>;
  deviceInfo(serial: string): Promise<{ info: Record<string, unknown>; capabilities: Record<string, unknown> }>;
  listPartitions(serial: string, confirmation: string): Promise<string[]>;
  startJob(job: StartJob): Promise<{ job_id: string }>;
  resumeJob(jobId: string): Promise<{ job_id: string }>;
  cancelJob(jobId: string): Promise<boolean>;
  listJobs(): Promise<JobInfo[]>;
  finalize(): Promise<{ summary: Record<string, unknown>; exact_duplicate_groups: number; similar_pairs: number; thumbnail_links: number }>;
  listResults(offset: number, limit: number, classification: Classification | null): Promise<{ total: number; items: RecoveryResult[] }>;
  exportResults(dest: string, classifications: Classification[], includeDuplicates: boolean): Promise<Record<string, unknown>>;
  generateReport(): Promise<{ json: string; html: string }>;
  openReport(): Promise<void>;
  previewUrl(outputPath: string): string;
  onEvent(cb: (ev: WorkerEvent) => void): void;
}

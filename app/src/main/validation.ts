// Validation of every request from the renderer before it reaches the
// worker. The renderer is treated as untrusted: it can only scan drives the
// worker listed, and only write to folders the user picked in a dialog.

import type { Category, ResultsQuery, ScanRequest, State } from "../shared/types";

export class ValidationError extends Error {}

const CATEGORIES: Category[] = ["all", "pictures", "music", "documents", "video", "compressed", "emails"];
const STATES: State[] = ["excellent", "poor", "very_poor", "unrecoverable"];
const SORTS = ["name", "path", "size", "modified", "state"];

/** Values the user picked in native dialogs (or drives the worker listed). */
export class Grants {
  private granted = new Map<string, Set<string>>();

  grant(purpose: "drive" | "recover", value: string): void {
    if (!this.granted.has(purpose)) this.granted.set(purpose, new Set());
    this.granted.get(purpose)!.add(value);
  }

  require(purpose: "drive" | "recover", value: unknown): string {
    if (typeof value !== "string" || !this.granted.get(purpose)?.has(value)) {
      throw new ValidationError(purpose === "drive" ? "unknown drive" : "choose the folder with the Browse button");
    }
    return value;
  }
}

export function assertInt(n: unknown, min: number, max: number): number {
  if (typeof n !== "number" || !Number.isInteger(n) || n < min || n > max) throw new ValidationError("invalid number");
  return n;
}

export function toScanRequest(r: unknown, grants: Grants): ScanRequest {
  if (typeof r !== "object" || r === null) throw new ValidationError("invalid scan request");
  const q = r as ScanRequest;
  if (!Array.isArray(q.drives) || q.drives.length === 0 || q.drives.length > 26) throw new ValidationError("choose a drive");
  if (!CATEGORIES.includes(q.category)) throw new ValidationError("invalid file type");
  if (typeof q.deep_scan !== "boolean" || typeof q.recycle_bin_only !== "boolean") throw new ValidationError("invalid options");
  let folder: string | null = null;
  if (q.folder !== null && q.folder !== undefined) {
    if (typeof q.folder !== "string" || q.folder.length > 1024 || !/^[A-Za-z]:\\/.test(q.folder) || /[\u0000-\u001f]/.test(q.folder)) {
      throw new ValidationError("invalid folder");
    }
    folder = q.folder;
  }
  return {
    drives: q.drives.map((d) => grants.require("drive", d)),
    category: q.category,
    deep_scan: q.deep_scan,
    folder,
    recycle_bin_only: q.recycle_bin_only,
  };
}

export function toResultsQuery(r: unknown): ResultsQuery {
  if (typeof r !== "object" || r === null) throw new ValidationError("invalid query");
  const q = r as ResultsQuery;
  if (!SORTS.includes(q.sort)) throw new ValidationError("invalid sort");
  if (q.states !== null && (!Array.isArray(q.states) || !q.states.every((s) => STATES.includes(s)))) {
    throw new ValidationError("invalid states");
  }
  if (q.text !== null && (typeof q.text !== "string" || q.text.length > 500)) throw new ValidationError("invalid text");
  return {
    offset: assertInt(q.offset, 0, 1e9),
    limit: assertInt(q.limit, 1, 5000),
    sort: q.sort,
    descending: q.descending === true,
    text: q.text,
    states: q.states,
  };
}

// Validation of every request coming from the renderer before it reaches
// the worker. The renderer is treated as untrusted: it can only refer to
// filesystem paths the user picked in a native dialog during this session.

import type { Classification, StartJob } from "../shared/types";
import { CLASSIFICATIONS } from "../shared/types";

export class ValidationError extends Error {}

const SERIAL = /^[A-Za-z0-9._:\[\]-]{1,128}$/;
const PARTITION = /^[A-Za-z0-9_-]{1,64}$/;
const JOB_ID = /^[A-Za-z0-9-]{1,96}$/;
const FORMATS = new Set(["jpeg", "png", "webp", "heic"]);

export function assertSerial(s: unknown): string {
  if (typeof s !== "string" || !SERIAL.test(s)) throw new ValidationError("invalid device serial");
  return s;
}

export function assertJobId(s: unknown): string {
  if (typeof s !== "string" || !JOB_ID.test(s)) throw new ValidationError("invalid job id");
  return s;
}

export function assertClassification(c: unknown): Classification {
  if (typeof c !== "string" || !(CLASSIFICATIONS as readonly string[]).includes(c)) {
    throw new ValidationError("invalid classification");
  }
  return c as Classification;
}

export function assertInt(n: unknown, min: number, max: number): number {
  if (typeof n !== "number" || !Number.isInteger(n) || n < min || n > max) throw new ValidationError("invalid number");
  return n;
}

/** Paths the user selected through native dialogs, by purpose. */
export class GrantedPaths {
  private granted = new Map<string, Set<string>>();

  grant(purpose: "image" | "folder" | "export" | "case", path: string): void {
    if (!this.granted.has(purpose)) this.granted.set(purpose, new Set());
    this.granted.get(purpose)!.add(path);
  }

  require(purpose: "image" | "folder" | "export" | "case", path: unknown): string {
    if (typeof path !== "string" || !this.granted.get(purpose)?.has(path)) {
      throw new ValidationError(`path was not selected by the user for ${purpose}`);
    }
    return path;
  }
}

export const CONFIRMATION_PREFIX = "READ-ONLY ROOT ACQUISITION ";

/** Translate a renderer job request into worker `job.start` params. */
export function toWorkerJob(job: unknown, paths: GrantedPaths): Record<string, unknown> {
  if (typeof job !== "object" || job === null) throw new ValidationError("invalid job");
  const j = job as StartJob;
  switch (j.kind) {
    case "image_scan": {
      if (!Array.isArray(j.formats) || j.formats.length === 0 || !j.formats.every((f) => FORMATS.has(f))) {
        throw new ValidationError("invalid formats");
      }
      if (typeof j.working_copy !== "boolean" || typeof j.filesystem_analysis !== "boolean" || typeof j.carve !== "boolean") {
        throw new ValidationError("invalid options");
      }
      if (!j.filesystem_analysis && !j.carve) throw new ValidationError("select filesystem analysis, carving or both");
      return {
        kind: "image_scan",
        image_path: paths.require("image", j.image),
        evidence_mode: j.working_copy ? "working_copy" : "read_only_original",
        formats: j.formats.join(","),
        filesystem_analysis: j.filesystem_analysis,
        carve: j.carve,
      };
    }
    case "local_scan":
      return { kind: "local_scan", dir: paths.require("folder", j.dir) };
    case "device_acquire":
      return { kind: "device_acquire", serial: assertSerial(j.serial) };
    case "partition_acquire": {
      const serial = assertSerial(j.serial);
      if (typeof j.partition !== "string" || !PARTITION.test(j.partition)) throw new ValidationError("invalid partition");
      if (j.confirmation !== CONFIRMATION_PREFIX + serial) {
        throw new ValidationError(`type the confirmation exactly: ${CONFIRMATION_PREFIX}${serial}`);
      }
      return { kind: "partition_acquire", serial, partition: j.partition, confirmation: j.confirmation };
    }
    default:
      throw new ValidationError("unknown job kind");
  }
}

/**
 * Resolve a case-relative result path for the preview protocol. Rejects
 * absolute paths, drive letters, traversal and odd characters; the caller
 * must still verify the real path is inside the case root.
 */
export function safeRelative(rel: string): string | null {
  if (rel.length === 0 || rel.length > 1024) return null;
  if (rel.startsWith("/") || rel.startsWith("\\") || /^[A-Za-z]:/.test(rel)) return null;
  const parts = rel.split(/[\\/]/);
  if (parts.some((p) => p === ".." || p === "" || p === ".")) return null;
  if (!(parts[0] === "recovered" || parts[0] === "acquired")) return null;
  if (/[\u0000-\u001f<>|"?*]/.test(rel)) return null;
  return parts.join("/");
}

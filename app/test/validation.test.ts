import * as assert from "node:assert/strict";
import { test } from "node:test";
import { GrantedPaths, safeRelative, toWorkerJob, ValidationError } from "../src/main/validation";

test("image scans require a dialog-granted path", () => {
  const g = new GrantedPaths();
  const job = { kind: "image_scan", image: "C:\\evidence\\sd.img", working_copy: true, formats: ["jpeg"], filesystem_analysis: true, carve: true };
  assert.throws(() => toWorkerJob(job, g), ValidationError);
  g.grant("image", "C:\\evidence\\sd.img");
  const w = toWorkerJob(job, g);
  assert.equal(w.image_path, "C:\\evidence\\sd.img");
  assert.equal(w.evidence_mode, "working_copy");
  assert.equal(w.formats, "jpeg");
  // A folder grant does not authorise an image path.
  const g2 = new GrantedPaths();
  g2.grant("folder", "C:\\evidence\\sd.img");
  assert.throws(() => toWorkerJob(job, g2), ValidationError);
});

test("rejects bad formats, serials and unknown kinds", () => {
  const g = new GrantedPaths();
  g.grant("image", "/x.img");
  const base = { kind: "image_scan", image: "/x.img", working_copy: false, filesystem_analysis: true, carve: true };
  assert.throws(() => toWorkerJob({ ...base, formats: ["exe"] }, g), ValidationError);
  assert.throws(() => toWorkerJob({ ...base, formats: [] }, g), ValidationError);
  assert.throws(() => toWorkerJob({ kind: "device_acquire", serial: "abc; reboot" }, g), ValidationError);
  assert.throws(() => toWorkerJob({ kind: "shell", cmd: "rm" }, g), ValidationError);
  assert.throws(() => toWorkerJob(null, g), ValidationError);
  assert.deepEqual(toWorkerJob({ kind: "device_acquire", serial: "R5CT12" }, g), { kind: "device_acquire", serial: "R5CT12" });
});

test("partition acquisition needs the exact confirmation phrase", () => {
  const g = new GrantedPaths();
  const j = { kind: "partition_acquire", serial: "ABC", partition: "userdata", confirmation: "yes" };
  assert.throws(() => toWorkerJob(j, g), /READ-ONLY ROOT ACQUISITION ABC/);
  assert.throws(() => toWorkerJob({ ...j, partition: "../boot", confirmation: "READ-ONLY ROOT ACQUISITION ABC" }, g), ValidationError);
  assert.equal(toWorkerJob({ ...j, confirmation: "READ-ONLY ROOT ACQUISITION ABC" }, g).partition, "userdata");
});

test("preview paths cannot escape the case", () => {
  assert.equal(safeRelative("recovered/carved/a.jpg"), "recovered/carved/a.jpg");
  assert.equal(safeRelative("acquired\\R5\\sdcard\\x.jpg"), "acquired/R5/sdcard/x.jpg");
  for (const bad of ["../x", "recovered/../../etc/passwd", "/etc/passwd", "C:\\Windows\\x", "evidence/disk.img", "jobs/x/job.json", "recovered//x", "recovered/a\u0000b"]) {
    assert.equal(safeRelative(bad), null, bad);
  }
});

test("drive scans only accept drives the worker listed", () => {
  const g = new GrantedPaths();
  const job = { kind: "drive_scan", drive: "D:", all_space: false, include_existing: false };
  assert.throws(() => toWorkerJob(job, g), ValidationError);
  g.grant("drive", "D:");
  assert.deepEqual(toWorkerJob(job, g), { kind: "drive_scan", drive: "D:", carve_scope: "unallocated", include_existing: false });
  assert.equal(toWorkerJob({ ...job, all_space: true }, g).carve_scope, "all");
  assert.throws(() => toWorkerJob({ ...job, drive: "\\\\.\\C:" }, g), ValidationError);
});

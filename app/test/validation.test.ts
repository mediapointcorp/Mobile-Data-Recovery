import * as assert from "node:assert/strict";
import { test } from "node:test";
import { Grants, toResultsQuery, toScanRequest, ValidationError } from "../src/main/validation";

const base = { drives: ["D:"], category: "pictures", deep_scan: false, folder: null, recycle_bin_only: false };

test("scans only accept drives the worker listed", () => {
  const g = new Grants();
  assert.throws(() => toScanRequest(base, g), ValidationError);
  g.grant("drive", "D:");
  assert.deepEqual(toScanRequest(base, g), base);
  assert.throws(() => toScanRequest({ ...base, drives: ["\\\\.\\PhysicalDrive0"] }, g), ValidationError);
  assert.throws(() => toScanRequest({ ...base, drives: [] }, g), ValidationError);
});

test("rejects bad categories, options and folders", () => {
  const g = new Grants();
  g.grant("drive", "D:");
  assert.throws(() => toScanRequest({ ...base, category: "exe" }, g), ValidationError);
  assert.throws(() => toScanRequest({ ...base, deep_scan: "yes" }, g), ValidationError);
  assert.throws(() => toScanRequest({ ...base, folder: "../etc" }, g), ValidationError);
  assert.equal(toScanRequest({ ...base, folder: "D:\\Photos" }, g).folder, "D:\\Photos");
  assert.throws(() => toScanRequest(null, g), ValidationError);
});

test("recover destinations must come from the dialog", () => {
  const g = new Grants();
  assert.throws(() => g.require("recover", "E:\\out"), ValidationError);
  g.grant("recover", "E:\\out");
  assert.equal(g.require("recover", "E:\\out"), "E:\\out");
});

test("results queries are bounded", () => {
  const q = { offset: 0, limit: 500, sort: "state", descending: false, text: null, states: ["excellent"] };
  assert.deepEqual(toResultsQuery(q), q);
  assert.throws(() => toResultsQuery({ ...q, limit: 1e6 }), ValidationError);
  assert.throws(() => toResultsQuery({ ...q, sort: "drop table" }), ValidationError);
  assert.throws(() => toResultsQuery({ ...q, states: ["deleted"] }), ValidationError);
});

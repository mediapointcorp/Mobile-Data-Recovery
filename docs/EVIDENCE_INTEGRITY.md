# Evidence integrity

## Principles

1. **Read-only access.** Evidence images are opened with `File::open`, which requests read access only. No code path opens evidence for writing.
2. **Hash before and after.** Each image scan computes the SHA-256 of the original before analysis and again afterwards. Both values, and whether they match, go into `jobs/<id>/evidence.json`, the audit log and the report.
3. **Working copies.** With *Analyse a hash-verified working copy* (the UI default; `--working-copy` on the CLI), the image is copied to `<case>/evidence/`. The copy is re-hashed from disk and must match before analysis starts. Analysis then reads only the copy, and the original is still re-hashed at the end.
4. **Separate outputs.** All outputs go into the case folder. The worker refuses:
   - a case folder that *is* the evidence;
   - a backup-folder scan whose case folder sits inside that folder;
   - an export destination inside the case, or one that is not empty.
5. **Copies of device files are verified.** Device-side and PC-side SHA-256 are compared, and existing files are never overwritten.
6. **Every exported file is re-hashed after copying** and listed in `manifest.csv` / `export.json`.
7. **Tamper-evident audit log.** `audit.jsonl` is append-only and flushed per event. Each line includes `prev`, the SHA-256 of the previous line, so editing, inserting or deleting any line breaks the chain. The report states whether the chain verifies.

## What is recorded

| Where | Content |
|---|---|
| `audit.jsonl` | Timestamp (UTC, RFC 3339), tool name and version, event and details. Covers case open/create, job create/start/finish (with parameters, status, error, stats), evidence registration and verification (with hashes), layout analysis, device identification (properties, capabilities, adb version), device listings, every acquired file (device path, local path, size, both hashes), resume points, root partition listing and acquisition, finalisation, export and report generation |
| `jobs/<id>/job.json` | Parameters (scan options, formats, chunk size, mode), status, run count, errors, limitations, statistics |
| `jobs/<id>/evidence.json` | Original path, analysed path, mode, size, SHA-256 before and after, working-copy hash, timestamps, `unchanged` |
| `jobs/<id>/layout.json` | Partition scheme, volumes, filesystem notes, entropy measurements |
| `report.json` / `report.html` | All of the above, summarised, plus per-result provenance |

## Evidence-integrity section of the report

The HTML report contains an **Evidence integrity** table: source ID, original path, mode, size, SHA-256 before, SHA-256 after, and the verdict *unchanged* / **CHANGED** / *not verified (job incomplete)*. Below it, the **Audit log** section shows whether the hash chain is intact.

Example from a test case (`fat_image_end_to_end`):

| Source | Mode | Size | SHA-256 before | SHA-256 after | Result |
|---|---|---|---|---|---|
| `image:…` | WorkingCopy | 35,651,584 | `a5a11c91…` | `a5a11c91…` | unchanged |

## Tested guarantees

| Test | Guarantee |
|---|---|
| `pr-model::evidence::working_copy_is_verified_and_original_unchanged` | The working copy is byte-identical, and the original is unchanged |
| `pr-model::evidence::detects_modified_evidence` | A modified original is reported as changed |
| `pr-model::evidence::destination_cannot_be_evidence` | Output onto or inside the evidence is refused |
| `pr-model::audit::tampering_is_detected` | Edited or deleted audit lines break the chain |
| `pr-worker::pipeline::fat_image_end_to_end` | The original's hash is identical after a full scan, and every required audit event is present |
| `pr-worker::pipeline::interrupted_scan_resumes_with_identical_results` | Two interruptions and a resume leave the evidence unchanged and give results identical to one uninterrupted run |
| `pr-adb::client::pull_never_overwrites_and_records_hashes` | Device copies never overwrite existing files, and hash mismatches are surfaced |

## Limits of what the tool can attest

- Hashes prove the image did not change *during analysis*. They cannot prove the image is a faithful acquisition of the device. That depends on how it was acquired, so record your imaging tool's own hashes as well.
- A partition read from a running, mounted phone can change during the read. The acquisition notes say so.
- File access times on the phone may be updated by reads (depending on mount options). Content is not modified.
- The audit chain detects modification of the log, but not its wholesale replacement. To prevent that, copy `audit.jsonl` (or its last-line hash) to write-once storage at the end of an examination.

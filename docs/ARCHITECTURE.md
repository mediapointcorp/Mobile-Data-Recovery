# Architecture and language selection

## Goals that shaped the design

1. **Correctness on hostile input.** Evidence images and phone file systems are untrusted binary data. Parsers must be memory-safe and bounded.
2. **Bounded memory on huge sources.** A 512 GB phone image has to scan in a few megabytes of RAM.
3. **Never modify evidence, and prove it.** Everything is read-only, with hashes before and after, and a tamper-evident audit log.
4. **Honest results.** The classification system makes it impossible to confuse an existing file, a thumbnail or a partial file with recovered deleted data.
5. **A UI that cannot be turned into a remote shell.** A compromised renderer must not be able to run commands or read arbitrary files.

## Language choices

| Layer | Choice | Why |
|---|---|---|
| Binary processing (carving, filesystems, validation, hashing) | **Rust** | Memory safety without a garbage collector is decisive when parsing attacker-controlled bytes. It runs at C-like speed (the carver scans page-cached data at about 2 GB/s on one core), streams with predictable memory, and cross-compiles cleanly to Windows (`x86_64-pc-windows-msvc`/`-gnu`). Maintained crates exist for decoding (`image` with zune-jpeg, png, image-webp), EXIF (`kamadak-exif`) and hashing (`sha2`). C++ was rejected because a parser bug would become a memory-corruption bug. |
| Orchestration, jobs, CLI, local service | **Rust** (same process as the engines) | One process holds every privileged operation (ADB, file access, evidence). The privilege boundary stays simple, and there is no second runtime to secure. |
| Desktop UI | **Electron + TypeScript** (vanilla DOM, no framework) | Ships on Windows 10/11 with an evergreen Chromium, displays JPEG/PNG/WebP/AVIF previews natively, and has a mature sandbox model (context isolation, sandboxed renderer, CSP). TypeScript catches protocol mismatches at compile time. No UI framework is used, which keeps the dependency surface small. |
| Device communication | **ADB (Android platform-tools)** | The only authorised, vendor-neutral channel to an unrooted phone. It is invoked as an external program with argument vectors, never through a PC-side shell. |

Tauri (Rust + WebView2) was considered for a smaller install. Electron was chosen because its sandbox and IPC hardening are well documented, and because a Linux CI or developer box can run it with no WebView2 or webkit2gtk dependency. The worker boundary makes the UI shell swappable later.

## Process model and trust boundaries

```
┌───────────────────────────── Electron ─────────────────────────────┐
│  Renderer (sandbox: true, contextIsolation: true, CSP, no Node)    │
│     │  window.photoRecovery.* (fixed, typed API from preload.ts)   │
│     ▼                                                              │
│  Main process                                                      │
│   • validates every request (src/main/validation.ts)               │
│   • only paths picked in native dialogs this session are accepted  │
│   • prmedia:// serves files from <case>/recovered|acquired only    │
│   • spawns and owns the worker; no network listener                │
└──────────────┬─────────────────────────────────────────────────────┘
               │ stdin/stdout, one JSON object per line
┌──────────────▼──────────── photorecovery-worker serve ─────────────┐
│  server.rs: allowlisted methods, deny_unknown_fields params         │
│  jobs/: image · device · local · partition (resumable)             │
│  pr-adb · pr-fs · pr-carve · pr-validate · pr-model                │
└────────────────────────────────────────────────────────────────────┘
```

## Crates

| Crate | Responsibility | Spec section |
|---|---|---|
| `pr-model` | `RecoveryResult` and the classifications; `ReadAt` read-only sources and the bounded `Cursor`; evidence registration, working copies and verification; hash-chained audit log | 4.H, 4.I |
| `pr-carve` | Streaming scanner with chunk overlap and checkpointable state. JPEG parser walks segments and entropy data (stuffing, fill bytes, RST sequence, marker legality after scans, zero-run cut-offs). PNG walks chunks with CRCs. WebP walks the RIFF chunk tree. HEIF/AVIF walks top-level boxes, parses `meta` for `ispe` and `iloc` extents. Exports partial candidates with an end marker appended | 4.E |
| `pr-validate` | Structural re-walk **and** decode (with allocation limits); SHA-256; 64-bit dHash; EXIF; exact and perceptual dedupe (exact LSH banding); thumbnail↔original matching | 4.F, 4.G |
| `pr-fs` | GPT (512/4096-byte sectors) and MBR; identification of FAT12/16/32, exFAT, NTFS, ext4, F2FS, LUKS, Android sparse; encryption indicators and entropy sampling; FAT live and deleted entry recovery with LFN names, overwrite detection and fragmentation awareness | 4.D |
| `pr-adb` | Allowlisted ADB operations with timeouts, serial/path validation, device-shell quoting, discovery classification (camera, screenshots, app media, system trash, recycle-bin heuristics, thumbnail caches and containers), root partition acquisition gated by a confirmation phrase | 4.B, 4.C |
| `photorecovery-worker` | Case folders, resumable jobs, classification, merge/dedupe, export, JSON and HTML reports, JSON-lines server, CLI | 4.A (service side), 7 |
| `pr-testkit` | Deterministic fixtures: real encoded JPEG (baseline, progressive, restart markers, EXIF thumbnails), PNG, WebP, synthetic HEIF; FAT32 builder with deletion, reuse and fragmentation; ground-truth disks | 6 |
| `pr-bench` | Ground-truth metrics, throughput and peak-memory measurement, fixture generation | 6 |

## Data flow for a raw image

```
image ──register (SHA-256, optional verified working copy)──► evidence.json
      ──analyze_layout (GPT/MBR → volumes → FS id, encryption hints)──► layout.json
      ──FAT volumes: walk live + deleted entries → read → validate──► results_fs.jsonl
      ──scan (8 MiB chunks + 32 B overlap)
            signature hit → format parser (bounded Cursor, 64 KiB buffer)
              ├─ Rejected → stats only (false positive)
              └─ Candidate → export → validate → classify ──► results_carve.jsonl
            every chunk: fsync results, write checkpoint.json (ScanState)
      ──verify (re-hash original; check working copy)──► evidence.json
finalize: merge jobs → reverse thumbnail links → SHA-256 dedupe → dHash similarity
          → thumbnail matching → results.json → report.json / report.html
```

### Bounded memory

- The scanner holds one chunk (default 8 MiB) plus 32 bytes of overlap.
- Each parser reads through a `Cursor` with a 64 KiB buffer, so its memory does not depend on candidate size.
- Per-format length limits apply: JPEG 128 MiB, PNG 256 MiB, WebP 64 MiB, HEIF 128 MiB, `meta` box 4 MiB. There is also a cap of 100,000 segments, chunks or boxes per candidate.
- Validation reads one exported candidate at a time, with the decoder allocation capped at 1 GiB and dimensions capped at 30,000 px.
- Measured peak RSS of a carve-only pass is about 12 MiB for both 0.3 GB and 1.6 GB images (docs/BENCHMARKS.md).

### Signatures across chunk boundaries

A hit is attributed to the chunk where it *starts*. Each read extends 32 bytes past the chunk, which is more than the longest signature (12 bytes for ISO-BMFF `ftyp` + brand). Parsing is random-access on the source and is not limited to the chunk. Tests place a JPEG at every offset from −20 to +20 bytes around a boundary, and check that chunk sizes of 64 B, 4093 B and 8 MiB give identical results.

### Resumability

`ScanState` holds the next offset, a "covered until" mark (to skip the interior of complete files), embedded-thumbnail windows and statistics. It is written atomically after each chunk, *after* the results file is synced. On resume, results at or beyond the checkpoint offset are dropped and found again. Exports are deterministic (`carve_<offset>.<ext>`), so re-exporting overwrites the same staging file. A test interrupts a scan twice and checks that the results match an uninterrupted run exactly.

## Case folder

```
<case>/
  case.json  audit.jsonl  results.json  report.json  report.html
  evidence/              verified working copies, acquired partition images
  acquired/<serial>/...  hash-verified copies of device files (sanitised paths)
  acquired/local-<id>/   copies of backup-folder files
  recovered/<class>/     exported carved / filesystem-recovered files
  jobs/<job-id>/         job.json checkpoint.json evidence.json layout.json
                         device.json listing.json results*.jsonl
```

## Worker protocol (v1)

Requests: `{"id": <number>, "method": "...", "params": {...}}`. Responses carry `result` or `error: {code, message}`. Events have `event` instead of `id`: `ready`, `job.progress {job_id, phase, done, total, message}`, `job.finished {job_id, status, error}`.

Methods: `tool.version`, `case.open`, `adb.status`, `adb.devices`, `adb.deviceInfo`, `adb.listPartitions`, `job.start`, `job.resume`, `job.cancel`, `job.list`, `results.finalize`, `results.list`, `export.run`, `report.generate`, `shutdown`. Unknown methods and unknown parameter fields are rejected. Only one job runs at a time.

## Classification rules

| Situation | Classification |
|---|---|
| File present in shared storage, a backup folder, or a live FS entry | Existing original (the best-located copy) / Existing duplicate (other byte-identical existing copies) |
| File in the Android system trash or an app recycle bin | Existing original **with a warning**: it still exists, so this is not deleted-data recovery |
| Deleted FAT entry, clusters free, data validates structurally and decodes | Deleted-file record |
| Deleted FAT entry with reused clusters, or data that does not validate | Partial image (some data) or Unavailable source data (start cluster reused or not an image); unavailable records are never exported |
| Carved, complete structure, decodes, longest side > 320 px | Carved image |
| Carved, complete, ≤ 320 px; EXIF-embedded; from a thumbnail container or cache | Thumbnail (never upscaled) |
| Carved, structure broken or truncated | Partial image (end marker appended on export; this is stated) |
| Carved, complete structure, cannot be decoded (HEIC/AVIF, or decoder failure) | Unverified candidate |

Decoders are lenient: zune-jpeg renders spliced or fragmented entropy data without complaint. A result is therefore `valid` only when the structural re-walk **and** decoding both succeed. This rule exists because a test found the decoder accepting a fragmented file.

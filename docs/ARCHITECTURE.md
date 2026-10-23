# Architecture

```
┌────────────── PhotoRecovery.exe (Electron + TypeScript, runs as administrator) ──────────────┐
│ sandboxed UI: wizard → scanning → results/preview/recover                                     │
│        │ fixed preload API (window.photoRecovery)                                             │
│ main process: validates every call; native folder dialogs                                     │
└────────┬─────────────────────────────────────────────────────────────────────────────────────┘
         │ stdin/stdout, one JSON object per line (no network)
┌────────▼────────────────── photorecovery-worker.exe (Rust) ──────────────────────────────────┐
│ server.rs   drives.list · scan.start/cancel/results/preview/recover                           │
│ recover.rs  the Recuva-style engine: list → state → deep scan → preview → recover             │
│ drives.rs   Windows drive list (type, SSD/TRIM, system drive), same-drive checks              │
│ pr-fs       NTFS (MFT) · FAT12/16/32 · exFAT · GPT/MBR · filesystem identification            │
│ pr-carve    deep scan: structure-aware JPEG/PNG/WebP/HEIC detection in free space             │
│ pr-validate picture validation (structure + decode)                                           │
│ pr-model    read-only, sector-aligned raw drive access                                        │
└───────────────────────────────────────────────────────────────────────────────────────────────┘
```

## Why these languages

- **Rust** for everything that touches the drive. Filesystem structures on a deleted or damaged drive are untrusted bytes, and Rust keeps parsing memory-safe at C speed (deep scan reads data at about 2 GB/s from cache). It also produces a single small Windows executable.
- **Electron + TypeScript** for the UI: a modern Windows interface, picture previews and a well-documented sandbox. The UI never touches drives; it only calls the worker.

## Scan pipeline (per drive)

1. Open `\\.\X:` read-only and determine the partition layout and filesystem.
2. List files from filesystem records (`pr_fs::files::list_files`):
   - **NTFS:** MFT records, including run lists and extension records.
   - **exFAT:** directory entry sets, with the `NoFatChain` flag and allocation bitmap.
   - **FAT:** directory entries, with LFN names and the FAT table.
3. Keep only deleted files of the chosen type, plus Recycle Bin items (`$R…`, named from `$I…`).
4. Give each file a state from cluster reuse and a signature check of its first bytes.
5. Deep scan (optional): compute free space from the allocation maps and carve pictures there. Pictures already found via records are skipped, and each candidate is validated.
6. Keep the results in memory. The UI pages, sorts and filters them through `scan.results`.
7. On **Recover**, read each file's data again from the drive and write it to the chosen folder. Destinations on a source drive are refused, and the original timestamps are restored.

## Advanced: forensic image analysis (CLI only)

`photorecovery-worker scan-image` keeps the original case-folder pipeline (`jobs/`, `case.rs`, `report.rs`, `finalize.rs`) for analysing **disk image files**: SHA-256 before and after, an optional verified working copy, a hash-chained audit log, resumable jobs, and JSON/HTML reports. The app does not use it.

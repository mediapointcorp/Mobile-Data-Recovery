# PhotoRecovery

PhotoRecovery recovers photos from Android phones (especially realme devices) connected to a Windows PC over USB, and from raw storage images. It is careful about forensics: it reads evidence only, hashes it before and after analysis, and labels every result by how it was obtained.

> **Recovery is never guaranteed.** Deleted photos can be recovered only when their data is still present, readable and not encrypted. On modern Android phones, internal storage uses file-based encryption, so deleted photos in internal storage generally **cannot** be recovered by any tool that lacks the keys. PhotoRecovery reports these limitations instead of producing anything that looks like a recovery.

## What it does

| Source | What you get | Classification |
|---|---|---|
| Android phone over ADB (no root) | Copies of every accessible image in shared storage (`/sdcard`, SD cards), including hidden folders, thumbnail caches, app media and the Android 11+ system trash (`.trashed-*`) | Existing original / Existing duplicate / Thumbnail |
| Backup or copy folder on the PC | The same search over a local folder | Existing original / duplicate / Thumbnail |
| Raw image (SD-card dump, `dd` image, acquired partition) | Filesystem analysis (live and deleted FAT entries), then structure-aware carving of unallocated and all other data | Deleted-file record, Carved image, Partial image, Thumbnail, Unverified candidate, Unavailable source data |
| Rooted, authorized **test** device (advanced) | A read-only `dd` image of a chosen partition, which you then scan as a raw image | — (it becomes evidence) |

Every result records its source, method, format, offset, length, dimensions, SHA-256, perceptual hash, validation status, classification, output path, warnings, limitations and associations (duplicate of, similar to, thumbnail of, embedded in).

## Architecture in one picture

```
┌──────────────── Windows desktop app (Electron + TypeScript) ────────────────┐
│ sandboxed renderer ──(fixed preload API)──► main process ──(stdio JSON)──┐ │
└──────────────────────────────────────────────────────────────────────────┼─┘
                                                                           ▼
┌────────────────────────── photorecovery-worker (Rust) ────────────────────────┐
│ jobs: image scan · device acquire · folder scan · partition acquire (root)    │
│ pr-adb ─ pr-fs ─ pr-carve ─ pr-validate ─ pr-model (results, evidence, audit) │
└───────────────────────────────────────────────────────────────────────────────┘
```

The UI never runs commands. It talks to the worker only through a fixed set of typed requests over the worker's stdin/stdout, with no network port. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full design and the reasons behind each language choice.

## Quick start (Windows 10/11)

```powershell
# 1. Prerequisites (Rust, Node.js LTS, Android platform-tools) — see docs/WINDOWS_BUILD.md
powershell -ExecutionPolicy Bypass -File scripts\setup-windows.ps1

# 2. Build and test everything
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1

# 3. Run the desktop app
cd app; npm start

# 4. (optional) Installer: app\release\PhotoRecovery Setup <version>.exe
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -Installer
```

To connect a phone, see [docs/USB_ADB_SETUP.md](docs/USB_ADB_SETUP.md). It covers enabling USB debugging on realme UI.

### Command line

The worker is also a complete CLI:

```text
photorecovery-worker devices
photorecovery-worker device-info --serial <SERIAL>
photorecovery-worker acquire-device --case D:\Cases\phone1 --serial <SERIAL>
photorecovery-worker scan-image --case D:\Cases\sd1 --image E:\dumps\sdcard.img --working-copy
photorecovery-worker scan-dir   --case D:\Cases\backup --dir E:\PhoneBackup
photorecovery-worker jobs       --case D:\Cases\sd1
photorecovery-worker resume     --case D:\Cases\sd1 --job <JOB-ID>
photorecovery-worker report     --case D:\Cases\sd1
photorecovery-worker export     --case D:\Cases\sd1 --dest F:\Recovered --class carved_image --class deleted_file_record
```

Type `q` and press Enter to cancel a running CLI job. Every job can be resumed.

### Try it without a phone

```bash
cargo run --release -p pr-bench -- fixtures --out fixtures
cargo run --release -p photorecovery-worker -- scan-image --case "$PWD/cases/demo" --image fixtures/sdcard-fat32.img
cargo run --release -p photorecovery-worker -- report --case "$PWD/cases/demo"
```

`sdcard-fat32.img` is a FAT32 SD card with three live and three deleted photos. `raw-disk.img` mixes intact, truncated, corrupted, fragmented, duplicated, thumbnail-only and fake-signature data. Each image comes with a ground-truth manifest.

## Recovery strategy (in order)

1. Discover accessible existing originals (device shared storage, backup folders).
2. Search copies, the system trash, thumbnail caches and `.thumbdata` containers. Containers are carved.
3. Acquire supported evidence without modification: hash-verified `adb pull`, read-only image access, or read-only `dd` on rooted test devices.
4. Identify the partition table and filesystems. Parse supported filesystem metadata: FAT12/16/32 live and deleted entries.
5. Carve raw data for JPEG (fully), PNG, WebP and HEIC/AVIF (structure only).
6. Validate (structure walk **and** decode), hash, deduplicate, link thumbnails, classify.
7. Export to a separate folder with a verified manifest, and generate the recovery and evidence-integrity report.

## Repository layout

```
crates/
  model/     result model, read-only sources, evidence hashing, hash-chained audit log
  carve/     streaming resumable scanner + JPEG/PNG/WebP/HEIF structure parsers
  validate/  structural + decode validation, SHA-256, dHash, EXIF, dedupe, thumbnail matching
  fs/        GPT/MBR, filesystem identification, encryption hints, FAT deleted-entry recovery
  adb/       allowlisted read-only ADB operations, discovery classification
  worker/    jobs, checkpoints, JSON-lines service, CLI, reports, export
  testkit/   deterministic fixtures: real JPEG/PNG/WebP encodes, FAT32 builder, ground-truth disks
  bench/     ground-truth benchmark harness and fixture generator
app/         Electron + TypeScript desktop UI
docs/        architecture, setup, acquisition, integrity, security, benchmarks
scripts/     Windows setup and build scripts
```

## Testing

```bash
cargo test --workspace                 # Rust unit + integration tests (synthetic ground truth)
cargo clippy --workspace --all-targets
cd app && npm ci && npm run typecheck && npm test
cargo run --release -p pr-bench -- run --out bench-out   # metrics → bench-out/bench.md
```

The tests cover:

- intact JPEG variants: baseline, progressive, restart intervals, EXIF thumbnails;
- signatures split at every position around a chunk boundary, and identical results across chunk sizes from 64 B to 8 MiB;
- truncated, corrupted, fragmented and spliced files, and fake signatures;
- duplicates and thumbnail-only scenarios;
- FAT live, deleted, overwritten and fragmented-deleted files;
- unsupported (ext4/F2FS), encrypted (LUKS, high-entropy) and sparse sources;
- interrupted and resumed scans, which must produce identical results;
- evidence hashes before and after analysis, and audit-chain tamper detection;
- ADB command construction and quoting, and the root confirmation gate;
- the service protocol, plus UI-side request validation.

## Documentation

- [Architecture and language selection](docs/ARCHITECTURE.md)
- [Windows installation and build](docs/WINDOWS_BUILD.md)
- [USB / ADB setup (realme)](docs/USB_ADB_SETUP.md)
- [Acquisition adapters and support limits](docs/ACQUISITION.md)
- [Evidence integrity](docs/EVIDENCE_INTEGRITY.md)
- [Security review](docs/SECURITY_REVIEW.md)
- [Benchmarks and known limitations](docs/BENCHMARKS.md)

## Status

Version 0.1.0 includes a working JPEG carver, PNG/WebP/HEIF structure carving, FAT record recovery, ADB acquisition and the desktop UI. **None of this has been validated on physical realme hardware yet.** ADB behaviour is covered by unit tests with recorded command output, and all recovery metrics come from synthetic ground truth. See [docs/BENCHMARKS.md](docs/BENCHMARKS.md) for what that does and does not show.

## License

No license has been chosen yet. Add one before distributing.

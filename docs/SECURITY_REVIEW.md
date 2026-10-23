# Security review

Scope: version 0.1.0. Covers the desktop app, the worker, the ADB adapter, the parsers, and the files they write.

## Threat model

| Asset | Threat |
|---|---|
| Source phone | Accidental or induced modification. Use of the tool to run arbitrary commands on the phone |
| Evidence images | Modification. Mix-ups between evidence and output |
| Analyst PC | Malicious evidence content attacking parsers or decoders. Malicious device file names (path traversal). Script injection into the UI or the report |
| Results | Misleading claims (fabricated or upscaled "recoveries"). Silent tampering with the audit trail |

Attackers considered: a crafted disk image or phone file system (untrusted bytes and names), a compromised renderer (for example an XSS payload in a file name), and a malicious local process (out of scope for full protection; see residual risks).

## Controls

### No arbitrary command execution

- The renderer gets no Node access: `sandbox: true`, `contextIsolation: true`, `nodeIntegration: false`. A strict CSP applies: `default-src 'none'; script-src 'self'`, no inline script, `connect-src 'none'`.
- The preload exposes a fixed API (`src/preload/preload.ts`) with no generic `invoke`.
- The main process rejects IPC from anything other than the app window's top frame. It validates every argument (`src/main/validation.ts`, unit-tested) and calls only allowlisted worker methods.
- The worker accepts a closed set of methods, and parameters are strictly typed (`deny_unknown_fields`). There is no method that executes a command.
- ADB is invoked with argument vectors (no PC-side shell). Device-side shell strings come from fixed templates. The only interpolated values are validated device paths, quoted with `'…'\''…'`.
  - Serials must match `[A-Za-z0-9._:\[\]-]{1,128}`.
  - Paths must lie under `/sdcard/`, `/storage/emulated/0/` or `/storage/`, with no `..` and no control characters.
  - Partition names must match `[A-Za-z0-9_-]{1,64}` and must appear in the device's own `/dev/block/by-name` listing.
- Tests check, for example, that `"$(reboot)"` stays quoted and that `abc; reboot` is rejected as a serial.

### No device modification

- Every ADB operation is a read: `getprop`, `df`, `ls`, `find`/`stat`, `sha256sum`, `pull`, and `dd if=` reading a block device.
- Root operations need an exact typed confirmation (`READ-ONLY ROOT ACQUISITION <serial>`). The main process checks it for acquisition jobs, and the ADB adapter checks it again for every root operation. A test asserts that no command runs before the confirmation is checked.
- `su` is never executed for capability detection; only `command -v su` is used.

### Raw drive access (PC drive scans)

- **Read-only access.** Drives are opened with read access only, and share mode read+write so Windows keeps working. No code path opens a device for writing.
- **Only listed drives can be scanned.** The renderer can only start a scan for a drive identifier the worker itself returned in `drives.list` during this session. The identifier is validated again in the worker: `X:` / `PhysicalDriveN` only, mapped to `\\.\` paths internally.
- **No writing to the scanned drive.** A drive scan refuses to run when the case folder is on that drive. For whole disks this is checked by mapping the case volume to its physical disk numbers; if the mapping is unknown, the scan is refused.
- **Elevation.** Raw reads need administrator rights. The app never elevates silently: **Restart as administrator** starts the same executable through the Windows UAC prompt, using a fixed command line with no renderer-supplied input.

### File-system safety on the PC

- **The renderer cannot name arbitrary paths.**
  - Image, folder and export paths must have been chosen in a native dialog during this session, and each grant is bound to its purpose.
  - Previews use `prmedia://case/…`, which accepts only `recovered/` or `acquired/` relative paths without traversal. The resolved real path must be inside the case root, so symlink escapes are blocked.
- **Device-supplied names are sanitised** (`local_relative_path`):
  - `..` and `.` segments are dropped;
  - `<>:"|?*\` and control characters are replaced;
  - Windows reserved names (`CON`, `COM1`, …) and trailing dots or spaces are neutralised;
  - each segment is limited to 200 characters.
- Filesystem-recovered names are reduced to `[A-Za-z0-9._-]`.
- Writes go through atomic temp files and renames. Device pulls use a temporary name and never overwrite.
- Job IDs from requests are validated before they are used as directory names.

### Parsing untrusted bytes

- Everything is written in Rust with no `unsafe`, except the `GetProcessMemoryInfo` call in the benchmark tool.
- **Every parser is bounded:**
  - per-format maximum candidate length;
  - a maximum of 100,000 segments, chunks or boxes per candidate;
  - a 4 MiB `meta` box limit and recursion depth 8 in ISO-BMFF;
  - FAT walk limits: 2,000,000 entries, depth 32, loop detection on cluster chains, 256 MiB FAT cap;
  - GPT entries capped at 1,024.
- Decoding uses `image::Limits`: 1 GiB maximum allocation and 30,000 px maximum dimension. EXIF parsing uses the maintained `kamadak-exif` crate.
- Request lines over 1 MiB are rejected.

### Output injection

- The UI renders all data with `textContent`. There is no `innerHTML` with data.
- The HTML report escapes every interpolated value (`& < > " '`) and carries its own CSP (`default-src 'none'; style-src 'unsafe-inline'`), so no script can run even if escaping were bypassed.
- In `manifest.csv`, CSV fields are quoted and embedded quotes are doubled.

### Integrity and honesty

- The audit log is hash-chained, and evidence hashes are taken before and after analysis.
- The classification rules forbid presenting thumbnails, existing files or partial data as recovered originals.
- A result is "valid" only when both the structural walk and decoding succeed.

### Local service exposure

- The worker opens no sockets. Its only interface is the stdio of the Electron main process that spawned it.
- The worker runs with the user's privileges. Neither the app nor the worker asks for elevation (`requestedExecutionLevel: asInvoker`).

## Findings during development (fixed)

| # | Finding | Fix |
|---|---|---|
| 1 | The JPEG decoder accepted fragmented or spliced files as valid, which would have mislabelled them as complete recoveries | Validation now re-walks the structure, and FAT reads with any reused cluster are never classified as intact (`pr-validate`, `jobs/image.rs`) |
| 2 | A random-data "gap" in a fragmented file was absorbed into the candidate because markers after scan data were not checked | Marker legality after scans is now checked, and a failed post-scan segment cuts back to the scan end (`pr-carve::jpeg`) |
| 3 | The audit log was not tamper-evident | Added the SHA-256 hash chain and verification in the report |

## Residual risks and recommendations

- **Dependency supply chain.** Cargo and npm lockfiles are committed. Run `cargo audit` and `npm audit` in CI, and review updates. Electron must be kept current, since Chromium security fixes ship through it.
- **Code signing.** Release builds are unsigned. Sign the installer and worker, and verify signatures before running in an evidence lab.
- **Local malicious processes** running as the same user can modify case files. Store cases on access-controlled volumes, and archive the audit log's final hash to write-once storage.
- **`adb` binary trust.** PhotoRecovery runs the first `adb` found (explicit path, then `PATH`, then SDK locations). Ensure `PATH` does not contain untrusted directories, or configure an explicit path.
- **Decoder denial of service.** A crafted image within the limits can still take seconds to decode. Validation is single-threaded per job and can be cancelled between candidates.
- **Root acquisition** inherits whatever `su` on the test device does. Use it only on devices you control.

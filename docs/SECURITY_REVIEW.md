# Security review

Scope: PhotoRecovery 0.1.0 for Windows. Covers the desktop app (Electron), the recovery engine (`photorecovery-worker`) and the filesystem parsers.

## Threat model

| Asset | Threat |
|---|---|
| The drive being recovered | Accidental writes that overwrite the deleted data |
| The PC | A crafted or damaged drive attacking the parsers; malicious file names (path traversal, reserved names); script injection into the UI; misuse of the administrator rights the app runs with |
| The user | Misleading results: unrecoverable files presented as recoverable |

## Controls

### Never writing to the source

- Drives are opened with read access only, shared with Windows. No code path opens a drive for writing.
- A scan writes nothing to disk. Results stay in memory.
- **Recover** writes only to the chosen folder. It refuses a folder on any drive the selected files come from:
  - drive letters are compared after canonicalising the path;
  - for whole disks, the destination volume's physical disk numbers are compared;
  - if that mapping is unknown, recovery is refused.

### Administrator rights

Raw drive reads need elevation, so the installer marks the app `requireAdministrator` and Windows shows a UAC prompt at launch. Because the app runs elevated, it limits what the UI can do:

- The renderer is sandboxed: `sandbox`, `contextIsolation`, no Node.js, and a strict CSP (`script-src 'self'`, `connect-src 'none'`).
- The preload script exposes a fixed set of functions, with no generic IPC.
- The main process accepts calls only from the app window's top frame, and validates every argument (`src/main/validation.ts`, unit-tested):
  - scans can target only drive IDs that the worker itself listed;
  - recovery can write only to a folder chosen in the native folder dialog;
  - file IDs, sort keys, file-type categories and states are checked against fixed lists.
- The worker accepts a closed set of methods with strictly typed parameters (`deny_unknown_fields`). It re-checks that drives come from its own listing. It has no method that runs commands, and it opens no network sockets: its only interface is the stdio of the app process.
- **Restart as administrator** (shown only if the app was started without elevation) runs a fixed PowerShell `Start-Process -Verb RunAs` of the app's own executable. No renderer input reaches it.

### Untrusted on-disk data

- All parsing is Rust, without `unsafe` code in the parsers. The Windows API calls (`DeviceIoControl`, token query) are confined to `model/device.rs` and `worker/drives.rs`.
- **Bounded work:**
  - NTFS: record fix-ups are verified, run lists are length-checked, and limits are 64 M records, 1 M runs and 512 MiB `$Bitmap`;
  - exFAT/FAT: directory size, depth and entry count are capped, and FAT chains have loop detection;
  - deep scan: per-format size and structure limits;
  - picture decoding: 1 GiB allocation and 30,000 px limits.
- **Recovered file names are sanitised:**
  - `<>:"|?*\/` and control characters are replaced;
  - trailing dots and spaces are removed;
  - `.` and `..` are neutralised;
  - names are limited to 200 characters;
  - folder structure is rebuilt only from sanitised components under the chosen folder;
  - existing files are never overwritten (`name (1).ext`).

### Output

- The UI inserts every file name, path and comment with `textContent`, never as HTML.
- Previews are JPEG thumbnails re-encoded by the engine and shown as `data:` URLs.

## Findings fixed during development

| Finding | Fix |
|---|---|
| Lenient decoders accepted spliced or fragmented JPEGs as valid | Pictures count as valid only when the structural walk and decoding both succeed |
| Photos still in the Recycle Bin were missed by drive scans | Recycle Bin items are always listed, with original names from `$I` files |
| The "output inside source" check failed for not-yet-existing paths on Windows (`\\?\` prefix mismatch) | The nearest existing ancestor is canonicalised before comparing |
| A Windows raw read of a non-device path failed on the length IOCTL | Falls back to the file size for regular files |

## Residual risks

- **Unsigned builds:** sign the installer and executables before distributing; SmartScreen warns about unsigned builds.
- **Electron updates:** Chromium security fixes ship through Electron, so keep it current. Run `cargo audit` and `npm audit` in CI.
- A crafted picture within the limits can still take seconds to decode. Previews are generated one at a time, on request.

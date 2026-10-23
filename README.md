# PhotoRecovery

PhotoRecovery recovers deleted files on Windows. It works like Recuva: choose the type of file, choose where the files were, and it lists what can be brought back. Each file gets a state from **Excellent** to **Unrecoverable**, and you can preview pictures and recover the files you tick to a folder of your choice.

It works on internal hard disks and SSDs, USB sticks, memory cards and external drives formatted **NTFS, FAT32 or exFAT**.

> **Recovery is never guaranteed.** A deleted file can come back only while its data is still on the drive.
> - Every file you save afterwards can overwrite it, so stop using the drive and scan as soon as possible.
> - On SSDs, Windows' TRIM usually erases deleted data within minutes, so chances there are low.
> - PhotoRecovery shows such files as *Unrecoverable* instead of pretending.

## How to use it

1. **Install** `PhotoRecovery Setup 0.1.0.exe`. When you start the app, Windows asks for administrator permission, because reading a drive directly needs it. PhotoRecovery only reads the drive.
2. **What type of files?** Pictures, Music, Documents, Video, Compressed, Emails or All files.
3. **Where were the files?**
   - *I'm not sure* (all drives)
   - *In the Recycle Bin*
   - *On a specific drive, USB stick or memory card*
   - *In a specific folder*
4. **Deep Scan** (optional): slower. It also searches free space for pictures whose file records are gone.
5. **Results:** filename, original path, date, size, state and a comment. Click a picture to preview it, tick the files you want, and click **Recover…**.
6. **Choose a folder on a different drive** (for example a USB stick). PhotoRecovery refuses to write onto the drive you are recovering from, because that could overwrite the files you want back.

| State | Meaning |
|---|---|
| 🟢 Excellent | None of the file's data has been reused; it should come back intact. |
| 🟡 Poor | Part of the data was overwritten; the file may be damaged. |
| 🟠 Very poor | Most of the data was overwritten, or the start of the file no longer matches its type. |
| 🔴 Unrecoverable | The data is gone: overwritten, erased to zeros (SSD TRIM) or encrypted. It cannot be recovered. |

## What it finds

| Situation | Found? |
|---|---|
| File deleted normally and **still in the Recycle Bin** | ✅ shown with its original name and folder; you can also restore it from the Recycle Bin |
| Recycle Bin **emptied** | ✅ if the space has not been reused; original name restored from Windows' Recycle Bin info file |
| **Shift+Delete** (permanent delete) | ✅ if the space has not been reused (NTFS, FAT, exFAT) |
| Large or fragmented files on NTFS | ✅ read through NTFS's own record of where the pieces were |
| Fragmented files on FAT/exFAT | ⚠️ often only partly, because these filesystems forget the pieces on delete |
| Formatted, RAW or damaged drive | ⚠️ Deep Scan finds pictures (JPEG, PNG, WebP, HEIC) by content, without names |
| Deleted from an SSD a while ago | ❌ usually erased by TRIM, shown as Unrecoverable |
| BitLocker drive | Unlock it in Windows first, then scan it |

## Building from source (Windows 10/11)

```powershell
powershell -ExecutionPolicy Bypass -File scripts\setup-windows.ps1     # Rust, Node.js, VS Build Tools (asks first)
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -Installer
```

The installer is written to `app\release\`. For development, run `cd app; npm start` from an administrator terminal. See [docs/WINDOWS_BUILD.md](docs/WINDOWS_BUILD.md).

Every push also builds the installer on GitHub Actions. To download it, open the latest run → **Summary** → **Artifacts** → `photorecovery-windows-installer`.

### Command line

The engine is also a command-line tool. Run it from an administrator terminal:

```text
photorecovery-worker drives
photorecovery-worker scan --drive E: --type pictures
photorecovery-worker scan --drive D: --type documents --folder "D:\Work" --recover-to F:\Recovered --keep-folders
photorecovery-worker scan --drive E: --type pictures --deep --recover-to F:\Recovered
```

## Project layout

```
app/          Windows desktop app (Electron + TypeScript): wizard, results, preview, recover
crates/
  worker/     recovery engine (recover.rs), drive access, app service, CLI
  fs/         NTFS, FAT12/16/32 and exFAT parsers with deleted-file recovery; partition tables
  carve/      deep scan: finds JPEG/PNG/WebP/HEIC by structure in free space
  validate/   picture checks (structure + decode), hashes
  model/      raw drive access (read-only, sector-aligned), shared types
  testkit/    test-file generators
  bench/      benchmarks
docs/         how it works, build, security, test results
```

## Testing

```powershell
cargo test --workspace
cd app; npm ci; npm run typecheck; npm test
```

Recovery is tested against real disk images. They were made with the real NTFS and exFAT filesystem drivers, with files deleted in every common way: Shift+Delete, Recycle Bin, emptied Recycle Bin, fragmented, and overwritten. Every recovered file is checked byte-for-byte against the original. On GitHub's Windows machines, a test also reads the live `C:` drive through the same raw-access path the app uses. See [docs/TESTING.md](docs/TESTING.md).

## Documentation

- [How it works](docs/HOW_IT_WORKS.md)
- [Building on Windows](docs/WINDOWS_BUILD.md)
- [Security review](docs/SECURITY_REVIEW.md)
- [Testing and results](docs/TESTING.md)

## License

No license has been chosen yet. Add one before distributing.

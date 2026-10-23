# How PhotoRecovery works

## Reading the drive

PhotoRecovery opens a drive such as `\\.\E:` with **read access only**, shared with Windows so the system keeps working. Windows only allows raw reads in whole sectors, so every read is rounded to 4 KiB boundaries internally. Reading a drive this way needs administrator rights, so the installer asks for them when the app starts.

**Nothing is written during a scan.** Results are kept in memory. Data is written only when you click **Recover…**, and only to a folder on a different drive.

## Finding deleted files (normal scan)

Each filesystem keeps records of its files. Deleting a file marks its record and its space as free, but usually leaves the record's details in place until something reuses them.

| Filesystem | What survives a delete | What PhotoRecovery does |
|---|---|---|
| **NTFS** (Windows drives, large external disks) | The file's MFT record: name, folder, dates, size and its **data-run list** (where every piece of the file is) | Reads the master file table, applying its integrity fix-ups, and rebuilds each deleted file's path. It follows the data runs, so fragmented files come back intact. Compressed or EFS-encrypted files are reported as Unrecoverable. |
| **exFAT** (memory cards and USB sticks over 32 GB) | The directory entry set: name, dates, size, first cluster, and a "contiguous" flag | Contiguous files are located exactly. For fragmented files the cluster chain is often cleared on delete, so the data is assumed to continue from the first cluster and the state shows if that space was reused. |
| **FAT32/FAT16** (small USB sticks and cards) | The directory entry: the name minus its first letter if there is no long name, size and first cluster | Same approach as exFAT; long file names are rebuilt from LFN entries. |

**State.** For each file, PhotoRecovery checks the drive's current allocation map (`$Bitmap` on NTFS, the allocation bitmap on exFAT, the FAT on FAT32):

- If none of the file's clusters are in use again, the state is **Excellent**.
- If some are, it is **Poor**; if most are, **Very poor**.
- If the first cluster is in use again, the state is **Unrecoverable**.

It also reads the first bytes and compares them with the file type's signature: JPEG `FF D8 FF`, PNG, PDF `%PDF`, DOCX/ZIP `PK`, MP4 `ftyp` and many more. Data that reads as zeros, which is typical of SSD TRIM, is marked **Unrecoverable**. Data that no longer matches the type is marked **Very poor**.

## The Recycle Bin

A normal delete moves the file to `$Recycle.Bin\<user>\$Rxxxxxx.ext`. Windows also writes a small `$Ixxxxxx.ext` file that holds the original path.

- PhotoRecovery lists files still in the bin under their **original name and folder**.
- It also reads the `$I` records of an **emptied** bin, as long as they have not been reused, so emptied files get their names back too.

## Deep Scan

Deep Scan reads the drive's **free space** (or the whole drive, if no filesystem is recognised) and recognises pictures by their internal structure, not only their first bytes:

- **JPEG:** markers, tables, scan data and restart markers are followed.
- **PNG:** chunk checksums are verified.
- **WebP:** the RIFF chunks are walked.
- **HEIC/AVIF:** the box layout is checked.

Each candidate is then decoded to confirm it is a real picture. Found pictures have no original name (`deep_….jpg`). Pictures already found through file records are not listed twice, and tiny previews and icons are skipped. Fragmented pictures come back only up to the first break, marked **Poor**.

## Recovering

- Recovered files are written to the folder you choose, and their original modified date is restored.
- **Restore folder structure** recreates the original folders under the destination.
- Name clashes become `name (1).jpg`.
- The destination must be on a different drive from the files being recovered; this is checked by drive letter, and by physical disk for whole disks.

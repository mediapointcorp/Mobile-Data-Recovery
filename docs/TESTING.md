# Testing and results

```powershell
cargo test --workspace            # all engine tests
cd app; npm test                  # UI request validation
```

## Real filesystem tests

The test drives in `crates/fs/tests/fixtures/` are **real** volumes. They were formatted by `mkntfs` and `mkfs.exfat`, and files were written and deleted through the actual NTFS and exFAT filesystem drivers, not hand-built structures. The `make_*.sh` scripts next to them regenerate the images and are kept for maintainers. Every recovered file is compared byte-for-byte (SHA-256) with the original.

### `ntfs-recycle`: the three ways people delete files (NTFS USB disk)

| Photo | How it was deleted | Result |
|---|---|---|
| Photo_0000, 0001 | Shift+Delete | Excellent; byte-identical; original name and folder |
| Photo_0002, 0003 | Normal delete (still in the Recycle Bin) | Excellent; byte-identical; original name restored from `$I` |
| Photo_0004 | Recycle Bin, then emptied | Excellent; byte-identical; original name restored from the deleted `$I` record |
| Photo_0005 | Not deleted | Not listed |

### `ntfs-deleted`: fragmentation and overwriting (NTFS)

| Scenario | Result |
|---|---|
| 3 deleted photos, untouched | byte-identical |
| 1 deleted photo in **7+ fragments** | byte-identical (data-run list followed) |
| 1 deleted photo whose record and space were reused | not listed as recoverable |
| 3 existing photos | not listed |

An independent tool (`ntfsundelete`) agrees on which files are recoverable.

### `exfat-card`: memory card (exFAT)

| Scenario | Result |
|---|---|
| 2 deleted photos + 1 deleted PDF | Excellent; byte-identical |
| 1 deleted photo written into scattered free space (the exFAT driver cleared its chain) | **not** Excellent (half its assumed clusters are reused): honest Poor/Very poor |
| 2 existing photos | not listed |
| *Documents* filter | only the PDF |

### Other tests

- **Recover safety:** recovering onto the scanned drive is refused, and nothing is written.
- **UI service:** unknown methods and fields are rejected; scans are limited to drives the worker listed.
- **Live Windows drive:** on GitHub's Windows machines a test reads the system drive `C:` through the same raw-access path. It parses the real NTFS boot sector, `$MFT` and `$Bitmap`, and checks that free plus used space equals the volume size. The test fails rather than skips if it cannot read the drive.
- **Deep scan engine:** intact, truncated, corrupted, fragmented and fake-signature pictures are tested, including signatures split across read-chunk boundaries. Interrupted and resumed scans give identical results.
- **FAT32:** live, deleted, overwritten and fragmented files.

## Deep scan benchmark

`cargo run --release -p pr-bench -- run --out bench-out` builds a disk image of about 300 MB from a ground-truth manifest and measures deep-scan results. Last run (4 vCPU, data in cache):

| Metric | Value |
|---|---|
| Throughput | ~2.2 GB/s (CPU-bound; real drives are limited by disk speed) |
| Peak memory | ~12 MiB, independent of drive size |
| Intact pictures recovered byte-identical | 100% |
| Fake signatures accepted | 0 of 5 |
| Truncated/corrupted/fragmented pictures giving a partial result | 100% |

These are synthetic best-case figures. Real drives contain encoders, fragmentation patterns and TRIM behaviour the generator does not model.

## Known limitations

1. **SSDs:** TRIM usually erases deleted data quickly. Such files show as Unrecoverable.
2. **FAT/exFAT fragmented files:** the cluster chain is usually lost on delete, so they come back partial (shown as Poor/Very poor).
3. **Deep scan** recognises pictures only (JPEG, PNG, WebP, HEIC/AVIF). Other file types need their filesystem record.
4. **HEIC** pictures are recovered but not previewed.
5. Not supported: ReFS, BitLocker-locked drives (unlock first), and other operating systems' filesystems (ext4, APFS, HFS+). For those, deep scan still finds pictures in unencrypted space.
6. Drives are scanned while Windows uses them, so results reflect the moment each part was read.

# Benchmarks and known limitations

## How to reproduce

```bash
cargo build --release -p pr-bench
target/release/pr-bench run --out bench-out                      # default: seed 42, 24 JPEGs, 256 MB filler
target/release/pr-bench run --pad-mb 1500 --jpegs 48 --seed 7    # larger image
```

`bench-out/` then contains `bench.md` (the tables below), `bench.json` (all numbers) and `manifest.json` (ground truth).

## What is measured, and how

The harness builds a synthetic raw image from a **ground-truth manifest**:

- real JPEG encodes: baseline, progressive, restart intervals, EXIF thumbnails, qualities 70–94, sizes 160–2048 px;
- PNG, lossless WebP and a structural HEIF;
- files placed straddling chunk boundaries;
- an exact duplicate;
- truncated files followed by zero-filled blocks;
- files with 2 KiB of random data overwritten in the middle;
- files split into two fragments with 16 KiB of foreign data between them;
- small thumbnail-only previews;
- fake signatures (JPEG APP0, PNG, RIFF/WEBP and `ftypheic` headers followed by garbage);
- random filler with no image signatures.

The full pipeline then runs: register → FS analysis → carve → export → validate → classify. Results are compared with the manifest:

| Metric | Definition |
|---|---|
| Byte-identical recovery rate | Placed files in *recoverable* scenarios (intact, chunk boundary, duplicate, thumbnail-only) for which a raw-carve result at the same offset has the same SHA-256 as the original |
| Valid-candidate precision | Of results classified *Carved image*, the fraction whose SHA-256 equals some original in the manifest |
| False-positive rate | Raw-carve results whose offset is not a placed file (embedded EXIF thumbnails are excluded; they are counted separately) ÷ all raw-carve results |
| Partial-image yield | Truncated, corrupted and fragmented files that produced a *Partial image* result |
| Throughput / peak memory | Carve-only pass in a **separate child process** (`VmHWM` on Linux, `PeakWorkingSetSize` on Windows), so fixture generation does not inflate memory |

Inaccessible data is never mixed into these rates. The encrypted control is reported separately, and FAT records whose data was overwritten are reported as *unavailable*, not as failures of the recovery rate.

## Results (synthetic ground truth)

Environment: Linux x86_64, 4 vCPU (Intel Xeon @ 2.10 GHz), image in page cache. Tool 0.1.0, seed 42.

| Metric | Value |
|---|---|
| Image size | 301.3 MB |
| Carve-only throughput | 2230 MB/s (0.14 s) |
| Peak memory (carve-only process) | 11.9 MiB |
| Full pipeline time (export + validate) | 1.41 s |
| Byte-identical recovery (recoverable scenarios) | 100.0% |
| Valid-candidate precision | 100.0% (32/32) |
| False-positive rate | 0.0% (0/59) |
| Fake signatures accepted | 0/5 |
| Partial-image yield (truncated/corrupted/fragmented) | 100.0% |
| Embedded EXIF thumbnails recovered | 13 |

| Scenario | Placed | Byte-identical | Partial | Other result | Missed |
|---|---|---|---|---|---|
| intact | 29 | 29 | 0 | 0 | 0 |
| chunk boundary | 3 | 3 | 0 | 0 | 0 |
| duplicate | 1 | 1 | 0 | 0 | 0 |
| thumbnail only | 2 | 2 | 0 | 0 | 0 |
| truncated | 8 | 0 | 8 | 0 | 0 |
| corrupted | 8 | 0 | 8 | 0 | 0 |
| fragmented | 8 | 0 | 8 | 0 | 0 |
| false positive | 5 | 0 | 0 | 0 | 0 |

Fragmented files are **never** reported as byte-identical. The part before the fragmentation point is recovered as a partial image.

### FAT32 deleted-record recovery (10 files per scenario)

| Scenario | Files | Byte-identical | Partial | Reported unavailable | Missed |
|---|---|---|---|---|---|
| live | 10 | 10 | 0 | 0 | 0 |
| deleted, contiguous, untouched | 10 | 10 | 0 | 0 | 0 |
| deleted, clusters reused by another file | 10 | 0 | 0 | 10 | 0 |
| deleted, fragmented (interleaved with another file) | 10 | 0 | 10 | 0 | 0 |

### NTFS deleted-file recovery (real volume)

The fixture is `crates/fs/tests/fixtures/ntfs-deleted.img.gz`. It is a 12 MB volume made with `mkntfs`; files were written and deleted through the real `ntfs-3g` driver by `make_ntfs_fixture.sh`. The independent `ntfsundelete` tool reports IMG_0003–0006 as 100% recoverable.

| Scenario | Files | PhotoRecovery result |
|---|---|---|
| live | 3 | byte-identical (image scan) / not reported (drive scan, deleted-only) |
| deleted, clusters untouched | 3 | byte-identical, original path and timestamps |
| deleted, fragmented (7+ data runs) | 1 | byte-identical, run list followed |
| deleted, MFT record and clusters reused | 1 | not claimed |

### Scaling

Same harness, `--pad-mb 1500 --jpegs 48 --seed 7`: a 1619.9 MB image scanned at 2238 MB/s, with peak memory 11.9 MiB. Memory does not grow with image size.

### Encrypted control

32 MB of random data stands in for ciphertext. It produced 0 results, and the limitation "most likely encrypted… cannot recover images from ciphertext" was reported.

## How to read these numbers

- **They are an upper bound, not a prediction.** The fixtures come from the same team that wrote the parsers. Real phone data has features the generator does not produce:
  - hardware-encoder JPEGs with MPF secondary images and maker notes;
  - real HEIC files with HEVC tiles;
  - fragmentation patterns from F2FS and ext4;
  - TRIM/discard-zeroed blocks;
  - images from many different encoders.
- **Throughput is CPU-bound in page cache.** Real acquisitions are limited by the source: USB 2.0 ADB pulls run at roughly 20–40 MB/s, card readers at 20–90 MB/s, and NVMe at several GB/s. Validation and export dominate on image-dense data (1.41 s for 301 MB here, most of it decoding).
- **Recovery rate by device model, Android version, filesystem and deletion scenario.** The harness and report support this breakdown, but **no physical-device measurements have been made yet**. Fill this table only from runs with a ground-truth manifest created on the device (photos taken and then deleted in a controlled way):

| Device model | Android / realme UI | Filesystem | Encryption | Scenario | Files | Byte-identical | Partial | Unavailable |
|---|---|---|---|---|---|---|---|---|
| *(no hardware runs recorded)* | | | | | | | | |

  What to expect, based on how Android works rather than on measurements:
  - Internal storage on FBE devices (all devices launched with Android 10+, including current realme phones): deleted photos are **not recoverable** by any method without keys. Only the system trash, thumbnails and app copies are available, and they are reported as existing files.
  - Portable SD cards (FAT32/exFAT, unencrypted): deleted photos are often recoverable until they are overwritten.

## Known limitations

1. **No fragment reassembly.** Fragmented files yield the first fragment as a partial image. Bifragment gap carving is a possible future addition, but it would be labelled separately and never as an original.
2. **HEIC/AVIF** are validated structurally only (box walk and `iloc` extents). The bundled decoders cannot decode HEVC/AV1, so these results are *Unverified candidates*. HEIC is not previewed in the UI (AVIF previews depend on Chromium's decoder). They are exported intact.
3. **exFAT, ext4, F2FS**: identification only, with no deleted-record parsing. Their space is still carved. Memory cards and USB sticks over 32 GB are usually exFAT; on them, deleted photos are found by carving only, so fragmented ones come back partial.
10. **SSDs.** On SSDs with TRIM (almost all modern Windows PCs), deleted clusters usually read back as zeros within minutes. NTFS records still list such files, but they are reported as *Unavailable* ("clusters now read as zeros"), never as recovered.
11. **NTFS scope.** Only files with image extensions are examined through the MFT. Photos renamed to other extensions are found by carving. Live drives change during the scan, so results reflect the moment each part was read.
4. **Android sparse, E01, AFF4, VHD and split raw images** need conversion to a single raw image first.
5. **Thumbnail size heuristic.** A complete carved image whose longest side is ≤ 320 px is classified as a thumbnail. Genuine small originals (stickers, icons) will be labelled thumbnails, with a warning saying why.
6. **dHash similarity** misses heavily cropped thumbnails, such as square centre crops. They stay unlinked rather than being linked wrongly.
7. **Lenient decoders.** The structural check catches spliced JPEG data that breaks marker rules. Foreign data that happens to be syntactically valid JPEG entropy data (for example another JPEG's scan data with compatible markers) can still decode with visual artefacts. Restart-marker checks narrow this for files with restart intervals.
8. **One job at a time** per worker, and validation is single-threaded.
9. **Device behaviour unverified on hardware.** ADB command construction and parsing are unit-tested against recorded output formats (toybox `find`, `stat`, `sha256sum`, `getprop`, `df`). Realme-specific locations have not been confirmed on a physical device.

# Acquisition adapters and support limitations

Each adapter states exactly what it reads, what it can never return, and how it protects the source.

## Support matrix

| Adapter | Requires | Reads | Can recover deleted data? | Source modification | Validation status |
|---|---|---|---|---|---|
| **ADB shared storage** (`device_acquire`) | USB debugging authorised | Image files and thumbnail containers under `/sdcard/` and SD-card volumes, via `find` + `stat` and `adb pull -a` | Only what Android still keeps as files: system trash (`.trashed-*`), app recycle bins (heuristic), thumbnail caches. These are reported as **existing** files | None. `pull` and `sha256sum` are reads; atime may update depending on mount options | Unit-tested with recorded command output; **not yet run against physical realme hardware** |
| **Local folder** (`local_scan`) | A folder on the PC (backup, copy, extracted archive) | Image files and `.thumbdata*` containers; symlinks and junctions are not followed | Same as above (existing files only) | None. Files are copied into the case first and hashed before and after copying | Integration-tested |
| **PC drive** (`drive_scan`) | Windows administrator rights; the case folder must be on a different drive | `\\.\D:` (a volume) or `\\.\PhysicalDriveN` (a whole disk), opened read-only and shared with Windows; reads are 4 KiB-aligned | **Yes**: NTFS deleted MFT records (full run lists, fragmented files included), FAT deleted entries, and carving of the unallocated clusters only (default) | None. The drive is never opened for writing. It is in use by Windows, so it is not hashed; image it first for evidential work | NTFS tested on real `mkntfs`/`ntfs-3g` volumes; raw access tested in CI on the Windows system drive |
| **Raw image** (`image_scan`) | A raw (`dd`) image: SD-card dump, USB-reader image, partition image | Whole image: partition table, filesystem metadata, all data including unallocated space | **Yes**, where data is present and unencrypted: FAT and NTFS deleted entries, plus carving of everything | None. Opened read-only and hashed before and after. Optional verified working copy | Integration-tested on synthetic ground truth |
| **Root partition image** (`partition_acquire`) | Rooted, explicitly authorised **test** device; exact confirmation phrase `READ-ONLY ROOT ACQUISITION <serial>` | `/dev/block/by-name/<partition>` via `adb exec-out su -c 'dd if=… bs=4194304'`, hashed while streaming | Only via a subsequent raw-image scan. `userdata` on FBE devices is ciphertext | Reads only. Running `su` may be logged by Magisk or the root manager. A mounted partition can change while it is read, so the image is not a static snapshot | Unit-tested with a fake runner; **validate on your test device before relying on it** |

### Not supported, by design

- Bootloader unlocking, rooting, flashing custom recoveries, factory resets, or any operation that writes to the device.
- Android backup (`adb backup`). It is deprecated, excludes media, and many apps opt out.
- MTP access. ADB covers the same shared storage with better metadata.
- Decrypting FBE/FDE storage.

### Formats

| Input format | Status |
|---|---|
| Raw `dd` / `.img` / `.bin` / `.001` (first segment only for split images) | Supported |
| Android sparse images (`simg`, magic `0xED26FF3A`) | Detected and reported. Convert first with `simg2img` |
| E01 / AFF4 / VMDK / VHD(X) | Not supported. Export to raw with your imaging tool first |

### Filesystems

| Filesystem | Identification | Live entries | Deleted-record recovery | Carving |
|---|---|---|---|---|
| FAT12/16/32 | ✓ | ✓ (chain followed) | ✓ (contiguous assumption, overwrite detection, structural validation) | ✓ |
| exFAT | ✓ | — | not yet implemented (planned: `NoFatChain` stream extensions make this reliable) | ✓ |
| ext4 | ✓ (+ encryption flag) | — | Not offered: ext4 zeroes extent mappings on delete | ✓ (unencrypted data only) |
| F2FS | ✓ (+ encryption flag, best-effort superblock parse) | — | Not offered: out-of-place writes and invalidated node blocks | ✓ (unencrypted data only) |
| NTFS | ✓ | ✓ (MFT records, optional in drive scans) | ✓ (MFT run lists incl. fragmented files and extension records; `$Bitmap` overwrite detection; compressed and EFS-encrypted files reported as unavailable) | ✓ (drive scans: free clusters only) |
| LUKS | ✓ | — | — | Reported as encrypted; nothing is fabricated |

## Device identification

`device_info` reads `getprop` and records:

- brand, manufacturer, model, device and Android version/SDK;
- security patch, `ro.crypto.state` / `ro.crypto.type`, build type and verified-boot state;
- bootloader lock state, and whether `su` is present (checked with `command -v`, never executed);
- shared-storage size and free space, and external volumes.

realme devices are recognised by brand or manufacturer. The realme UI / ColorOS version is read from `ro.build.version.realmeui`, `…oplusrom` or `…opporom`, whichever exists.

## Discovery classification on the device

| Path pattern | Kind | Classification |
|---|---|---|
| `…/DCIM/Camera/…`, other `DCIM` | camera | Existing original |
| `…/Screenshots/…` | screenshot | Existing original |
| `Android/media/…`, WhatsApp, Telegram, … | app media | Existing original |
| `.trashed-<expiry>-<name>` | system trash | Existing original + "still exists, not deleted-data recovery" warning |
| folders named `*recycle*`, `*trash*`, `.deleted` | recycle bin (heuristic) | Existing original + warning |
| `.thumbnails`, `*thumbnail*`, `thumbcache` | thumbnail cache | Thumbnail |
| `.thumbdata*` | thumbnail container | Carved; each preview is a Thumbnail |

realme/ColorOS gallery "Recently deleted" storage varies between versions and has **not** been verified on hardware. It is found only if it lives in readable shared storage under one of the patterns above. If your device keeps it in app-private storage, ADB cannot read it.

## Integrity of copies

For every pulled file:

1. `sha256sum` runs on the device, if toybox provides it (Android 8+).
2. `adb pull -a` copies to a temporary name. The file is renamed only after success, and existing files are never overwritten.
3. The local SHA-256 is compared with the device hash. A mismatch becomes a warning on the result.
4. Size differences between listing and copy are reported, since they can indicate a file changed during acquisition.

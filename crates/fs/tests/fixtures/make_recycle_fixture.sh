#!/usr/bin/env bash
# Builds ntfs-recycle.img.gz: an NTFS USB disk where photos were deleted the
# three ways a Windows user deletes them. Requires root, FUSE, ntfs-3g,
# losetup and a release build of pr-bench. Run from the repo root.
#   Photo_0000/0001  Shift+Delete (permanently deleted)
#   Photo_0002/0003  normal delete: moved to $Recycle.Bin as $R…, with $I info files
#   Photo_0004       moved to the Recycle Bin, then the bin was emptied
#   Photo_0005       not deleted
set -euo pipefail
OUT=$(cd "$(dirname "$0")" && pwd)
BENCH=${BENCH:-target/release/pr-bench}
W=$(mktemp -d)
trap 'mountpoint -q "$W/mnt" && umount "$W/mnt"; [ -n "${LOOP:-}" ] && losetup -d "$LOOP"; rm -rf "$W"' EXIT
"$BENCH" jpegs --out "$W/photos" --count 6 --seed 77 --prefix Photo
truncate -s 64M "$W/disk.img"
mkntfs -F -f -q -L USBDISK "$W/disk.img" 2>/dev/null
LOOP=$(losetup -f --show "$W/disk.img")
M="$W/mnt"; mkdir "$M"; ntfs-3g "$LOOP" "$M"
BIN="$M/\$Recycle.Bin/S-1-5-21-1000"
mkdir -p "$M/Pictures/Holiday" "$BIN"
cp "$W"/photos/*.jpg "$M/Pictures/Holiday/"; sync
python3 - "$BIN" <<'PY'
import struct, sys, os
for code, name in [("AB12CD", "Photo_0002.jpg"), ("EF34GH", "Photo_0003.jpg"), ("IJ56KL", "Photo_0004.jpg")]:
    path = "E:\\Pictures\\Holiday\\" + name
    data = struct.pack("<qqqI", 2, 123456, 133700000000000000, len(path) + 1) + (path + "\0").encode("utf-16-le")
    open(os.path.join(sys.argv[1], "$I" + code + ".jpg"), "wb").write(data)
PY
H="$M/Pictures/Holiday"
rm "$H/Photo_0000.jpg" "$H/Photo_0001.jpg"
mv "$H/Photo_0002.jpg" "$BIN/\$RAB12CD.jpg"
mv "$H/Photo_0003.jpg" "$BIN/\$REF34GH.jpg"
mv "$H/Photo_0004.jpg" "$BIN/\$RIJ56KL.jpg"
rm "$BIN/\$RIJ56KL.jpg" "$BIN/\$IIJ56KL.jpg"
sync; umount "$M"; losetup -d "$LOOP"; LOOP=
gzip -9 -c "$W/disk.img" > "$OUT/ntfs-recycle.img.gz"

#!/usr/bin/env bash
# Builds exfat-card.img.gz: an exFAT memory card (mkfs.exfat + the
# exfat-fuse driver) with deleted photos and a deleted document.
# Requires root, FUSE, exfatprogs, exfat-fuse, losetup and a release
# build of pr-bench. Run from the repo root.
#   DCIM/100MEDIA/IMG_0000..0001   kept
#   DCIM/100MEDIA/IMG_0002..0003   deleted
#   DCIM/100MEDIA/IMG_0004         written into scattered free clusters (fragmented), then deleted
#   Documents/report.pdf           deleted (non-image file, for "All files" scans)
set -euo pipefail
OUT=$(cd "$(dirname "$0")" && pwd)
BENCH=${BENCH:-target/release/pr-bench}
W=$(mktemp -d)
trap 'mountpoint -q "$W/mnt" && umount "$W/mnt"; [ -n "${LOOP:-}" ] && losetup -d "$LOOP"; rm -rf "$W"' EXIT
"$BENCH" jpegs --out "$W/p" --count 5 --seed 31 --prefix IMG
head -c 30000 /dev/urandom > "$W/report.pdf"; printf '%%PDF-1.7\n' | dd of="$W/report.pdf" conv=notrunc status=none
truncate -s 8M "$W/card.img"
mkfs.exfat -c 4K -L CARD "$W/card.img" >/dev/null
LOOP=$(losetup -f --show "$W/card.img")
M="$W/mnt"; mkdir "$M"; mount.exfat-fuse "$LOOP" "$M"
mkdir -p "$M/DCIM/100MEDIA" "$M/Documents"
for i in 0000 0001 0002 0003; do cp "$W/p/IMG_$i.jpg" "$M/DCIM/100MEDIA/"; done
cp "$W/report.pdf" "$M/Documents/"
# Fill the card with 4 KiB files, then free every other one so a new
# photo has to be split across one-cluster holes.
n=0; while dd if=/dev/zero of="$M/Documents/f$n.bin" bs=4096 count=1 status=none 2>/dev/null; do n=$((n+1)); done
rm -f "$M/Documents/f$n.bin"; sync
for ((k=1; k<n; k+=2)); do rm "$M/Documents/f$k.bin"; done; sync
cp "$W/p/IMG_0004.jpg" "$M/DCIM/100MEDIA/"; sync
rm "$M/DCIM/100MEDIA/IMG_0002.jpg" "$M/DCIM/100MEDIA/IMG_0003.jpg" "$M/DCIM/100MEDIA/IMG_0004.jpg" "$M/Documents/report.pdf"
sync; umount "$M"; losetup -d "$LOOP"; LOOP=
python3 - "$W" "$OUT/exfat-card.manifest.json" <<'PY'
import hashlib, json, os, sys
w, out = sys.argv[1], sys.argv[2]
sha = lambda p: hashlib.sha256(open(p, 'rb').read()).hexdigest()
m = [{'path': f'/DCIM/100MEDIA/IMG_{k}.jpg', 'scenario': s, 'sha256': sha(f'{w}/p/IMG_{k}.jpg')} for k, s in
     [('0000', 'live'), ('0001', 'live'), ('0002', 'deleted'), ('0003', 'deleted'), ('0004', 'deleted_fragmented')]]
m.append({'path': '/Documents/report.pdf', 'scenario': 'deleted', 'sha256': sha(f'{w}/report.pdf')})
json.dump(m, open(out, 'w'), indent=2)
PY
gzip -9 -c "$W/card.img" > "$OUT/exfat-card.img.gz"

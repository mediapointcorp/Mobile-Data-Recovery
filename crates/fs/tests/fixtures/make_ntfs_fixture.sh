#!/usr/bin/env bash
# Builds ntfs-deleted.img.gz: a real NTFS volume (mkntfs + the ntfs-3g
# driver) in which photos were deleted with the actual filesystem driver.
#
# Requires Linux with root, FUSE, ntfs-3g (mkntfs, ntfs-3g, ntfsinfo,
# ntfsundelete) and a release build of pr-bench. Run from the repo root:
#   cargo build --release -p pr-bench
#   sudo crates/fs/tests/fixtures/make_ntfs_fixture.sh
#
# Scenarios (ground truth written to ntfs-deleted.manifest.json):
#   live        IMG_0000..0002  present in /DCIM/Camera
#   deleted     IMG_0003..0005  deleted, nothing written afterwards to their clusters
#   fragmented  IMG_0006        written into one-cluster holes (dozens of data runs), then deleted
#   overwritten IMG_0007        deleted, then the free space was filled with zeros
set -euo pipefail
OUT=$(cd "$(dirname "$0")" && pwd)
BENCH=${BENCH:-target/release/pr-bench}
W=$(mktemp -d)
trap 'mountpoint -q "$W/mnt" && umount "$W/mnt"; rm -rf "$W"' EXIT

"$BENCH" jpegs --out "$W/p" --count 8 --seed 11 --prefix IMG
IMG="$W/ntfs.img"
truncate -s 12M "$IMG"
mkntfs -F -f -q -c 4096 -L PHOTOS "$IMG" 2>/dev/null
mkdir "$W/mnt"
ntfs-3g "$IMG" "$W/mnt"
M="$W/mnt"
mkdir -p "$M/DCIM/Camera" "$M/Documents"

for i in 0000 0001 0002 0003 0004 0005 0007; do cp "$W/p/IMG_$i.jpg" "$M/DCIM/Camera/"; done
# Fragmentation: 120 one-cluster files, fill the rest of the disk, then
# delete every other small file so only one-cluster holes remain free.
for n in $(seq -w 0 119); do dd if=/dev/zero of="$M/Documents/f$n.bin" bs=4096 count=1 status=none; done
dd if=/dev/zero of="$M/Documents/fill1.bin" bs=4096 status=none 2>/dev/null || true
sync
for n in $(seq -w 1 2 119); do rm "$M/Documents/f$n.bin"; done
sync
cp "$W/p/IMG_0006.jpg" "$M/DCIM/Camera/"
sync
umount "$M"
echo "--- IMG_0006 runlist (must have more than one run):"
ntfsinfo -v -F /DCIM/Camera/IMG_0006.jpg "$IMG" | sed -n '/Runlist/,/End of inode/p' | head -8
ntfs-3g "$IMG" "$M"

# Overwrite: delete IMG_0007, then fill all free space with zeros.
rm "$M/DCIM/Camera/IMG_0007.jpg"; sync
dd if=/dev/zero of="$M/Documents/fill2.bin" bs=4096 status=none 2>/dev/null || true
sync
# Now delete the photos whose clusters stay untouched.
rm "$M/DCIM/Camera/IMG_0003.jpg" "$M/DCIM/Camera/IMG_0004.jpg" "$M/DCIM/Camera/IMG_0005.jpg" "$M/DCIM/Camera/IMG_0006.jpg"
sync
umount "$M"

echo "--- ntfsundelete (independent reference):"
ntfsundelete -s "$IMG" 2>/dev/null | awk '$1 >= 64 || /Inode|recoverable/' || true

python3 - "$W/p" "$OUT/ntfs-deleted.manifest.json" <<'PY'
import hashlib, json, sys, os
p, out = sys.argv[1], sys.argv[2]
sha = lambda n: hashlib.sha256(open(os.path.join(p, n), 'rb').read()).hexdigest()
scen = {'0000':'live','0001':'live','0002':'live','0003':'deleted','0004':'deleted','0005':'deleted','0006':'deleted_fragmented','0007':'deleted_overwritten'}
m = [{'path': f'/DCIM/Camera/IMG_{k}.jpg', 'scenario': v, 'sha256': sha(f'IMG_{k}.jpg')} for k, v in scen.items()]
json.dump(m, open(out, 'w'), indent=2)
PY
gzip -9 -c "$IMG" > "$OUT/ntfs-deleted.img.gz"
ls -la "$OUT"

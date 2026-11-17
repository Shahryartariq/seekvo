#!/usr/bin/env bash
# Builds the NTFS image behind small-ntfs.bin. Needs Linux, root and the
# ntfs-3g package (mkntfs and the ntfs-3g mount helper).
#
#   sudo ./make-image.sh /tmp/small-ntfs.img
#   cargo run --example make_fixture -- /tmp/small-ntfs.img tests/fixtures/small-ntfs.bin
#
# The pad files fill the volume and half of them are then deleted, so the
# file table has to grow into scattered free space and ends up in several
# pieces. That is the case worth testing.
set -euo pipefail

image=${1:?usage: make-image.sh <image file>}
mount_point=$(mktemp -d)

rm -f "$image"
truncate -s 4M "$image"
mkntfs -F -Q -q "$image" >/dev/null
ntfs-3g "$image" "$mount_point"
cd "$mount_point"

mkdir -p Docs/Reports/2026 Pads Many
printf 'hello' > Docs/a.txt
ln Docs/a.txt link-to-a.txt
printf 'x' > 'Docs/Über 📷.jpg'
printf 'q3' > Docs/Reports/2026/report-q3.pdf
dd if=/dev/zero of=big.bin bs=1000 count=300 status=none
truncate -s 5000000 sparse.bin
printf 'gone' > deleted.txt
rm deleted.txt

i=0
while dd if=/dev/zero of="Pads/pad-$i" bs=32768 count=1 status=none 2>/dev/null; do
  i=$((i + 1))
done
n=0
for pad in Pads/pad-*; do
  n=$((n + 1))
  if [ $((n % 2)) -eq 0 ]; then rm "$pad"; fi
done
for j in $(seq 1 90); do
  printf '%s' "$j" > "Many/file-$j.dat"
done
rm -rf Pads

sync
cd /
umount "$mount_point"
rmdir "$mount_point"

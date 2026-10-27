#!/bin/sh
# Builds the FAT test images in this directory (or in the directory given as $1) with
# mkfs.fat from dosfstools and mcopy from mtools, the reference tools for the on-disk format.
# The volume serial, label and every time are fixed and mkfs.fat runs with --invariant, so the
# images come out the same from run to run. The committed images are the known-good ones; CI
# also builds fresh ones with this script and runs the same tests on them (FATD_FIXTURES).
# The images and their contents are Ferric Oxide test data, under the repository's MIT
# licence.
#
# fat12.img.gz (2 MiB, 1 KiB clusters), fat16.img.gz (8 MiB, 512-byte clusters) and
# fat32.img.gz (40 MiB, 512-byte clusters, enough clusters to be FAT32), each labelled
# FERRIC, with:
#   hello.txt                  "hello from FAT\n" (an 8.3 name in lower case)
#   A Long File Name.txt       "long\n" (a long file name)
#   Ünïcødé.txt                "unicode\n" (a long name outside ASCII)
#   EFI/BOOT/BOOTX64.EFI       "MZ" and 510 more bytes, as on an EFI system partition
#   dir/nested.txt             "nested\n"
#   empty/                     an empty directory
#   big.bin                    200 KiB, byte i is (i * 7 + i / 4096) mod 251; on FAT12 and
#                              FAT16 its cluster chain is split around another file (on
#                              FAT32, mtools allocates after the FSInfo sector's hint)
#   many/f000..f199            200 small files, so the directory takes several clusters, in
#                              a chain split around the files before it
# and the deleted entries of two files the root directory held.
# Every file and directory was last written 2026-01-01 12:34:56 UTC.
set -eu
out=${1:-$(dirname "$0")}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# mtools converts names through the locale's character set, and times to local time.
export LC_ALL=C.UTF-8 TZ=UTC MTOOLS_SKIP_CHECK=1
stamp='2026-01-01 12:34:56'
src=$work/src
tree=$work/tree
mkdir -p "$src" "$tree/EFI/BOOT" "$tree/dir" "$tree/empty" "$tree/many"
printf 'hello from FAT\n' > "$tree/hello.txt"
printf 'long\n' > "$tree/A Long File Name.txt"
printf 'unicode\n' > "$tree/Ünïcødé.txt"
python3 -c 'import sys; sys.stdout.buffer.write(b"MZ" + bytes(i % 256 for i in range(510)))' \
    > "$tree/EFI/BOOT/BOOTX64.EFI"
printf 'nested\n' > "$tree/dir/nested.txt"
i=0
while [ $i -lt 200 ]; do
    name=$(printf 'f%03d' $i)
    printf '%s\n' "$name" > "$tree/many/$name"
    i=$((i + 1))
done
printf 'gap\n' > "$src/gap.bin"
python3 -c '
import sys
n = 200 * 1024
sys.stdout.buffer.write(bytes((i * 7 + i // 4096) % 251 for i in range(n)))
' > "$src/big.bin"
printf 'after\n' > "$src/after.bin"
find "$src" "$tree" -exec touch -d "$stamp" {} +

make() {
    image=$work/$1
    rm -f "$image"
    mkfs.fat --invariant -C -F "$2" -s "$3" -n FERRIC -i 0f0e0d0c "$image" "$4" >/dev/null
    m="-i $image"
    # A file, then a second after it, then the first deleted: on FAT12 and FAT16, big.bin's
    # chain starts in the gap and continues after the second file.
    mcopy -m $m "$src/gap.bin" ::/gap.bin
    mcopy -m $m "$src/after.bin" ::/after.bin
    mdel $m ::/gap.bin
    mcopy -m $m "$src/big.bin" ::/big.bin
    mdel $m ::/after.bin
    # The rest, with the directories' times (-s copies them recursively, -m keeps times).
    mcopy -s -m $m "$tree"/* ::/
    fsck.fat -n "$image" >/dev/null
    gzip -9n < "$image" > "$out/$1.gz"
}

make fat12.img 12 2 2048
make fat16.img 16 1 8192
make fat32.img 32 1 40960

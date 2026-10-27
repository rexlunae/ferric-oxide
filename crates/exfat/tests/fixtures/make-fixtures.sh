#!/bin/sh
# Builds the exFAT test images in this directory (or in the directory given as $1) with
# mkfs.exfat from exfatprogs, the reference tool for the on-disk format, and fills them
# through a mount: the kernel's exfat driver where there is one, otherwise exfat-fuse. Run
# it as root (it uses a loop device). fsck.exfat checks each image. The volume serial number
# is random, so the bytes differ from run to run, but every reader-visible fact does not.
# The committed images are the known-good ones; CI also builds fresh ones with this script
# and runs the same tests on them (EXFAT_FIXTURES). The images and their contents are
# Ferric Oxide test data, under the repository's MIT licence.
#
# exfat-4k.img.gz (8 MiB, 4 KiB clusters) and exfat-512.img.gz (16 MiB, 512-byte
# clusters), each labelled FERRIC, with:
#   hello.txt                  "hello from exFAT\n"
#   A Long File Name.txt       "long\n"
#   Ünïcødé.txt                "unicode\n" (a name outside ASCII)
#   EFI/BOOT/BOOTX64.EFI       "MZ" and 510 more bytes
#   dir/nested.txt             "nested\n"
#   empty/                     an empty directory
#   big.bin                    200 KiB, byte i is (i * 7 + i / 4096) mod 251, written in
#                              two parts with another file written between them, so its
#                              clusters are a FAT chain rather than one contiguous run
#   many/f000..f199            200 small files, so the directory takes several clusters
# and the deleted entry set of a file the root directory held. Every file and directory was
# last written 2026-01-01 12:34:56 UTC.
set -eu
out=${1:-$(dirname "$0")}
out=$(cd "$out" && pwd)
work=$(mktemp -d)
mnt=$work/mnt
mkdir "$mnt"
loop=
cleanup() {
    umount "$mnt" 2>/dev/null || true
    [ -n "$loop" ] && losetup -d "$loop" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT
export LC_ALL=C.UTF-8 TZ=UTC
stamp='2026-01-01 12:34:56'

pattern() {
    python3 -c '
import sys
start, end = int(sys.argv[1]), int(sys.argv[2])
sys.stdout.buffer.write(bytes((i * 7 + i // 4096) % 251 for i in range(start, end)))
' "$1" "$2"
}

make() {
    image=$work/$1
    truncate -s "$3" "$image"
    mkfs.exfat -L FERRIC -c "$2" "$image" >/dev/null
    loop=$(losetup -f --show "$image")
    if ! mount -t exfat "$loop" "$mnt" 2>/dev/null; then
        mount.exfat-fuse "$loop" "$mnt" >/dev/null
    fi
    printf 'deleted\n' > "$mnt/deleted.txt"
    mkdir -p "$mnt/EFI/BOOT" "$mnt/dir" "$mnt/empty" "$mnt/many"
    printf 'hello from exFAT\n' > "$mnt/hello.txt"
    printf 'long\n' > "$mnt/A Long File Name.txt"
    printf 'unicode\n' > "$mnt/Ünïcødé.txt"
    python3 -c 'import sys; sys.stdout.buffer.write(b"MZ" + bytes(i % 256 for i in range(510)))' \
        > "$mnt/EFI/BOOT/BOOTX64.EFI"
    printf 'nested\n' > "$mnt/dir/nested.txt"
    # big.bin's first half, another file's clusters after it, then the second half: the
    # file can no longer be one contiguous run, so the FAT describes its chain.
    pattern 0 102400 > "$mnt/big.bin"
    sync
    head -c 8192 /dev/zero > "$mnt/between.bin"
    sync
    pattern 102400 204800 >> "$mnt/big.bin"
    rm "$mnt/between.bin" "$mnt/deleted.txt"
    i=0
    while [ $i -lt 200 ]; do
        name=$(printf 'f%03d' $i)
        printf '%s\n' "$name" > "$mnt/many/$name"
        i=$((i + 1))
    done
    find "$mnt" -mindepth 1 -depth -exec touch -d "$stamp" {} +
    umount "$mnt"
    losetup -d "$loop"
    loop=
    fsck.exfat -n "$image" >/dev/null
    gzip -9n < "$image" > "$out/$1.gz"
}

make exfat-4k.img 4096 8M
make exfat-512.img 512 16M

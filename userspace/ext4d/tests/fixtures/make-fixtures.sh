#!/bin/sh
# Builds the ext4 test images in this directory (or in the directory given as $1) with
# mke2fs from e2fsprogs, the reference tool for the on-disk format. The contents, times,
# UUIDs and hash seed are fixed. Inode generation numbers are random, so the bytes (and the
# checksums over them) differ from run to run, but every reader-visible fact does not. The
# committed image is the known-good one. CI also builds a fresh image with this script and
# runs the same tests on it (EXT4D_FIXTURES). The images and their contents are Ferric
# Oxide test data, under the repository's MIT licence.
#
# basic.ext4: 4 MiB, 1 KiB blocks, mkfs.ext4's default features (extents, 64bit,
# metadata_csum, dir_index, a journal and so on), with:
#   hello.txt          "hello from ext4\n"
#   dir/nested.txt     "nested\n"
#   empty/             an empty directory
#   link               a symbolic link to hello.txt
#   big.bin            200 KiB, byte i is (i * 7 + i / 4096) mod 251, over several blocks
#   many/f000..f199    200 small files, so the directory is hashed (an htree)
set -eu
out=${1:-$(dirname "$0")}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
root=$work/root
mkdir -p "$root/dir" "$root/empty" "$root/many"
printf 'hello from ext4\n' > "$root/hello.txt"
printf 'nested\n' > "$root/dir/nested.txt"
ln -s hello.txt "$root/link"
python3 -c '
import sys
n = 200 * 1024
sys.stdout.buffer.write(bytes((i * 7 + i // 4096) % 251 for i in range(n)))
' > "$root/big.bin"
i=0
while [ $i -lt 200 ]; do
    name=$(printf 'f%03d' $i)
    printf '%s\n' "$name" > "$root/many/$name"
    i=$((i + 1))
done
find "$root" -exec touch -h -d '2026-01-01 00:00:00 UTC' {} +
rm -f "$out/basic.ext4"
E2FSPROGS_FAKE_TIME=1767225600 mke2fs -q -F -t ext4 -b 1024 \
    -L ferric-basic -U 6f0e1d2c-3b4a-4596-8a7b-6c5d4e3f2a10 \
    -E hash_seed=0a1b2c3d-4e5f-4607-8819-2a3b4c5d6e7f,root_owner=0:0 \
    -d "$root" "$out/basic.ext4" 4M

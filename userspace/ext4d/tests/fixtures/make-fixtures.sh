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
#   dir/private.txt    mode 0640, uid 1000, gid 100
#   dir/private-dir/   mode 0700, uid 1000, gid 100, contains secret
#   dir/*-link         links to private.txt, private-dir/secret and /
#   empty/             an empty directory
#   link               a symbolic link to hello.txt
#   big.bin            200 KiB, byte i is (i * 7 + i / 4096) mod 251, over several blocks
#   many/f000..f199    200 small files, so the directory is hashed (an htree)
set -eu
out=${1:-$(dirname "$0")}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
root=$work/root
mkdir -p "$root/dir/private-dir" "$root/empty" "$root/many"
printf 'hello from ext4\n' > "$root/hello.txt"
printf 'nested\n' > "$root/dir/nested.txt"
ln -s hello.txt "$root/link"
printf 'private\n' > "$root/dir/private.txt"
printf 'secret\n' > "$root/dir/private-dir/secret"
chmod 640 "$root/dir/private.txt"
chmod 700 "$root/dir/private-dir"
ln -s private.txt "$root/dir/private-link"
ln -s private-dir/secret "$root/dir/secret-link"
ln -s / "$root/dir/root-link"
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

# Fixed credentials allow owner/group/other checks on every host.
for name in /dir/private.txt /dir/private-dir; do
    debugfs -w -R "set_inode_field $name uid 1000" "$out/basic.ext4" >/dev/null 2>&1
    debugfs -w -R "set_inode_field $name gid 100" "$out/basic.ext4" >/dev/null 2>&1
done

# Journal replay (features.toml fs.ext4_journal_replay). Each NAME.ext4.gz is a volume that
# needs recovery: debugfs's journal_write logs transactions into its journal without
# writing them home, the state a crash leaves. NAME.replayed.ext4.gz is the same volume after
# `e2fsck -E journal_only`, e2fsprogs' replay, which the tests compare ext4d's against. Every
# volume holds hello.txt ("hello from ext4\n") and logs the same transactions, at block
# numbers found on that volume (H is hello.txt's data block, F1..F5 free blocks):
#
#   1  H = "journaled hello\n" + zeros; F1, F2 = 0x11
#   2  F3 = 0x22
#   3  revoke F2
#   4  F2 = 0x33 (logged after the revoke, so it counts)
#   5  revoke F1 (cancels transaction 1's copy: F1 stays as it was)
#   6  F4 = a block that starts with the JBD2 magic number (logged escaped)
#   7  F5 = 0x44, never committed (not replayed)
#
#   journal-v3     ext4 defaults (64bit, metadata_csum), version 3 journal checksums
#   journal-v2     ext4 without 64bit (32-bit tags), version 2 checksums
#   journal-v2-64  ext4 defaults, version 2 checksums (64-bit tags)
#   journal-ext3   ext3: an indirect-mapped journal inode, no checksums
#   journal-4k     ext4 with 4 KiB blocks (16 MiB, a 4 MiB journal), version 3 checksums
#   journal-wrap   journal-v3 with its log moved to start three blocks before the end of
#                  the journal, so it wraps round to the first log block
#
# The jbd2 crate's tests use the journal inodes of journal-v3, journal-v2 and journal-ext3
# (crates/jbd2/tests/fixtures/*.jbd2.gz, written here too when that directory is given as $2).
journal_image() {
    name=$1 blocksize=$2 size=$3 open=$4
    shift 4
    img=$work/$name.ext4
    E2FSPROGS_FAKE_TIME=1767225600 mke2fs -q -F "$@" -b "$blocksize" \
        -U 6f0e1d2c-3b4a-4596-8a7b-6c5d4e3f2a20 -E root_owner=0:0 -d "$jroot" "$img" "$size"
    hello=$(debugfs -R "bmap /hello.txt 0" "$img" 2>/dev/null)
    set -- $(debugfs -R "ffb 5 $(( $(debugfs -R stats "$img" 2>/dev/null |
        sed -n 's/^Block count: *//p') / 2 ))" "$img" 2>/dev/null | sed 's/.*: //')
    python3 - "$work" "$blocksize" <<'EOPY'
import struct, sys
work, bs = sys.argv[1], int(sys.argv[2])
def put(name, data):
    open(f"{work}/{name}", "wb").write(data)
put("j-hello", b"journaled hello\n".ljust(bs, b"\0"))
put("j-11", b"\x11" * (2 * bs))
put("j-22", b"\x22" * bs)
put("j-33", b"\x33" * bs)
put("j-44", b"\x44" * bs)
put("j-magic", (struct.pack(">I", 0xC03B3998) + b"escaped!" * bs)[:bs])
EOPY
    cat > "$work/journal.cmd" <<CMD
$open
jw -b $hello,$1,$2 $work/j-hello-11
jw -b $3 $work/j-22
jw -r $2 /dev/null
jw -b $2 $work/j-33
jw -r $1 /dev/null
jw -b $4 $work/j-magic
jw -c -b $5 $work/j-44
jc
CMD
    cat "$work/j-hello" "$work/j-11" > "$work/j-hello-11"
    E2FSPROGS_FAKE_TIME=1767225600 debugfs -w -f "$work/journal.cmd" "$img" >/dev/null 2>&1
}

replayed() {
    cp "$work/$1.ext4" "$work/$1.replayed.ext4"
    E2FSPROGS_FAKE_TIME=1767225600 e2fsck -E journal_only -y "$work/$1.replayed.ext4" >/dev/null 2>&1
    for f in "$1.ext4" "$1.replayed.ext4"; do
        gzip -9 -n -c "$work/$f" > "$out/$f.gz"
    done
}

jroot=$work/jroot
mkdir -p "$jroot"
printf 'hello from ext4\n' > "$jroot/hello.txt"
find "$jroot" -exec touch -h -d '2026-01-01 00:00:00 UTC' {} +

journal_image journal-v3 1024 2M "jo -c -v 3" -t ext4
journal_image journal-v2 1024 2M "jo -c -v 2" -t ext4 -O ^64bit
journal_image journal-v2-64 1024 2M "jo -c -v 2" -t ext4
journal_image journal-ext3 1024 2M "jo" -t ext3
journal_image journal-4k 4096 16M "jo -c -v 3" -t ext4 -J size=4

# journal-wrap: move journal-v3's log so it starts three blocks before the journal's end.
cp "$work/journal-v3.ext4" "$work/journal-wrap.ext4"
debugfs -R "blocks <8>" "$work/journal-wrap.ext4" 2>/dev/null > "$work/journal-blocks"
python3 - "$work/journal-wrap.ext4" "$work/journal-blocks" <<'EOPY'
import struct, sys
image, blocks = sys.argv[1], sys.argv[2]
bs = 1024
phys = [int(b) for b in open(blocks).read().split()]
f = open(image, "r+b")
def rd(n):
    f.seek(phys[n] * bs); return bytearray(f.read(bs))
def wr(n, data):
    f.seek(phys[n] * bs); f.write(data)
def crc32c(data):
    c = 0xFFFFFFFF
    for b in data:
        c ^= b
        for _ in range(8):
            c = (c >> 1) ^ 0x82F63B78 if c & 1 else c >> 1
    return c  # JBD2's form: not inverted at the end
sb = rd(0)
maxlen, first, start = struct.unpack(">III", sb[0x10:0x1C])
# The log is the run of blocks debugfs wrote from the first log block; every one of them
# (descriptors, data, revoke and commit blocks) has nonzero bytes, and the rest are zero.
used = 0
while any(rd(first + used)):
    used += 1
log = [rd(first + i) for i in range(used)]
for i in range(used):
    wr(first + i, bytes(bs))
new_start = maxlen - 3
for i, block in enumerate(log):
    wr(first + (new_start - first + i) % (maxlen - first), block)
sb[0x1C:0x20] = struct.pack(">I", new_start)
sb[0xFC:0x100] = bytes(4)
sb[0xFC:0x100] = struct.pack(">I", crc32c(bytes(sb[:1024])))
wr(0, sb)
EOPY

for name in journal-v3 journal-v2 journal-v2-64 journal-ext3 journal-4k journal-wrap; do
    replayed "$name"
done

if [ -n "${2:-}" ]; then
    for name in journal-v3 journal-v2 journal-ext3; do
        debugfs -R "dump <8> $work/$name.jbd2" "$work/$name.ext4" >/dev/null 2>&1
        gzip -9 -n -c "$work/$name.jbd2" > "$2/$name.jbd2.gz"
    done
fi

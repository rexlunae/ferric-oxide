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
#
# dir-64k.ext4.gz: 4 MiB, 64 KiB blocks, no journal, metadata_csum or dir_index, with
# d/a ("linked from a whole 64 KiB block\n") and d/b, a second link to it whose entry is
# alone in d's second block. That entry spans the whole block, a length of 65536 that
# rec_len's 16 bits cannot hold: mke2fs stores it as 0xFFFF in the empty block debugfs's
# expand_dir adds, and only the entry's inode, name and type are written over it here.
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

root64=$work/root64
mkdir -p "$root64/d"
printf 'linked from a whole 64 KiB block\n' > "$root64/d/a"
find "$root64" -exec touch -h -d '2026-01-01 00:00:00 UTC' {} +
img=$work/dir-64k.ext4
E2FSPROGS_FAKE_TIME=1767225600 mke2fs -q -F -t ext4 -b 65536 \
    -O ^has_journal,^metadata_csum,^dir_index \
    -U 6f0e1d2c-3b4a-4596-8a7b-6c5d4e3f2a30 -E root_owner=0:0 -d "$root64" "$img" 4M 2>/dev/null
E2FSPROGS_FAKE_TIME=1767225600 debugfs -w -R "expand_dir /d" "$img" >/dev/null 2>&1
a=$(debugfs -R "stat /d/a" "$img" 2>/dev/null | sed -n 's/^Inode: *\([0-9]*\).*/\1/p')
python3 - "$img" "$(debugfs -R "bmap /d 1" "$img" 2>/dev/null)" "$a" <<'EOPY'
import struct, sys
image, block, inode = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
f = open(image, "r+b")
f.seek(block * 65536)
entry = f.read(8)
assert entry == struct.pack("<IHBB", 0, 0xFFFF, 0, 0), entry
f.seek(block * 65536)
f.write(struct.pack("<IHBB", inode, 0xFFFF, 1, 1) + b"b")
EOPY
E2FSPROGS_FAKE_TIME=1767225600 debugfs -w -R "sif /d/a links_count 2" "$img" >/dev/null 2>&1
e2fsck -fn "$img" >/dev/null 2>&1
gzip -9 -n -c "$img" > "$out/dir-64k.ext4.gz"

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
# journal-wrap-inode: ext4 defaults with a version 3 journal whose one committed
# transaction logs hello.txt's inode table block with new times (atime 0x70000000, mtime
# 0x70000001, ctime 0x70000002, over 0x6955B900 for all three in the inode table; the copy
# is made by debugfs's set_inode_field, which also updates the inode checksum), its log moved to start two blocks before the journal's end
# so that the commit block wraps round. ext4-view does not follow it, so a read-only mount
# must read all of the inode's times one way.
#
# journal-wrap-space: a wrapped version 3 journal that allocates allocated.txt. Every
# changed block from debugfs's allocation is logged, including the volume superblock's
# free-block and free-inode counts. The physical superblock retains the old counts.
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

# wrap_log IMAGE BACK: move IMAGE's journal log so it starts BACK blocks before the
# journal's end and wraps round to the first log block.
wrap_log() {
    debugfs -R "blocks <8>" "$1" 2>/dev/null > "$work/journal-blocks"
    python3 - "$1" "$work/journal-blocks" "$2" <<'EOPY'
import struct, sys
image, blocks, back = sys.argv[1], sys.argv[2], int(sys.argv[3])
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
new_start = maxlen - back
for i, block in enumerate(log):
    wr(first + (new_start - first + i) % (maxlen - first), block)
sb[0x1C:0x20] = struct.pack(">I", new_start)
sb[0xFC:0x100] = bytes(4)
sb[0xFC:0x100] = struct.pack(">I", crc32c(bytes(sb[:1024])))
wr(0, sb)
EOPY
}

# journal-wrap: journal-v3's log, starting three blocks before the journal's end.
cp "$work/journal-v3.ext4" "$work/journal-wrap.ext4"
wrap_log "$work/journal-wrap.ext4" 3

img=$work/journal-wrap-inode.ext4
E2FSPROGS_FAKE_TIME=1767225600 mke2fs -q -F -t ext4 -b 1024 \
    -U 6f0e1d2c-3b4a-4596-8a7b-6c5d4e3f2a40 -E root_owner=0:0 -d "$jroot" "$img" 2M
# mke2fs -d takes ctime (and atime) from the clock, so pin all three before and after.
set_times() {
    for field in "atime @$2" "mtime @$3" "ctime @$4" "atime_extra 0" "mtime_extra 0" \
        "ctime_extra 0"; do
        debugfs -w -R "set_inode_field /hello.txt $field" "$1" >/dev/null 2>&1
    done
}
set_times "$img" 1767225600 1767225600 1767225600
cp "$img" "$work/times.ext4"
set_times "$work/times.ext4" 1879048192 1879048193 1879048194
itable=$(debugfs -R "imap /hello.txt" "$img" 2>/dev/null | sed -n 's/.*located at block \([0-9]*\),.*/\1/p')
dd if="$work/times.ext4" of="$work/itable" bs=1024 skip="$itable" count=1 2>/dev/null
printf 'jo -c -v 3\njw -b %s %s\njc\n' "$itable" "$work/itable" > "$work/journal.cmd"
E2FSPROGS_FAKE_TIME=1767225600 debugfs -w -f "$work/journal.cmd" "$img" >/dev/null 2>&1
wrap_log "$img" 2

# Log a real file allocation, including all of its data and metadata changes, while
# leaving their original home blocks in place until the reference replay.
img=$work/journal-wrap-space.ext4
E2FSPROGS_FAKE_TIME=1767225600 mke2fs -q -F -t ext4 -b 1024 \
    -U 6f0e1d2c-3b4a-4596-8a7b-6c5d4e3f2a60 -E root_owner=0:0 -d "$jroot" "$img" 2M
cp "$img" "$work/space.changed.ext4"
printf 'recovered allocation\n' > "$work/space.txt"
E2FSPROGS_FAKE_TIME=1767225600 debugfs -w -R \
    "write $work/space.txt /allocated.txt" "$work/space.changed.ext4" >/dev/null 2>&1
python3 - "$img" "$work/space.changed.ext4" "$work" <<'EOPY'
import pathlib, sys
before, after = [pathlib.Path(p).read_bytes() for p in sys.argv[1:3]]
work = pathlib.Path(sys.argv[3])
blocks = [i for i in range(len(before) // 1024)
          if before[i * 1024:(i + 1) * 1024] != after[i * 1024:(i + 1) * 1024]]
assert 1 in blocks, "allocation must change the volume superblock"
(work / "space.blocks").write_text(",".join(map(str, blocks)))
(work / "space.payload").write_bytes(b"".join(after[i * 1024:(i + 1) * 1024] for i in blocks))
EOPY
blocks=$(cat "$work/space.blocks")
printf 'jo -c -v 3\njw -b %s %s\njc\n' "$blocks" "$work/space.payload" > "$work/journal.cmd"
E2FSPROGS_FAKE_TIME=1767225600 debugfs -w -f "$work/journal.cmd" "$img" >/dev/null 2>&1
wrap_log "$img" 2

for name in journal-v3 journal-v2 journal-v2-64 journal-ext3 journal-4k journal-wrap \
    journal-wrap-inode journal-wrap-space; do
    replayed "$name"
done

# quota.ext4.gz: ext4 defaults with quota (user and group quota inodes, 3 and 4), 2 MiB,
# 1 KiB blocks, with hello.txt. Giving hello.txt a new owner or group would move its usage
# from one quota record to another.
img=$work/quota.ext4
E2FSPROGS_FAKE_TIME=1767225600 mke2fs -q -F -t ext4 -b 1024 -O quota \
    -U 6f0e1d2c-3b4a-4596-8a7b-6c5d4e3f2a50 -E root_owner=0:0 -d "$jroot" "$img" 2M
# mke2fs -d leaves the quota files without the usage of what it copies in; e2fsck -fy
# counts it (and exits 1, having changed the volume).
E2FSPROGS_FAKE_TIME=1767225600 e2fsck -fy "$img" >/dev/null || [ $? -eq 1 ]
E2FSPROGS_FAKE_TIME=1767225600 e2fsck -fn "$img" >/dev/null
gzip -9 -n -c "$img" > "$out/quota.ext4.gz"

# write-data.ext4.gz: ext4 defaults, 2 MiB, 1 KiB blocks, with files a data write must
# refuse: immutable.txt (i_flags IMMUTABLE), append.txt (APPEND), sparse.bin (8 KiB plus
# "tail", "head" at its start and a hole over blocks 1 to 7) and unwritten.bin (10 bytes,
# then an unwritten extent over blocks 1 to 3 that debugfs's fallocate adds).
droot=$work/droot
mkdir -p "$droot"
printf 'immutable\n' > "$droot/immutable.txt"
printf 'append only\n' > "$droot/append.txt"
printf 'unwritten\n' > "$droot/unwritten.bin"
dd if=/dev/zero of="$droot/sparse.bin" bs=1024 count=0 seek=8 2>/dev/null
printf 'tail' >> "$droot/sparse.bin"
printf 'head' | dd of="$droot/sparse.bin" conv=notrunc 2>/dev/null
find "$droot" -exec touch -h -d '2026-01-01 00:00:00 UTC' {} +
img=$work/write-data.ext4
E2FSPROGS_FAKE_TIME=1767225600 mke2fs -q -F -t ext4 -b 1024 \
    -U 6f0e1d2c-3b4a-4596-8a7b-6c5d4e3f2a60 -E root_owner=0:0 -d "$droot" "$img" 2M
debugfs -w -R "set_inode_field /immutable.txt flags 0x80010" "$img" >/dev/null 2>&1
debugfs -w -R "set_inode_field /append.txt flags 0x80020" "$img" >/dev/null 2>&1
debugfs -w -R "fallocate /unwritten.bin 1 3" "$img" >/dev/null 2>&1
E2FSPROGS_FAKE_TIME=1767225600 e2fsck -fn "$img" >/dev/null
gzip -9 -n -c "$img" > "$out/write-data.ext4.gz"

if [ -n "${2:-}" ]; then
    for name in journal-v3 journal-v2 journal-ext3; do
        debugfs -R "dump <8> $work/$name.jbd2" "$work/$name.ext4" >/dev/null 2>&1
        gzip -9 -n -c "$work/$name.jbd2" > "$2/$name.jbd2.gz"
    done
fi

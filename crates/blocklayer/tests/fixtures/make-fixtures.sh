#!/bin/bash
# Builds the block-layer test images in this directory (or in the empty directory given as
# $1) with the reference tool for each format: cryptsetup for LUKS2, lvm2 for LVM, and
# OpenSSL (through Python's cryptography package) for LUKS1 bulk data, which cryptsetup
# cannot encrypt without device-mapper. It needs root, for loop devices; device-mapper is
# not used. Every volume holds the pattern below, so the tests check decrypted and mapped
# bytes against something no Ferric Oxide code produced: cryptsetup encrypted the LUKS2
# data (`reencrypt --encrypt`, offline), OpenSSL the LUKS1 data, and the LVM data was put at
# the physical extents `lvs --segments` reported. Keys, salts and UUIDs are random, so the
# bytes differ from run to run but no fact the tests check does; the committed images are
# the known-good ones. They are Ferric Oxide test data, under the repository's MIT licence.
#
# To keep the images small, bytes no test reads are zeroed after the tools are done: the
# LUKS2 keyslot area outside keyslot 0's key material (the format does not read unused
# keyslot space), and encrypted data past what the tests read (it decrypts to noise, and
# nothing reads it).
#
# Pattern with seed s: byte i of a volume is (i * 7 + i / 4096 + s) mod 251.
# The passphrase is "correct horse" everywhere.
#
# lvm.img          a 1 MiB PV in blkvg (64 KiB extents, data from 64 KiB). LV "data" is
#                  PE 0-1 then PE 7-8, seed 4. LV "sealed" is PE 2-6 then PE 9 and holds a
#                  LUKS2 volume (aes-xts-plain64, 256-bit key, 512-byte sectors, data from
#                  256 KiB, so its 128 KiB of data straddle the jump from PE 6 to PE 9), seed 5:
#                  LUKS on LVM. Data bytes 0-8 KiB and 60-68 KiB (across the jump) are kept.
# lvm-on-luks.img  LUKS2 (aes-xts-plain64, 256-bit key, 4096-byte sectors, data from
#                  256 KiB) over a 512 KiB PV in innervg (64 KiB extents, data from 64 KiB).
#                  LV "inner" is PE 1, seed 6. The PV's first 64 KiB (label and metadata) and
#                  the LV's first 8 KiB are kept: LVM on LUKS, and the 4 KiB-sector IV check.
# luks1-data.bin   8 KiB of LUKS1 bulk data, seed 3, for the payload (sector 4096) of
#                  crates/luks/tests/fixtures/luks1.img, encrypted with that header's volume
#                  key (read back with `cryptsetup luksDump --dump-volume-key`).
set -euo pipefail
here=$(realpath "$(dirname "$0")")
out=$(realpath "${1:-$here}")
work=$(mktemp -d)
loops=()
cleanup() {
	for l in "${loops[@]}"; do losetup -d "$l" 2>/dev/null || true; done
	rm -rf "$work"
}
trap cleanup EXIT
cd "$work"
printf 'correct horse' > pass
lvmconf='devices { pv_min_size=512 }'
KEEP=8192

# pattern FILE OFFSET LEN SEED: writes the pattern for volume bytes 0..LEN at OFFSET of FILE.
pattern() {
	python3 -I - "$@" <<'PY'
import sys
path, offset, length, seed = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
with open(path, 'r+b') as f:
    f.seek(offset)
    f.write(bytes((i * 7 + i // 4096 + seed) % 251 for i in range(length)))
PY
}

# zero FILE OFFSET LEN: zeroes LEN bytes of FILE from OFFSET.
zero() {
	python3 -I - "$@" <<'PY'
import sys
path, offset, length = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
with open(path, 'r+b') as f:
    f.seek(offset)
    f.write(bytes(length))
PY
}

# dump FILE SECTION FIELD: the first value of FIELD (for example "offset:") after the line
# starting with SECTION in a LUKS2 dump.
dump() {
	cryptsetup luksDump "$1" | awk -v s="$2" -v f="$3" '$0 ~ "^"s {d=1} d && $1 == f {print $2; exit}'
}

# luks2 FILE SECTOR: encrypts FILE (already holding its plaintext followed by 512 KiB of
# room) in place, then zeroes the unused keyslot space.
luks2() {
	local file=$1 sector=$2 data area len
	cryptsetup reencrypt --encrypt -q --batch-mode --key-file pass --type luks2 \
		--reduce-device-size 512k --pbkdf pbkdf2 --pbkdf-force-iterations 1000 \
		--hash sha256 --key-size 256 --sector-size "$sector" \
		--luks2-metadata-size 16k --luks2-keyslots-size 192k "$file" >/dev/null 2>&1
	data=$(dump "$file" "Data segments:" offset:)
	area=$(cryptsetup luksDump "$file" | awk '/Area offset:/ {sub(/.*offset:/, ""); print $1; exit}')
	len=$(cryptsetup luksDump "$file" | awk '/Area length:/ {sub(/.*length:/, ""); print $1; exit}')
	# The keyslots area starts after both 16 KiB metadata copies.
	zero "$file" 32768 $((area - 32768))
	zero "$file" $((area + len)) $((data - area - len))
	echo "$data"
}

attach() { local l; l=$(losetup -f --show "$1"); loops+=("$l"); echo "$l"; }
detach() { losetup -d "$1"; loops=(); }
lvm_q() { local cmd=$1 dev=$2; shift 2; "$cmd" --config "$lvmconf" --devices "$dev" -q "$@"; }
lv_new() { lvm_q lvcreate "$1" --driverloaded n -an -Zn "${@:2}" >/dev/null 2>&1; }

# segments DEV LV: one line "first-LE first-PE last-PE" per segment of LV, as lvm2 reports it.
segments() {
	lvs --config "$lvmconf" --devices "$1" --segments --noheadings --nosuffix \
		-o lv_name,seg_start_pe,seg_pe_ranges | awk -v lv="$2" '$1 == lv {print $2, $3}' |
	while read -r le0 pe; do
		local pe0=${pe#*:} pe1=${pe#*-}
		echo "$le0" "${pe0%-*}" "$pe1"
	done
}

# geometry DEV: "pe_start extent_size" in bytes.
geometry() {
	echo "$(pvs --config "$lvmconf" --devices "$1" --noheadings --units b --nosuffix -o pe_start "$1" | tr -d ' ')" \
		"$(vgs --config "$lvmconf" --devices "$1" --noheadings --units b --nosuffix -o vg_extent_size | tr -d ' ')"
}

# copy_lv FILE DEV LV IMAGE: writes IMAGE over LV, at the physical extents lvm2 reports.
copy_lv() {
	local file=$1 dev=$2 lv=$3 image=$4 pe_start extent
	read -r pe_start extent < <(geometry "$dev")
	# dd counts in extents, so the data area must start on one.
	[ $((pe_start % extent)) -eq 0 ]
	segments "$dev" "$lv" | while read -r le0 pe0 pe1; do
		dd if="$image" of="$file" bs="$extent" skip="$le0" seek=$((pe_start / extent + pe0)) \
			count=$((pe1 - pe0 + 1)) conv=notrunc status=none
	done
}

# put_lv FILE DEV LV SEED: writes the pattern over LV, at the physical extents lvm2 reports.
put_lv() {
	local file=$1 dev=$2 lv=$3 seed=$4 pe_start extent
	read -r pe_start extent < <(geometry "$dev")
	segments "$dev" "$lv" | while read -r le0 pe0 pe1; do
		python3 -I - "$file" $((pe_start + pe0 * extent)) $((le0 * extent)) $(((pe1 - pe0 + 1) * extent)) "$seed" <<'PY'
import sys
path, at, start, length, seed = (sys.argv[1],) + tuple(int(a) for a in sys.argv[2:])
with open(path, 'r+b') as f:
    f.seek(at)
    f.write(bytes((i * 7 + i // 4096 + seed) % 251 for i in range(start, start + length)))
PY
	done
}

# first_pe DEV LV: the first physical extent of LV's first segment.
first_pe() {
	local pe
	pe=$(lvs --config "$lvmconf" --devices "$1" --segments --noheadings -o lv_name,seg_pe_ranges | awk -v lv="$2" '$1 == lv {print $2; exit}')
	pe=${pe#*:}
	echo "${pe%-*}"
}

# lvm.img: LUKS on LVM. The LUKS2 volume is the first 384 KiB of sealed.img, whose 128 KiB of
# data are what reencrypt left after the header.
truncate -s 640k sealed.img
pattern sealed.img 0 655360 5
data=$(luks2 sealed.img 512)
# Keep 8 KiB at the start of the data and 8 KiB across 64 KiB, where the LV jumps.
zero sealed.img $((data + KEEP)) $((65536 - KEEP / 2 - KEEP))
zero sealed.img $((data + 65536 + KEEP / 2)) $((393216 - data - 65536 - KEEP / 2))
truncate -s 1M lvm.img
dev=$(attach lvm.img)
lvm_q pvcreate "$dev" --dataalignment 64k --metadatasize 32k "$dev" >/dev/null
lvm_q vgcreate "$dev" -s 64k blkvg "$dev" >/dev/null
lv_new "$dev" -l 2 -n data blkvg
lv_new "$dev" -l 5 -n sealed blkvg
lvm_q lvextend "$dev" --driverloaded n -l +2 blkvg/data >/dev/null 2>&1
lvm_q lvextend "$dev" --driverloaded n -l +1 blkvg/sealed >/dev/null 2>&1
put_lv lvm.img "$dev" data 4
copy_lv lvm.img "$dev" sealed sealed.img
detach "$dev"

# lvm-on-luks.img: LVM on LUKS, with 4 KiB sectors.
truncate -s 512k inner.img
dev=$(attach inner.img)
lvm_q pvcreate "$dev" --dataalignment 64k --metadatasize 32k "$dev" >/dev/null
lvm_q vgcreate "$dev" -s 64k innervg "$dev" >/dev/null
lv_new "$dev" -l 1 -n gap innervg
lv_new "$dev" -l 1 -n inner innervg
lvm_q lvremove "$dev" --driverloaded n -f innervg/gap >/dev/null 2>&1
put_lv inner.img "$dev" inner 6
inner=$(first_pe "$dev" inner)
detach "$dev"
truncate -s 1M inner.img
data=$(luks2 inner.img 4096)
# Keep the PV's first 64 KiB (label and metadata) and the first 8 KiB of the LV.
keep_at=$((65536 + inner * 65536))
zero inner.img $((data + 65536)) $((keep_at - 65536))
zero inner.img $((data + keep_at + KEEP)) $((1048576 - data - keep_at - KEEP))
mv inner.img lvm-on-luks.img

# luks1-data.bin: OpenSSL encrypts each 512-byte sector of the payload with the volume key
# and the sector number from the start of the payload (little-endian, 16 bytes) as the XTS
# tweak: the LUKS1 specification's plain64, starting from the all-zero IV.
cp "$here/../../../luks/tests/fixtures/luks1.img" l1.img
truncate -s 4M l1.img
key=$(cryptsetup luksDump --dump-volume-key -q --key-file pass l1.img |
	awk '/^MK dump:/ {d=1; $1=""; $2=""} d {for (i=1; i<=NF; i++) if ($i ~ /^[0-9a-f][0-9a-f]$/) printf "%s", $i}')
python3 -I - luks1-data.bin "$key" "$KEEP" <<'PY'
import sys
from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes
path, key, n = sys.argv[1], bytes.fromhex(sys.argv[2]), int(sys.argv[3])
plain = bytes((i * 7 + i // 4096 + 3) % 251 for i in range(n))
with open(path, 'wb') as f:
    for s in range(n // 512):
        enc = Cipher(algorithms.AES(key), modes.XTS(s.to_bytes(16, 'little'))).encryptor()
        f.write(enc.update(plain[s * 512:(s + 1) * 512]) + enc.finalize())
PY

cp lvm.img lvm-on-luks.img luks1-data.bin "$out/"

# ferric-fat-types

Allocation-free, safe `no_std` definitions shared by FAT and exFAT readers:
boot identification constants and timestamp conversion from Microsoft's FAT 1.03
and exFAT 1.00 specifications. `userspace/fatd` uses them without enabling the
optional exFAT parser; `ferric-exfat` re-exports `time` for API compatibility.

Run `cargo test -p ferric-fat-types` from the host workspace. See the
[system I/O chapter](../../docs/src/ferric-io.md#exfat) for integration and limits.

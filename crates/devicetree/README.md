# Ferric Oxide device-tree access

`ferric-devicetree` is a `no_std`, allocation-free, safe Rust adapter around the
published `fdt = "=0.2.0-alpha3"` parser. The kernel and bootloader consume it by
path; the parser is fetched from crates.io and checked by each Cargo lockfile.
The exact prerelease avoids silently adopting another incompatible alpha API.
The parser remains an external MPL-2.0 dependency; this MIT crate does not
vendor its source.

The adapter checks header block extents before the parser performs its offset
arithmetic and validates structure/property names in fallible byte-aligned mode.
It provides borrowed nodes/properties, console aliases and suffixes, phandles,
complete ancestor traversal and bounded standard `reg`/`ranges` decoding.
Optional invalid properties return `None`; iterators stop at invalid entries.
Cell widths default independently per Devicetree Specification v0.4. Keeping
this small decoder here avoids the prerelease's zero-width division and
non-advancing numeric-error iterators; it also preserves partial-default behavior.
The upstream parser still handles binary structure, strings and tree traversal.

The firmware owner must provide a readable slice. Parsing cannot prove that a
firmware pointer is mapped, that register addresses describe real hardware, or
that every device binding is valid. Kernel reserved-memory policy stays in the
kernel, including its checked reservation-map scanner.

```sh
cargo test --locked -p ferric-devicetree
cargo clippy --locked --no-deps --all-targets -p ferric-devicetree -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps -p ferric-devicetree
cargo miri test --locked -p ferric-devicetree
```

The committed fixture is generated with:

```sh
dtc -I dts -O dtb -o crates/devicetree/testdata/firmware.dtb \
  crates/devicetree/testdata/firmware.dts
```

Its omitted size-cell property deliberately exercises the specification default,
so dtc emits an `avoid_default_addr_size` warning. The tests also reuse the kernel's
existing MMIO fixture. No dtc installation is needed to run them.

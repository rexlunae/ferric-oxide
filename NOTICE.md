# Notice

Ferric Oxide is a fork of [Redox OS](https://www.redox-os.org), originally developed at [gitlab.redox-os.org/redox-os](https://gitlab.redox-os.org/redox-os) and mirrored at [github.com/redox-os](https://github.com/redox-os).

## License

Redox OS is distributed under the MIT License. This fork keeps that license and every original copyright notice, including the `Copyright (c) 2016 Redox OS Developers` line in [LICENSE](LICENSE) and any notices in individual files and recipes. New work in this repository is also released under the MIT License, by the Ferric Oxide contributors.

Third-party software built by the recipes in this repository keeps its own license.

Components that this fork consumes as Cargo dependencies or git submodules are mirrored with full history under [github.com/rexlunae](https://github.com/rexlunae) so builds do not depend on upstream infrastructure. The mirrors keep their original MIT license and copyright notices.

## Trademarks

"Redox OS" and the Redox OS logo are trademarks of the Redox OS nonprofit. Ferric Oxide does not use them as its own name or logo, and nothing in this project implies endorsement by, or affiliation with, the Redox OS nonprofit. References to Redox OS in this repository describe where the code came from, or name upstream components (such as RedoxFS, relibc and the `redox` target triple) whose names are kept for technical compatibility.

The upstream trademark policy is published at [gitlab.redox-os.org/redox-os/redox/-/blob/master/TRADEMARK.md](https://gitlab.redox-os.org/redox-os/redox/-/blob/master/TRADEMARK.md).

## RTL8125 firmware tables

`crates/rtl8125/src/firmware.rs` contains MAC and PHY controller programs from
[Realtek's FreeBSD driver](https://github.com/alexdupre/rtl_bsd_drv/blob/cea3599aa88ba5893bb4af28974000d58326b095/if_re.c),
maintained by Alex Dupre, at commit `cea3599aa88ba5893bb4af28974000d58326b095`.
These tables retain their BSD-4-Clause terms in
[`crates/rtl8125/LICENSE-BSD-4-Clause`](crates/rtl8125/LICENSE-BSD-4-Clause).
Binary distributions containing them must reproduce that notice and disclaimer.
This product includes software developed by Bill Paul.

## GNU config.sub

`tools/cookbook/res/config.sub` is GNU `config.sub` (timestamp `2022-01-03`)
from the [GNU config project](https://savannah.gnu.org/projects/config), Copyright
1992-2022 Free Software Foundation, Inc. It is GPL-3.0-or-later with the Autoconf
exception stated in its header, which lets it be distributed with the packages
whose configure scripts use it. The cookbook's `GNU_CONFIG_GET` writes this copy
instead of downloading one.

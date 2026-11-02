# ferric-harness

Boots Ferric Oxide images in QEMU under a supervisor and drives them from the
host: the serial console (logged with host timestamps, `expect`, `serial send`),
screenshots and exact text recognition of the text consoles, keyboard and
pointer input, the boot environment (fw_cfg or typed into the bootloader's
editor), a trace mode that reads kernel trace rings from guest RAM through
`ferric-trace follow`, and measurements: boot milestones, `FERRIC_TEST_*` and
`FERRIC_BENCH` records, host vCPU thread CPU time and KVM statistics, a verdict
and `report.json`, with paired A/B comparison across runs.

Nothing runs in the guest for any of this: screen and input go through QMP,
text through the serial port QEMU already emulates.

The book chapter [Test harness](../../docs/src/ferric-harness.md) is the full
reference; [Validation and debugging](../../docs/src/ferric-testing.md) says
where it fits among the other checks.

## Quick start

```sh
cargo build -p ferric-harness
H=target/debug/ferric-harness

# A desktop image, interactively. The run directory is build/harness/<name>/,
# and build/harness/latest points at the newest run.
$H up build/x86_64/desktop/harddrive.img --mem 4096
$H expect 'login:' --timeout 300
$H serial send root && $H expect assword && $H serial send --secret password
$H serial send 'echo hi_$((6*7))' && $H expect hi_42
$H key meta_l-f2           # Super+F2: VT 2, fbcond's text login
$H text                    # the screen's text
$H shot --out vt2.png
$H pointer abs 640 400 && $H pointer button left
$H type 'root\n'
$H status
$H down                    # ACPI power button, quit after 20 s; exit 0 when the verdict passes

# A headless smoke image that powers off by itself, in one process.
$H run build/x86_64/ferric-smoke/harddrive.img --display none --smoke

# Scripted runs (see scenarios/).
$H run build/x86_64/desktop/harddrive.img \
    --scenario tools/ferric-harness/scenarios/serial-login.toml
$H run build/x86_64/wayland/harddrive.img --mem 4096 \
    --boot-env FERRIC_HARNESS_PROBE=a-b_42 --boot-env-via keys \
    --scenario tools/ferric-harness/scenarios/typed-boot-env.toml

# Reports: one or more runs, or two groups compared pair by pair.
$H report build/harness/<run>
$H report --a build/harness/a1 build/harness/a2 --b build/harness/b1 build/harness/b2
```

Trace mode (`--mode trace`) needs an image with the trace-mode kernel
(`kernel.trace_mode`) and a `ferric-trace` with the `follow` subcommand, found
with `--ferric-trace`, on `PATH` or in `target/*/`; the harness runs
`ferric-trace follow --help` before booting and stops with an error when the
subcommand is missing. The default boot environment delivery, fw_cfg, needs a
bootloader built with `bootloader.fw_cfg_env`; with other bootloaders, use
`--boot-env-via keys`. Both options are off in the catalog by default.

## Layout

| Module | What it does |
| --- | --- |
| `machine` | Options and the QEMU command line (the topology of `scripts/ferric-ci/boot-smoke.sh`) |
| `session` | A running guest: QEMU, its sockets, the monitor and sampler threads, every command |
| `control` | The supervisor's control socket (one JSON request and reply per connection, `0600`, served only to the supervisor's user) |
| `qmp`, `serial` | QMP client with an event reader; timestamped serial capture with `expect` |
| `screen`, `input` | PNG screenshots, text recognition, predicates; qcodes and pointer events |
| `bootenv`, `trace` | Boot environment (fw_cfg or keys); `ferric-trace follow` discovery and control |
| `observe`, `markers`, `verdict` | Console facts, milestones and markers; the checks and verdict |
| `stats`, `report` | Thread CPU time and KVM statistics; `report.json`, tables and paired A/B |
| `scenario` | `run --scenario` TOML steps |

## Tests

`cargo test -p ferric-harness` runs unit tests (QMP against a fake server,
serial capture over a socket pair, text recognition on screens rendered from
the font, qcode mapping, verdict cases ported from
`scripts/ferric-ci/test-boot-smoke.sh`, report parsing and A/B) and
`tests/fake_qemu.rs`, which points the harness at the test binary itself acting
as QEMU: it serves QMP and serial sockets, plays a scripted console and logs the
input it receives, so `up`, the control socket, `run --scenario`, trace mode and
`down` are tested end to end without QEMU.

`tests/real_qemu.rs` is an opt-in test that boots a real smoke image:

```sh
FERRIC_HARNESS_SMOKE_IMAGE=build/x86_64/ferric-smoke/harddrive.img \
    cargo test -p ferric-harness --test real_qemu -- --ignored
```

Add `FERRIC_HARNESS_SMOKE_IMAGE_TRACES=1` when that image's kernel was built
with `kernel.trace`: the `mode` check is then expected to fail.

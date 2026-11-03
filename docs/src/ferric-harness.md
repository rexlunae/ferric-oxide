# Test harness

`ferric-harness` (`tools/ferric-harness/`) boots a Ferric Oxide image in QEMU
and drives it from the host. It reads the serial console with host timestamps,
takes screenshots and reads the text consoles from them, sends keyboard and
pointer input, passes a boot environment to the bootloader, and records boot
milestones, test and benchmark lines, and host CPU and KVM statistics. Each run
ends with a verdict and a `report.json`, and the `report` command compares two
groups of runs pair by pair.

None of this adds code to the guest. Screen and input go through QMP, and text
goes over the serial port that QEMU already emulates. A normal boot under the
harness runs the same guest code as a boot without it.

Two parts need image options that are off by default:

- the default boot environment delivery, fw_cfg, needs a bootloader built with
  `bootloader.fw_cfg_env`, which reads the file and its `FERRIC_KERNEL` and
  `FERRIC_BOOT_RESOLUTION` keys
  ([#380](https://github.com/rexlunae/ferric-oxide/pull/380)). Other
  bootloaders ignore the file; use `--boot-env-via keys` with them;
- `--mode trace` needs an image with the trace-mode kernel (`kernel.trace_mode`,
  `usr/lib/boot/kernel-trace`, #380), which has the host-visible rings
  (`FERRIC_TRACE_HOST`), and a host `ferric-trace` whose `follow` subcommand
  reads those rings from guest RAM
  ([#381](https://github.com/rexlunae/ferric-oxide/pull/381)).

Both have run against real images on KVM. The [limits](#limits) section lists
what has run and what has not.

## The supervisor model

`ferric-harness up <image>` creates a run directory, writes the launch options
to `launch.json` and starts a supervisor, which is the same binary running in
its own process group. The supervisor starts QEMU paused and connects to the QMP
and serial sockets in the run directory. Then it resumes the guest, so no
console output is lost. It keeps these connections open for the rest of the
run:

- **Console.** Every serial byte goes to `serial.log`. Lines, with escape
  sequences and control bytes removed, go to `serial.jsonl` with the host
  `CLOCK_MONOTONIC` and wall-clock times at which their first byte arrived. A
  line that stays unfinished (a `login:` prompt) is written as a partial line,
  so commands can see prompts without waiting for a newline.
- **QMP.** Events are timestamped into `qmp-events.jsonl`. Commands can be sent
  from several threads at once.
- **Monitor.** The monitor checks each console line for milestones, test
  markers, crashes and kernel `trace:` lines. It answers the bootloader's
  resolution menu and starts `ferric-trace follow` in trace mode. It stops the
  run on a time limit or, two seconds after a crash line, so the backtrace
  reaches the log (`--on-crash continue` keeps the run going).
- **Sampler.** Every `--stats-interval` seconds (5 by default), the sampler
  records the CPU time of each QEMU thread and KVM's statistics.
- **Control socket.** `control.sock` takes one JSON request and returns one
  JSON reply per connection. Each connection gets its own thread, so a long
  `expect` does not hold up a `shot`. Only the supervisor's own user can drive
  the guest through it (see [Access](#access)).

### Access

Whoever reaches the sockets of a run can type into the guest, power it off or
take screenshots, so a run is kept to the user who started it:

- A run directory the harness creates is made `0700`. A directory that already
  existed (an empty one given with `--run`) keeps its mode; QEMU's `qmp.sock`
  and `serial.sock` then have the mode QEMU's umask gives them, so give `--run`
  a private directory.
- `control.sock` is made `0600` as soon as it is bound, before any connection
  is accepted.
- The supervisor reads each client's user with `SO_PEERCRED` and serves only
  its own effective user. Any other client, root included, gets the error
  `only uid <n> may use this run's control socket` and nothing else.
- A client must send its whole request within 10 seconds of connecting, and
  take its reply within as long, or it gets the error `no complete request
  within 10s of connecting` and is dropped. The limit covers reading the
  request, not carrying it out, so a long `expect --timeout` still works. At
  most 16 connections are served at once; another gets `the supervisor is
  already serving 16 connections; try again`. Requests are at most 1 MiB. So
  idle or repeated connections cannot use up the supervisor's threads.

The other commands are clients of the control socket. They find the run through
`--run <dir>`, then `$FERRIC_HARNESS_RUN`, then the `build/harness/latest`
link, which `up` points at the newest run. SIGINT or SIGTERM to the supervisor
stops QEMU, and the run is reported as interrupted.

`ferric-harness run <image>` does the same work in one process: boot, an
optional scenario, `down` and the report. During the run it also serves the
control socket, so you can attach `shot`, `text` or `status` to it from another
shell. Without `--scenario`, `run` waits for the guest to power off by itself,
for 300 seconds unless `--timeout` sets another limit. The smoke images work
this way.

The library behind the CLI is `ferric_harness::session::Session`; Rustdoc
covers every module.

## Commands

| Command | What it does |
| --- | --- |
| `up <image> [options]` | Starts QEMU and the supervisor, and returns once the guest runs. Prints the run directory and QEMU's PID |
| `run <image> [--scenario <toml>] [options]` | `up`, the scenario (or waiting for power-off), `down` and the report in one process. Exits 0 when the verdict passes |
| `status [--json]` | The run's state: uptime, serial counters, milestones, test counts, crash, host rings. After the run ends, it prints the verdict from `report.json` instead and exits as `down` does (see below for the JSON) |
| `shot [--out f.png]` | A QMP `screendump` as PNG, kept as `<run>/shots/shot-<n>-<t>ms.png`. `--out` also copies it to `f.png` |
| `text [--save f.png] [--json]` | Reads the text of the console on the screen (see [Screen](#screen)). `--save` also copies the screenshot to `f.png` |
| `type <text>` | Types on a US-layout keyboard. `\n` is Enter |
| `key <combo>...` | Presses key combinations in turn: `ret`, `ctrl-alt-f2`, `meta_l-f2` |
| `pointer abs <x> <y> [--raw]` | Moves the absolute pointer, in screen pixels or, with `--raw`, in QEMU axis units (0 to 0x7fff) |
| `pointer rel <dx> <dy>` | Relative motion, which ps2d ignores while it uses the absolute vmmouse |
| `pointer button <left\|right\|middle\|wheel-up\|wheel-down> [--click\|--down\|--up]` | A button click (the default), press or release |
| `serial send <text> [--eol cr\|lf\|crlf\|none]`, `serial send --secret` | Writes to the serial console, followed by a carriage return by default. With `--secret` (a password) the text is read as one line of standard input, so it is not in the command line other users can see, and is logged only as `<redacted N bytes>`. Typed at a terminal, it is not echoed: echo is off while the line is read, and comes back afterwards, also when Ctrl+C, SIGTERM or SIGHUP ends the command (the process then still ends by that signal). If the terminal's echo cannot be turned off safely (its settings cannot be read or changed, or those signals cannot be caught), the command fails before anything is typed, rather than reading the secret with echo on. Standard input that ends before a line is an error, and nothing is sent |
| `expect <regex> [--timeout s] [--since cursor\|start\|now\|<mark>]` | Waits for a multi-line regular expression on the console. By default the search starts just past the previous match; every match, even one found by searching from an earlier `--since` point, sets where the next default search starts |
| `mark <name>` | Records a named time and console offset in the report. `expect --since <name>` starts the search at the mark |
| `wait-screen <predicate> [--timeout s]` | Waits until the screen matches `text:<s>`, `regex:<re>`, `changed`, `stable[:<ms>]` or `nonblank` |
| `trace start` / `trace stop` | Starts or stops `ferric-trace follow` (trace mode). Each start writes a new capture directory |
| `stats` | Samples thread CPU time and KVM statistics now, and prints them as JSON |
| `down [--timeout s]` | Presses the ACPI power button, waits (20 seconds by default, `--powerdown-timeout` at `up`) and then makes QEMU quit. Writes the report and exits 0 when the verdict passes |
| `report <run>... [--a <run>... --b <run>...] [--json]` | Prints runs side by side, and a paired A/B comparison of two groups |

`status --json` prints one object. While the supervisor runs, it holds `name`,
`run_dir`, `image`, `mode`, `qemu_pid`, `running` (whether QEMU still runs),
`uptime_ms`, `milestones`, `tests` (passed, skipped, failed), `crash`,
`kernel_trace_lines`, `serial`, `host_rings`, `tracing`, `stop`, `verdict`
(`null` until the run ends, then `pass` or `fail`) and `expect_cursor`. Once the
run has ended, it holds `name`, `run_dir`, `running` (`false`) and `verdict`
with the same meaning, the verdict's `checks` as in `report.json`, and `report`,
the path of `report.json`; the `report` key tells the two apart.

Exit status: 0 for success, 1 for a failed verdict, an `expect` or
`wait-screen` timeout, or a guest that has gone, and 2 for other errors (bad
arguments, a missing image or firmware, no supervisor). Errors print with their
full chain of causes.

### Launch options

`up` and `run` share these options:

| Option | Default | Meaning |
| --- | --- | --- |
| `--run <dir>`, `--name <name>` | `build/harness/<image>-<date>-<time>` | Run directory. It must be empty or absent |
| `--mode normal\|trace` | `normal` | The kernel to boot (see [Modes](#modes)) |
| `--display std\|none` | `std` | A VGA framebuffer read through QMP, or no display, like CI's smoke boots. Without a display the bootloader shows no menu |
| `--boot-env KEY=VALUE` | none | A boot environment entry. Repeat for more |
| `--boot-env-via fw-cfg\|keys` | `fw-cfg` | How the boot environment reaches the bootloader |
| `--trace-ring-kib <n>` | 4096 | Host-visible ring size per CPU in trace mode: a power of two from 4 to 1048576 KiB, the sizes the kernel accepts (`ferric_trace::host::RingSize`). Any other value is rejected before QEMU starts. Use 65536 for system-call events under load (see [Limits](#limits)) |
| `--ferric-trace <path>`, `--no-trace-autostart` | `PATH`, then the newest `target/*/ferric-trace` | `follow` binary, and whether to start it when the rings are announced |
| `--smp`, `--mem` | 4, 2048 MiB | vCPUs and guest RAM |
| `--accel auto\|kvm\|tcg`, `--cpu <model>` | `auto` | KVM with `host,migratable=off` when `/dev/kvm` opens read-write, otherwise TCG with `max` |
| `--input ps2\|usb` | `ps2` | `usb` adds a USB tablet and keyboard |
| `--nic e1000\|virtio`, `--iommu none\|vtd\|amdvi` | `e1000`, `none` | Network card and emulated IOMMU, as in `boot-smoke.sh` |
| `--extra-disk <img>` | none | More NVMe disks, also opened with `snapshot=on` |
| `--reboot` | off | Let the guest reset. Only `UNHANDLED EXCEPTION` then counts as a crash |
| `--timeout <s>`, `--on-crash stop\|continue` | none, `stop` | Run time limit, and what a crash line does |
| `--smoke` | off | Requires the smoke protocol: `FERRIC_SMOKE_OK` and at least one test result |
| `--stats-interval <s>` | 5 | Seconds between statistics samples. 0 samples only at the end |
| `--firmware <file>` (`OVMF_FIRMWARE`) | searched | UEFI firmware. Without one, the harness searches the paths that `mk/qemu.mk` searches |
| `--qemu`, `--qemu-arg`, `--qemu-wrapper-arg` | `qemu-system-x86_64` | QEMU program, extra arguments, and arguments for a wrapper program |
| `--key-delay-ms <n>` | 15 | Delay between keyboard events. The PS/2 controller holds only a few bytes |

### QEMU profile

The machine follows `scripts/ferric-ci/boot-smoke.sh`, so the guest sees the
same PCI addresses and that script's log checks still apply. It is q35 with the
image as NVMe at `00:04.0` (`snapshot=on`, so the image is never modified), the
NIC at `00:05.0` on user networking with a private TFTP root, QEMU's `edu`
device at `00:10.0` and two HDA controllers. With a display, the machine also
gets `-vga std` and a `qemu-xhci` controller, as `make qemu` does, but no
window: you read the screen through QMP. The QMP socket
(`qmp.sock`) and the serial socket (`serial.sock`) are in the run directory.
QEMU's `TMPDIR` is `<run>/tmp`, so the snapshot overlay stays out of `/tmp`.
Unix socket paths are limited to 107 bytes, and the harness rejects a run
directory whose sockets would not fit.

## Scenarios

`run --scenario <file>` runs TOML steps in order. The first failing step ends
the scenario, and the run is then brought down and judged. The verdict's
`scenario` check fails if any step failed. If the guest powers off during the
scenario, the step that needed it fails (`the guest has exited`), and the
report is written only once the scenario has ended, so that failure is always
in it. Each step has one action and an
optional `timeout` in seconds. Without one, a step uses the file's `timeout`,
or 60 seconds if the file sets none. The harness checks every pattern,
predicate, key name and typed character before it boots the guest, and
rejects a `sleep` longer than its step's timeout.

```toml
description = "log in on the serial console"
timeout = 120

[[step]]
expect = "login: $"        # regex on the console; `since` = "start", "now" or a mark
timeout = 300
[[step]]
send = "root"              # serial input; `eol` = "cr" (default), "lf", "crlf", "none"
[[step]]
send-secret = "password"   # like send, but logged and reported as <redacted 8 bytes>
[[step]]
type = "echo hi\n"         # keyboard
[[step]]
key = ["meta_l-f2"]        # a combination or a list of them
[[step]]
pointer = { abs = [640, 400] }                    # `raw = true` for axis units
[[step]]
pointer = { button = "left", action = "click" }   # or "down", "up"; or rel = [dx, dy]
[[step]]
wait = "text:login:"       # a wait-screen predicate
[[step]]
text = "vt2.txt"           # read the screen into <run>/vt2.txt (a plain file name); `true` only logs it
contains = "redox login:"  # optional: fail unless the text contains this
[[step]]
shot = "vt2.png"           # a file name in <run>/shots; `true` names it automatically
[[step]]
sleep = 2.5                # seconds; no longer than the step's timeout
[[step]]
mark = "workload"
[[step]]
trace-start = true         # also trace-stop = true
[[step]]
wait-exit = true           # wait for the guest to power off by itself
```

`tools/ferric-harness/scenarios/` contains two scenarios. `serial-login.toml`
logs in on the serial console, checks a command's output and reads VT 2.
`typed-boot-env.toml` checks that a typed boot environment entry reaches
`/scheme/sys/env`. Both send the password with `send-secret`. A host test
parses every shipped scenario.

## Run directory and report

| File | Content |
| --- | --- |
| `launch.json` | Every launch option. The supervisor starts from this file |
| `qemu-args.txt`, `qemu.log` | QEMU's command line (one argument per line) and its output |
| `supervisor.log` | The supervisor's own log (`up` only) |
| `serial.log`, `serial.jsonl` | Raw console bytes, and console lines with `mono_ns`, wall time and text |
| `serial-input.jsonl` | Text the harness sent to the serial console, with timestamps. Secret text (`serial send --secret`, `send-secret`) appears only as `<redacted N bytes>`, N being its length without the line ending |
| `qmp-events.jsonl` | QMP events, with timestamps |
| `stats.jsonl` | Periodic statistics samples |
| `shots/` | Screenshots, including the ones `text` takes |
| `boot-env` | The fw_cfg boot environment file (absent when typing it) |
| `trace/`, `trace.2/`, ... | Host trace captures (trace mode) and `trace-follow.log` |
| `tftp/`, `tmp/` | The user network's TFTP root, and QEMU's temporary directory |
| `control.sock`, `qmp.sock`, `serial.sock` | Sockets, which are removed when the run ends |
| `report.json` | The report |

The report (`schema` 1) has these fields:

- `run`: name, directory and start time.
- `environment`: the git commit and whether the tree was dirty, QEMU's version,
  accelerator, CPU model and full command line, the host CPU, and the image's
  path and size.
- `options`: the launch options.
- `verdict`: the result and each check with its status (`pass`, `fail`, `warn`
  or `skip`) and detail. The checks are `stop` (how the run ended), `exit`
  (QEMU's exit status), `crash`, `tests` (no `FERRIC_TEST_FAIL`), `smoke` (with
  `--smoke`), `mode`, `trace` (see [Modes](#modes); `skip` in normal mode),
  `shutdown` (the SHUTDOWN event's reason) and `scenario`. A warning does not
  fail the run. The crash, loader and smoke rules are those
  of `boot-smoke.sh`, and its self-test cases are ported to the harness's
  tests.
- `ending`: why and when the run stopped, QEMU's exit status, and how long
  power-off took.
- `facts`, `milestones`, `markers`, `bench`, `marks`: what the console showed.
  Milestones are host milliseconds since QEMU started: `qemu_start`,
  `guest_start`, `bootloader`, `bootloader_menu`, `kernel` (first kernel line),
  `init`, `login` (first `login:`), `smoke_ok` and `shutdown`. `markers` holds
  `FERRIC_TEST_PASS|SKIP|FAIL` and the smoke markers, and `bench` holds
  `FERRIC_BENCH` records, all with their times.
- `serial`, `qmp_events`: line and byte counts, and the number of events of
  each kind.
- `stats`: the run's totals. These are QEMU's total CPU time, per-vCPU host
  thread CPU time (user and system) with KVM's per-vCPU counters (exits, halt
  exits, I/O and MMIO exits, interrupt injections and so on), the VM-wide
  counters, and the CPU time of every other QEMU thread.
- `trace`: the host-ring announcement, each capture with the exit status of
  `follow` and whether its directory was whole (`files`: an `INCOMPLETE`
  marker, a `stats.end`), and why an automatic start of `follow` failed
  (`start_failures`), in trace mode only.
- `scenario`: each step with its start time, duration and detail (scenario runs
  only). A `send-secret` step is described as `send <redacted N bytes>`.

## Modes

The [tracing chapter](ferric-tracing.md) explains why tracing that is compiled
in costs time even while it is off. Trace mode is therefore a second kernel
binary, and `--mode` chooses between the two kernels:

- **`--mode normal`** boots the image's normal kernel. The `mode` check fails
  if any kernel line starts with `trace:`, because a kernel built without
  `kernel.trace` prints none.
- **`--mode trace`** adds `FERRIC_KERNEL=trace` (load
  `usr/lib/boot/kernel-trace`) and `FERRIC_TRACE_HOST=<--trace-ring-kib>` to
  the boot environment, and `FERRIC_TRACE=sched` unless `--boot-env` gives a
  `FERRIC_TRACE` (`--boot-env FERRIC_TRACE=` turns no events on at boot). The
  trace-mode kernel cannot take events from `kernel.trace_boot`, which needs
  `kernel.trace` in the normal kernel, so without this nothing would be
  recorded until the guest enabled events. `sched` is what CI's traced smoke
  image turns on with `kernel.trace_boot = "sched"`, and the smoke tests
  (`ferric-trace-test`) expect `sched.switch` on from boot. It also backs guest RAM with
  `/dev/shm/ferric-harness-<run>` (`memory-backend-file`, `share=on`). The
  `mode` check fails unless the kernel logs `trace: trace-mode kernel`. When the
  kernel logs `trace: host rings at phys 0x<hex>, <n> CPUs x <KiB> KiB`, the
  harness runs `ferric-trace follow --guest-ram <file> --phys 0x<hex>
  --ram-mib <mem> --out <run>/trace --live`. `follow` maps the RAM file
  read-only and writes an ordinary capture directory, which `ferric-trace text`,
  `counts`, `perfetto` and `otlp` read, plus a `host-clock` file for lining up
  guest and host time. After QEMU exits, the harness sends `follow` SIGINT so
  it flushes, and then removes the RAM file. Before booting, the harness checks
  that `ferric-trace` has a `follow` subcommand
  (`ferric-trace follow --help`); a missing binary or subcommand is an error.

  The `trace` check fails a trace run without a usable trace. The kernel must
  have announced its host rings; it does not when it refused the ring size or
  could not allocate the rings. With automatic start (the default), `follow`
  must have started (a failure to start it is logged and fails the check),
  and every capture must be whole: `follow` exited with 0 after SIGINT, without
  being killed, and its directory has `stats.end` and no `INCOMPLETE` marker.
  With `--no-trace-autostart` the announcement is enough, but a capture started
  with `trace start` is held to the same rule. The detail names what is
  missing.

## Boot environment

The bootloader passes `KEY=VALUE` lines to the kernel, which serves them as
`/scheme/sys/env`. The harness adds the mode's keys and the `--boot-env`
entries in one of two ways:

- **fw_cfg** (the default). The lines are written to `<run>/boot-env` and
  passed with `-fw_cfg name=opt/org.ferric-oxide/boot-env,file=<run>/boot-env`.
  A bootloader built with `bootloader.fw_cfg_env` merges them before its
  resolution menu. When the run has a display, the harness also sets
  `FERRIC_BOOT_RESOLUTION=auto`, so such a bootloader skips the menu. Older
  bootloaders ignore the file.
- **keys** (`--boot-env-via keys`). When the resolution menu appears, the
  harness presses `e`, types each line into the boot environment editor with
  Enter, and boots with Enter on the empty line. This works with any bootloader
  that has a display, and it needs one: `--display none` is rejected. Every
  typed line must be printable text with a US-layout key: an entry with any
  other character (such as `é`, a tab or an escape) is rejected before the
  guest boots, naming the key and the character, because the editor would not
  receive it as typed. fw_cfg carries such values unchanged.

In either case the harness answers each resolution menu it sees on the console
with Enter. So images whose bootloader has no fw_cfg support still boot without
a person. Keys must be letters, digits and `_`. Values cannot contain line
breaks. The file may be at most 4 KiB, the most the bootloader reads. A
`FERRIC_KERNEL` entry that contradicts `--mode` is rejected, and so is a
`FERRIC_TRACE_HOST` entry with a ring size the kernel would refuse (it would
boot without host rings). The key names, the fw_cfg file name, the size limit
and the key rule come from `ferric_abi::boot_env`, which the bootloader and
kernel use too.

## Input and screen

### Input

Keyboard input is sent as QMP `input-send-event` key down and up pairs with
QEMU qcodes, one event at a time. Text is typed on a US layout, inputd's
default keymap, with Shift for capitals and symbols. A combination such as
`ctrl-alt-f2` presses its keys in order and releases them in reverse order.
`enter`, `super`, `space` and single characters are accepted as aliases. On
q35 the keyboard is the PS/2 keyboard (ps2d, or the firmware's driver in the
bootloader). The pointer is QEMU's vmmouse, which ps2d switches to absolute
mode, so `pointer abs` and button events reach it. Relative motion goes to the
PS/2 mouse, which ps2d ignores while vmmouse is in use. `--input usb` routes the
same events through xhcid and usbhidd as well.

### Screen

`shot` saves the framebuffer from QMP `screendump` as a PNG.

QEMU writes screenshots only in `<run>/shots/`. A control request names the
file, never a path: `shot` and `text` requests take a plain file name (no `/`,
not `.` or `..`) and are refused with any other, as are such names in a
scenario's `shot` step. `shot --out` and `text --save` write the copy
themselves, in the client, so a file outside the run directory is written with
the client's own permissions, not by QEMU.

`text` reads text without general OCR. fbcond, fbbootlogd and the kernel's
graphical debug output draw every character as an 8x16 cell in two colours,
using the glyphs of orbclient's `FONT`. The harness takes each cell's two
colours and looks the resulting bitmap up among the font's glyphs, so the text
it returns is exact. The cursor is the same cell with its colours swapped and
reads as the character under it. A cell of one colour reads as a space. Cells
that match no glyph (a desktop, an image) count as unknown. A console that uses
a PSF font from `/etc/fbcond.toml` cannot be read this way. The font is GNU
Unifont as shipped in the MIT-licensed orbclient crate, under the GPL with the
font-embedding exception. The harness uses it from that crate, as console-draw
does, and does not copy it.

`wait-screen` takes a screenshot, pausing 200 ms between them, until its
predicate holds. `changed` compares each screenshot with the first one, and
`stable:<ms>` requires no change for that long (1000 ms for a bare `stable`).

## Measurement and A/B

`report` flattens each report into named numbers:

- `milestone.<name>`;
- `bench.<series>.<field>`, from the first `FERRIC_BENCH` line of each series.
  The series is the line's name and the words before its first number, such as
  `iobench-randread-4k-qd1[policy=main]`. `FERRIC_BENCH_SAMPLE` lines are
  rounds, not results;
- `stats.qemu_cpu_ms` and `stats.vcpu_cpu_ms`;
- `kvm.<counter>`, summed over vCPUs.

With several runs, `report` prints the median, minimum and maximum of each
metric. With `--a` and `--b`, it compares the groups the way
`scripts/ferric-ci/check-trace-overhead.sh` does. The Nth runs of A and B are a
pair, ideally two boots run back to back that see about the same host load.
`report` computes each pair's percentage difference and reports the median of
those differences, so one unlucky pair cannot decide the result. A metric that
is missing from any run is listed as incomplete rather than judged on fewer
runs.

```sh
for i in 1 2 3 4 5; do
  ferric-harness run a.img --display none --smoke --name a$i
  ferric-harness run b.img --display none --smoke --name b$i
done
ferric-harness report --a build/harness/a{1..5} --b build/harness/b{1..5}
```

## Tests

`cargo test -p ferric-harness` runs these checks:

- unit tests: QMP against a fake server, serial capture over a socket pair,
  text recognition on screens rendered from the font, qcode mapping, the
  verdict cases from `test-boot-smoke.sh`, and report parsing and A/B
  comparison;
- `tests/fake_qemu.rs`, which uses the test binary itself as QEMU. It serves
  the QMP and serial sockets, plays a scripted console and logs the input it
  receives, so `up`, the control socket (with its idle-client timeout and
  connection limit), `run --scenario`, typed boot
  environments, trace mode with a stand-in `follow`, and `down` are tested end
  to end;
- `tests/real_qemu.rs`, an ignored test that boots a real smoke image:

  ```sh
  FERRIC_HARNESS_SMOKE_IMAGE=build/x86_64/ferric-smoke/harddrive.img \
      cargo test -p ferric-harness --test real_qemu -- --ignored
  ```

  If that image's kernel was built with `kernel.trace`, also set
  `FERRIC_HARNESS_SMOKE_IMAGE_TRACES=1`. The `mode` check is then expected to
  fail.

`host-tools.yml` runs these tests with the workspace tests, along with strict
Clippy and Rustdoc for the crate.

## Limits

These runs were made on 1 November 2026 with KVM, QEMU 10.2.1 and OVMF, on 4
vCPUs, against images built from main before #380 and #381 were merged. Their
bootloaders have no fw_cfg support, and their kernels were built with
`kernel.trace`.

- **`ferric-smoke`, headless, 2 GiB RAM** (`run --display none --smoke`). The
  guest booted, ran the smoke tests (163 passed, 19 skipped, 0 failed), printed
  `FERRIC_SMOKE_OK` and powered off. The report recorded milestones (first
  kernel line after about 0.9 s, `login:` after 5.6 s, `FERRIC_SMOKE_OK` after
  121 s), per-vCPU thread CPU time and KVM counters. The `mode` check failed,
  as intended: the kernel printed `trace:` lines.
- **`wayland`, 4 GiB RAM, with a display.** These steps worked:
  - logging in on the serial console as `root`;
  - a boot environment entry typed into the bootloader editor
    (`--boot-env-via keys`), which appeared in `/scheme/sys/env`;
  - `meta_l-f2` switching to VT 2, and `text` reading its login screen;
  - the absolute pointer moving the cursor, and a click that switched the
    libcosmic demo to its second page.

  The guest did not power off within 20 seconds of the ACPI power button, so
  `down` made QEMU quit, and the `shutdown` check recorded a warning.
- **`wayland`, 2 GiB RAM.** The kernel panicked on a user page fault that could
  not get a frame
  ([#372](https://github.com/rexlunae/ferric-oxide/issues/372)). The harness
  saw the panic, stopped the run and failed the `crash` check.

On 2 November 2026 the same host ran images that include #380 and #381, with the
catalog defaults plus `kernel.trace_mode = true` and
`bootloader.fw_cfg_env = true` (so the normal kernel has no `kernel.trace`):

- **`ferric-perf`, 2 GiB RAM, normal and trace mode, booted at the same time.**
  Both passed every check in about 11 seconds. In normal mode the kernel
  printed no `trace:` line. In trace mode the bootloader took
  `FERRIC_KERNEL=trace` and `FERRIC_TRACE_HOST` from fw_cfg and loaded
  `kernel-trace`, which logged `trace: trace-mode kernel` and its host rings at
  `0x900000`. `follow` wrote the capture while the guest ran and stopped
  cleanly when it powered off.
- **Ring size.** With `FERRIC_TRACE=syscall,sched`, the benchmarks make
  millions of system calls a second. Each costs about 88 bytes of trace, which
  fills a 4 MiB ring in about 15 ms. At the default 4096 KiB the kernel
  overwrote about 4.6 million records `follow` had not read (`stats.end`); with
  `--trace-ring-kib 65536` it lost none (427 MB captured). The default suits
  scheduler and scheme events. Use 65536 for system-call events under load,
  and leave room in the guest's RAM for it (4 CPUs take 256 MiB).
- **`ferric-smoke` in normal mode**: 55 passed, 32 skipped, 0 failed (the
  trace tests skip: the normal kernel has no `kernel.trace` scheme).
- **`ferric-smoke` in trace mode**, with `--boot-env FERRIC_TRACE=sched`
  (the smoke tests expect `sched.switch` on from boot, as CI's `trace.boot`
  gives them; trace mode now sets it by default, #389) and
  `--trace-ring-kib 16384`: 74 passed, 34 skipped, 0 failed.
  The guest's own trace tests drained the host rings while `follow` captured
  them from the host with nothing lost. Only the steps that map the rings in
  the guest (`trace-mmap`, `trace-mapping`, `trace-clock`, `trace-capture`)
  skip, because host rings cannot be mapped
  ([#378](https://github.com/rexlunae/ferric-oxide/issues/378)).

These parts have not run against a real guest yet: `--input usb`, the
`vtd`/`amdvi` IOMMU options, and `report --a/--b` on runs of different images.
Only x86_64 guests are supported.

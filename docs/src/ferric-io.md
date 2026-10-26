# Registered buffers, IOMMUs and rings

The implemented I/O extensions expose registered memory and DMA domains
through kernel schemes. Their ABI definitions live in shared crates so the
kernel, bootstrap, drivers and tests use the same paths and request encoding.

## Registered memory and grants

`kernel.regbuf = true` enables `/scheme/kernel.regbuf`; the default is `false`.
`crates/regbuf/src/abi.rs` defines region/grant paths and requests;
`crates/kernel/src/scheme/regbuf.rs` supplies the kernel implementation.

1. An application opens `region` beneath the scheme root and registers a
   page-aligned range of read-write anonymous memory. Private anonymous and
   `MAP_SHARED` anonymous memory are accepted. File-backed private mappings
   and memory merely borrowed from another process are refused.
2. Registration pins the frames and charges a pin budget to the region
   opener's user ID. Non-root users are limited to 16 MiB each; all users,
   including root, share a total limit of one quarter of memory. Exceeding the
   budget returns `EDQUOT`.
3. Opening a `grant/ro/<offset>/<length>` or `grant/rw/<offset>/<length>` path
   relative to the region creates a descriptor granting only that byte range.
   Offsets and lengths are hexadecimal; use `GrantSpec` to encode them.
4. The application passes the grant descriptor to a service. The holder maps
   it with `MAP_SHARED`; writable mappings require a read-write grant.
5. The owner can revoke a grant through the region. The holder can drop a
   mapping with `Request::Unmap`, or close its grant. Closing the region
   revokes its grants and releases frames only when every mapping is gone.

The mapped whole pages refer directly to the registered frames. A partial
first or last page uses a bounce page containing only the granted bytes, so
bytes outside the grant are not exposed. On teardown, writable bounce pages
copy back only bytes changed from their initial snapshot. That preserves
newer writes by the owner or another grant to untouched bytes; it does not
provide transactional conflict resolution for concurrent writes to the same
byte. The portable rule is in `crates/regbuf/src/bounce.rs`.

A grant holder cannot independently `munmap`, `mprotect` or `mremap` the
pinned mapping (`EBUSY`), or re-share/register it to escape revocation
(`EPERM`). A fork of the holder does not inherit that mapping. Forking the
region owner copies private frames for its child while the region retains the
owner's frames; `MAP_SHARED` frames retain their usual sharing semantics.

Explicit sub-grants let a holder delegate part of its range with equal or
less access. They are opened relative to the parent grant using the `sub`
path encoding. The table permits eight levels of ancestry. Revoking an
ancestor revokes its descendants even if an intermediate descriptor was
closed. Closing an individual grant descriptor does not itself revoke its
descendants. Per-request borrowed buffers can still be forwarded through
scheme requests without becoming permanent grants; the memory capture path
undoes partial forwarding mappings on failure.

## Strict revocation and failed teardown

The grant table in `crates/regbuf/src/table.rs` records both CPU and DMA
mappings. A teardown takes records under the region lock, performs hardware
unmapping after releasing it, then reports success or restores a failed
record. This avoids holding the region lock across other address-space or
domain operations and makes concurrent unmaps visible as work in flight.

A successful `Revoke` waits until all mappings of the selected grant and its
descendants are gone, including work started by another thread. CPU teardown
includes TLB shootdowns; DMA teardown includes completed IOTLB invalidation.
New mappings of revoked grants fail with `ECANCELED`.

A revoke, or the region's close, takes the registered frames away from a
holder at a moment the holder did not choose. So a holder's CPU mapping is
not unmapped but replaced in place, under one hold of the holder's address
space lock, with zeroed private anonymous memory of the same protection (the
`PROT_READ`, `PROT_WRITE` and `PROT_EXEC` bits the holder mapped it with, so a
`PROT_NONE` mapping stays inaccessible). The frames are unpinned and the TLBs shot down as before, so the holder can no
longer see or change the owner's memory. But the range stays mapped: the
holder reads zeros there, its writes go to its own private pages, and it never
faults on memory it was granted. A driver that keeps a grant mapped (such as
a kernel-bypass queue socket's UMEM in `driver-network`) can therefore check
`GrantInfo::revoked` once per tick and stop serving the grant, without a
revoke between the check and an access crashing it. The replacement memory
is the holder's to `munmap` like any other. When the holder itself lets go,
with `Request::Unmap` or by closing its grant, the mapping is unmapped as
before. `crates/regbuf/src/table.rs` records why each mapping is taken down
(`Teardown`), and its host tests cover which operations keep the range.

If teardown fails, the request reports an error such as `EIO`; it does not
promise revocation completed. The mapping stays recorded for retry, and its
frames remain pinned. Closed regions with outstanding mappings are retained
on the kernel's retry list, keeping their pin charge until drained. A later
close/revoke retries them. Registration racing region closure refunds its
charge and cannot install frames into the closed region. The DMA backend has
additional failure and domain-close limitations described below; the region
table's retry bookkeeping alone does not resolve them.

## IOMMU policies and vendor support

`kernel.iommu` selects an x86-64 policy:

| Policy | Behavior |
| --- | --- |
| `never` (default) | Leaves IOMMU support inactive; no DMA-domain isolation from this feature |
| `auto` | Enables supported firmware-described units, logs unsupported ones, exposes domains; unbound processes retain physical translations and never-attached devices use the compatibility identity domain |
| `enforce` | Requires a bound domain with an attached device for DMA translation; unbound translations fail with `EACCES`, and unattached devices are blocked |

The kernel selects Intel VT-d from ACPI DMAR, or AMD-Vi from IVRS when using
that vendor path. The host-testable vendor crates parse firmware tables and
encode registers, page tables and invalidation commands. Kernel modules under
`arch/x86_64/iommu/` map hardware registers, allocate tables, drive queues and
report faults. `crates/iommu/` supplies common table walking, rings, IOVA
allocation, requester aliases and mapping lifecycle rules.

Firmware-reserved DMA ranges are retained: VT-d RMRRs and applicable AMD-Vi
IVMD ranges receive identity mappings. A domain is therefore constrained by
the platform's required reserved mappings as well as its driver's mappings.
PCI bridges and aliases are resolved so a device's possible requester IDs are
attached together. AMD-Vi tracks shared aliases per domain to prevent takeover
or premature blocking while another function still uses them.

Intel units with the required interrupt-remapping and queued-invalidation
support get interrupt-remapping tables. `pcid` requests MSI/MSI-X entries with
requester validation, and the kernel routes the I/O APIC through remapping.
On those units compatibility-format interrupts are blocked. AMD-Vi currently
implements DMA remapping only and uses compatibility-format interrupts.
Neither the `enforce` policy nor DMA tests establish AMD interrupt isolation.

## Per-CPU interrupt delivery

The `irq:` scheme lets a driver reserve vector `N + 32` on one CPU by
opening `irq:cpu-XX/N` with `O_CREAT`. Upstream counts interrupts and wakes
handles by vector number only, so the same vector held on two CPUs shows up
on both handles. With `kernel.irq_percpu` (off by default, x86_64 only):

- Interrupts are keyed by (logical CPU, vector). Each CPU increments its own
  row of atomic counters (`ferric_irq::Counts`), without a global lock, and
  only handles opened for that CPU and vector are woken. Interrupts from the
  PIC or I/O APIC belong to the BSP's `irq:N` handles. On an AP, IRQ numbers
  0 to 15 are ordinary vectors and can be reserved there.
- The top-level listing names CPUs by logical id, the number `cpu-XX` takes.
- The root-only file `irq:cpus` lists each online CPU as
  `cpu=<logical hex> apic=<APIC id> node=<NUMA node or ->`. `ferric-irq`
  holds the writer and parser, so user space reads it with
  `ferric_irq::parse_cpus`. The file appears in directory listings of
  `/scheme/irq`.
- Without interrupt remapping, pcid looks up the APIC id of the CPU a driver
  asks for in `irq:cpus` when it builds a compatibility-format MSI message,
  instead of using the logical id as one. It refuses a CPU whose APIC id the
  format cannot name (0xFF or above). Without `irq:cpus`, pcid uses the
  logical id, as upstream did.

In every build, opening `cpu-XX` for a CPU that does not exist returns
`ENOENT` instead of panicking the kernel. `ferric-iommu-test` checks
delivery with `iommu-msi-percpu`.

Drivers give MSI-X table entries to CPUs through `pcid_interface`:

- `Msix::alloc_on(cpu)` programs the next entry with a vector on that logical
  CPU.
- `Msix::alloc_spread(count, node)` programs up to `count` entries, one per CPU
  before any CPU gets a second, with CPUs on the device's NUMA node first
  (`ferric_irq::spread`).
- `InterruptVector::cpu` says where each vector went.

Without `irq:cpus` these calls put every vector on the BSP, like upstream's
`Msix::alloc`. Errors are typed (`pcid_interface::percpu::VectorError`).
`percpu::online_cpus` and `percpu::bsp_cpu` read the CPU map and the BSP's
logical id. `nvmed` uses them for its per-CPU queue pairs (below), and
root can steer a vector to another CPU; progress on roadmap Phase 2.4 is in the
status table in `ROADMAP.md`.

### Steering a vector to another CPU

Root moves a reserved vector to another CPU with a `SYS_CALL` on its `irq:`
handle: `ferric_irq::call::Call::Move { cpu }` (metadata `[1, cpu]`). The
kernel:

1. reserves a free vector (48 or above, not 0x80) on the target CPU;
2. with interrupt remapping (VT-d), rewrites every remapping entry that
   delivered the old vector to the old CPU: one 64-bit store of the entry's
   low half (vector and destination; the source-id check in the high half is
   untouched), then an interrupt entry cache invalidation for each entry;
3. re-keys every handle of the old (CPU, vector) to the new one, so the
   caller keeps its descriptor, and gives the old vector back;
4. reports one interrupt pending on the moved handles, so one that arrived
   on the old vector during the move is not lost (it may be one spurious
   wakeup).

A vector given back this way (or by closing its handle) can be reserved again.
The new reservation starts with the count read just before it was made
acknowledged, so it does not report the interrupts the previous owner
received but does report one that arrives right after. Reserving, moving and
giving back vectors take one lock, so two moves of the same vector run one
after the other (the second from where the first left it), and the owner's
close waits for a move in progress. A remapping unit has to be able to name
the target CPU's APIC only if it holds an entry to move; that is checked for
every unit before any entry changes.

The call returns `ferric_irq::call::Moved`: the new IRQ number and how many
remapping entries moved. When none moved, the device's interrupts are not
remapped and its MSI or MSI-X message names the CPU and vector itself, so
the driver must program the message for the new vector.
`pcid_interface::irq_helpers::Msix::retarget(index, cpu)` does all of this for
one MSI-X table entry (`percpu::move_vector` is the bare call). It goes by the
entry's own message rather than by the count, which can include other
devices' entries for the same vector: a remappable-format message is left to
the kernel, and a compatibility-format one is masked before the move (so the
device holds an interrupt pending instead of sending it to the vector given
back), reprogrammed, and restored to its earlier mask state. A handle
opened with `O_STAT`, which reserves nothing, can move a vector only when
remapping entries carry it. Errors: `EINVAL` (a legacy IRQ, an unreserved
vector, or the same CPU), `ENOENT` (no such CPU), `EEXIST` (no free vector on
the target), `EOPNOTSUPP` (an `O_STAT` handle without remapping, or a kernel
without `kernel.irq_percpu`) and `EIO` (an invalidation failed).

pcid remembers the remapping entry it made for each (CPU, vector) a driver
asked for, to hand it out again. Before it does, it asks the kernel where the
entry delivers now (`IrteTarget` on its IOMMU capability), and makes a new
entry when root steered the old one elsewhere. Policy, such as balancing
vectors across CPUs, is left to user space. `ferric-iommu-test` checks
steering with `iommu-msi-steer`, which moves edu's vector while the device
fires.

### Per-CPU NVMe queue pairs

With `drivers.nvme_percore` (off by default; needs `kernel.irq_percpu`),
`nvmed` gives each online CPU an I/O queue pair of its own:

- The admin queue and the first I/O queue pair stay on MSI-X entry 0, on the
  BSP, as upstream. Before it creates that first pair, it asks the controller
  for one more pair per CPU (Set Features, Number of Queues), since the
  specification allows that request only before any I/O queue exists, and
  takes as many as the controller grants and the MSI-X table has entries for.
  A pair counts only once the controller accepts both its Create I/O
  Completion Queue and Create I/O Submission Queue commands. If the
  submission queue fails, nvmed deletes the completion queue again and
  carries on with the pairs it already has. The log then says
  `nvmed: N I/O queue pairs (controller grants G): pair 1 on the main thread,
  N-1 per-CPU, workers on CPUs [...]`.
- It programs one MSI-X entry per pair with `Msix::alloc_spread`, in two
  batches, and checks each vector's CPU against `irq:cpus` and the order
  `ferric_irq::spread` promises (`ferric_irq::verify_spread`). The log says
  `nvmed: MSI-X entries 1..=N spread over CPUs [...]`.
- Each pair's completion queue interrupts through its own entry, and a worker
  thread pinned to that CPU (`pcid_interface::percpu::pin_current_thread`,
  which writes the thread's `sched-affinity` mask from `ferric_irq::affinity`)
  serves it with its own executor, `irq:` handle and bounce buffer. The
  scheduler queues a woken context on the CPU it last ran on, which a thread
  that just pinned itself elsewhere may no longer be allowed to use. The
  kernel then hands such a context to a CPU in its affinity
  (`context::switch`), instead of leaving it in a queue that never runs it.
  A context that is running when its affinity stops including its CPU is
  switched out at the next schedule and handed on the same way.
- The disk scheme stays on the main thread. A read or write that arrives while
  no other is in flight runs there, on the first I/O queue pair, exactly as
  with one queue: a hand-off costs two cross-CPU wake-ups (about 40 us a
  request on CI runners) and gains nothing without overlap. Requests that
  overlap go to the workers in turn, and the main thread waits for each
  worker to report its job done on a pipe. This rule is
  `ferric_blkqueue::QueuePolicy::Adaptive`. The disk scheme's set-policy call
  (`ferric_blkqueue::abi`, on a disk or partition descriptor) can instead keep
  every request on the main queue (`MainQueue`) or send every request to a
  worker (`Workers`). Only the process that opened the descriptor, which must
  be root, may make this call: driver-block refuses it with `EPERM` on a
  descriptor passed to another process (`ferric_blkqueue::openers`). The
  scheme cannot see the caller's CPU yet, so worker requests are spread
  round-robin. Native rings with a home CPU (later steps of the plan) are what
  route a request to the queue of the core it came from.
- Each job's result goes through a `ferric_blkqueue::slot::JobSlot`, which
  the worker settles exactly once: done, or abandoned if it drops the job
  unfinished. When a worker thread ends, even by a panic, it marks itself
  dead, abandons the jobs still queued for it, and reports
  `ferric_blkqueue::ids::WORKER_GONE` on the pipe. That wakes every waiting
  request, and the abandoned ones fail with `EIO` instead of hanging. Later
  requests skip dead workers, and run on the main queue once none is left.
  The pipe's reader keeps the part of an id that a read splits for the next
  read (`ferric_blkqueue::ids::IdStream`).
- driver-block (roadmap Phase 2.4, #133) serves a disk's plain reads and
  writes as concurrent tasks when the disk says its clones may run at once
  (`Disk::concurrent`). nvmed says so
  only with per-CPU workers under the `Workers` policy
  (`QueuePolicy::serves_concurrently`). Each request is checked and located
  at once, and becomes a `driver_block::PendingIo`: the request together with
  what it needs to answer itself, from any thread. The disk can take it with
  `Disk::submit`. nvmed hands it to a worker (#134), which runs it on its own queue
  pair and writes the scheme's response itself, so the reply skips the main
  thread (no completion pipe, no main-thread wake-up). Otherwise it runs as a
  task on the scheme's thread, on a clone of the disk. A `PendingIo` dropped
  unanswered (a worker that ends) answers `EIO`. Every other
  call, and every call on other disks, is still served one at a time, as
  upstream. A descriptor closed while its requests run stays open until the
  last one ends (`driver_block::SharedGate`, over
  `ferric_blkqueue::order::CloseGate`). The close then runs before the next
  request is served, and the request that makes it ready wakes the scheme's
  loop (`SharedGate::ready`, which nvmed waits on beside the scheme's events),
  so it runs on an idle disk too. So a later exclusive open cannot see
  the lock released before the writes land. On nvmed's side, the main
  thread's queue pair still serves one request at a time (its thread context
  and bounce buffer are shared): `MainQueue` requests queue for it on an
  async lock.
- On QEMU, spilling overlapping requests to per-CPU workers cannot beat one
  queue. The emulated controller serves all its queues from one thread, and
  each hand-off costs cross-CPU wake-ups. With `Adaptive` served
  concurrently, two-thread reads reached 0.78 of the single queue and
  four-thread reads 0.67. So `Adaptive` and `MainQueue` are served one call at
  a time, as before, and the benchmark's `workers_iops` rows (true overlap)
  are the signal to watch for changes that could make spilling pay.
- A watchdog ticks once a second on each worker and on the main thread. It
  logs `nvmed: watchdog ...` when a job has waited a second or more, saying
  whether its completion queue holds an entry nobody was woken for, or when
  it finds finished jobs whose report never arrived. The tick also wakes the
  executor, so a lost wake-up shows up as a stall rather than a hang. CI fails
  on any watchdog line.

`ferric-blkgrant-test` prints `FERRIC_BENCH nvme-read-4k-t<N>` with the
throughput of 4 KiB `pread`s from 1, 2 and 4 threads at once. It is a
same-boot A/B: three rounds of each queue policy in alternating order, or
nine when the first three fail (see below). It reports the medians (`copy_iops` for the default policy, `single_iops` for
`MainQueue`, `workers_iops` for `Workers`) and the verdict of
`ferric_blkqueue::ab`. Rounds are paired: the median of the default's
per-round ratio to the single queue (`ab_ratio`, which cancels drift during
the boot) must reach 1 less the rounds' spread (`ab_tolerance`, between 5%
and 15%). When the first three rounds fail, the benchmark runs six more
rounds of each policy and decides on all nine (`ab_rounds` says how many ran).
A round whose single queue measured nothing counts as a tie if the default
measured nothing too, and as a win for the default otherwise. While the scheme serves one call at a
time, `Adaptive` runs exactly the `MainQueue` path, and one unlucky round
used to fail the boot. With the extra rounds, identical code passes, while a
default that is slower in every round still fails. Runner hardware varies
about 2x between CI jobs, so numbers from different runs make a poor gate.
CI fails a boot whose line says `ab=fail`.

The kernel fix this work turned up is not specific to NVMe. When `schedule`
passes a runnable context over because its lock or its address space's lock
is held elsewhere for a moment, `switch` now returns
`SwitchResult::Contended` and the idle loop tries again instead of halting.
Before, the context waited for the CPU's next timer interrupt (up to about
4 ms). With two threads reading at once, nvmed's main thread mapping the next
client's buffer kept tripping this on the CPU of the worker it had just
woken, which held two-thread reads to about 400 IOPS.

### Overlapping depth with per-CPU workers (drivers.nvme_overlap)

`drivers.nvme_overlap` builds nvmed with its `overlap` Cargo feature: the
Adaptive policy keeps the first request of a burst on the
main thread's queue pair and hands each request that arrives while another is
in flight to a per-CPU worker that answers it itself ([`Disk::submit`], so
the reply skips the main thread). With the feature off (the default) Adaptive
serves one call at a time, exactly as upstream.

The same-boot measurement that keeps the default off: on QEMU/KVM (Kleya,
`host,migratable=off`, 5 I/O queue pairs, `ferric-iobench` qd32 rounds), the
adaptive-overlap rounds ran at 973/1006/967 permille of the main-queue IOPS
(2 of 3 rounds about 3% behind), because each hand-off costs more than the
emulated controller saves. The same rounds without overlap measure 1000-1021
permille, so the serial default is the better QEMU behavior. The Phase 2 exit
criterion (80% of Linux io_uring at qd32) cannot be met on the emulated
controller at all: it processes every queue's commands on one thread at about
52-67 us each, walling 4k random reads at roughly 15-19k IOPS whatever the
driver does. On real NVMe hardware the controller is far faster than the
per-request hand-off, and the overlap option is the path that keeps more than
one request in flight; measure it there before changing the default.

### Where a request's time goes (drivers.nvmed_perf)

`drivers.nvmed_perf` builds nvmed with its `perf` Cargo feature, which times
each I/O request's phases with the CPU cycle counter and logs a summary about
once a second:

```text
nvmed: perf: read.service n=2048 mean=196.633 min=176.044 max=227.512 us
nvmed: perf: read.device n=2048 mean=151.790 min=139.812 max=179.145 us
```

- `read.service`/`write.service` run from `namespace_read`/`namespace_write`
  entry to return, on whichever thread serves the request: the scheme's main
  thread, or a per-CPU worker answering a job or a direct request.
- `read.device`/`write.device` sum, over the request's commands, the time from
  just before the submission queue's doorbell to the observed completion,
  including the controller's service time and the interrupt-to-worker wake-up
  latency.
- The difference (service minus device) is the driver's own work: queue-pair
  and lock waits, bounce-buffer copies and executor wake-up latency not
  charged to the device.

The counter reads add no system calls, so an instrumented driver still shows
the syscall counts of the unmodified one, and the ticks are calibrated once
against the monotonic clock at driver start. Which thread served what, and
the queueing and response legs, come from the kernel trace
(`scheme.request`/`scheme.response`, `sched.switch`, `irq.deliver`/`irq.ack`,
and the debug-level `scheme.sqe_deliver`/`scheme.cqe_receive`, which need
`kernel.trace_max_level = "debug"`). The phase figures are for comparing one
boot, not a documented time source.

## Disk benchmark (ferric-iobench)

Phase 2's exit benchmark measures the NVMe paths with fio-style jobs.
fio itself is GPL-2.0, so the project does not ship or port it
(`recipes/wip/bench/io/fio` is an upstream recipe and stays unused). The
benchmark is its own MIT code, in two parts:

- `crates/iobench` (`ferric-iobench`, `no_std`, host-tested) holds the logic.
  It parses fio's options (`--rw`, `--bs`, `--iodepth`, `--numjobs`,
  `--io_size`/`--size`, `--randseed`, `--ioengine=psync`, `--direct=1`) and
  refuses by name what the runner does not do: writes, engines other than
  `psync`, and buffered I/O (`--direct=0`). Before a job runs, its block size
  is checked against the partition's logical block size (`st_blksize`), since
  the disk scheme refuses reads that are not whole blocks. A job with fewer
  blocks than reads in flight is refused too: some threads would have nothing
  to read, and the fio command fewer jobs. It has the block order, a
  latency histogram with fio's bucket layout, the IOPS, bandwidth and
  percentile report, and the fio command line for running the same job on
  Linux.
- `recipes/tests/ferric-iobench` is the on-target runner. It reads the
  largest partition of the NVMe disk scheme. Without arguments it runs three
  jobs: `randread-4k-qd1` (latency), `randread-4k-qd32` (IOPS) and
  `read-128k-qd1` (bandwidth). With arguments it runs the one job they
  describe, for example `ferric-iobench --rw=randread --bs=4k --iodepth=8`.

When nvmed has per-CPU queue pairs, every round runs the job once with each
queue policy: `MainQueue` (the one-queue path), `Adaptive` (the default) and
`Workers`. Odd rounds run the policies in reverse order.
`ferric_blkqueue::ab` compares `Adaptive` against `MainQueue` round by round,
with the same paired rule and confirmation rounds as `nvme-read-4k-t<N>`
above. The runner prints one line per policy and one for the comparison:

```text
FERRIC_BENCH iobench-randread-4k-qd32 policy=main rounds=3 ios=6144 iops=… mib_s=… lat_min_ns=… lat_mean_ns=… lat_p50_ns=… lat_p99_ns=… lat_p999_ns=… lat_max_ns=…
FERRIC_BENCH iobench-randread-4k-qd32 ab_baseline_iops=… ab_candidate_iops=… ab_ratio=… ab_tolerance=… ab_rounds=3 ab=pass
```

A policy's line covers all of its rounds: total reads over total time, and
every read in the histogram. The A/B line gives the per-round medians. Without
per-CPU queues there is one path, `policy=single`, and `ab=none`.
`scripts/ferric-ci/check-iobench-log.sh` requires every line on each of the
five CI boots, fails on `ab=fail`, and puts the numbers in a notice and in a
table in the job summary. CI runs on emulated NVMe under KVM on shared
runners, so its numbers are only good for the same-boot comparison, not as
absolute figures.

### Comparing with fio on Linux

`Job::fio_command` prints the fio command line that runs the same job on
Linux. Run it in a guest with the same shape (the same QEMU `-cpu`, CPU count,
memory and `-device nvme` options as `boot-smoke.sh`, or the same reference
machine), against the partition block device that matches the one the runner
read. There are two engines, and they answer different questions:

- `FioEngine::Psync` is the like-for-like run. It uses
  `--ioengine=psync --numjobs=N`, so there are N threads each doing one
  synchronous `pread`, which is exactly what the runner does. The runner
  shares a single block order between its threads. fio gives each job its own
  region, so the command cuts the region into one range of whole blocks per
  job (`Job::ranges`, one `--name=… --offset=… --size=…` section each; when
  the blocks do not divide evenly, the first jobs get one block more), and
  `--group_reporting` adds the jobs together. In both, every block is read
  once, and every offset and size is a multiple of `bs`, as direct I/O needs. The difference between the two
  numbers is the system's per-read cost: Ferric Oxide's system call, scheme
  IPC and nvmed against Linux's block layer and NVMe driver.
- `FioEngine::IoUring` is the exit baseline. It uses
  `--ioengine=io_uring --iodepth=N`: one thread with N reads in flight, which
  is Linux's fastest path. The disk sees the same queue depth. Ferric Oxide
  has no asynchronous disk path in the runner yet (kernel rings can carry disk
  reads later), so this gap includes the cost of using threads instead of a
  ring.

For the default jobs, with `DEV` as the partition device:

```sh
fio --filename=DEV --rw=randread --bs=4096 --direct=1 --randseed=9223372040899026669 --ioengine=psync --group_reporting --name=randread-4k-qd1-0 --offset=0 --size=4194304
# 32 sections of 262144 bytes, from --offset=0 to --offset=8126464:
fio --filename=DEV --rw=randread --bs=4096 --direct=1 --randseed=9223372040899026669 --ioengine=psync --group_reporting --name=randread-4k-qd32-0 --offset=0 --size=262144 --name=randread-4k-qd32-1 --offset=262144 --size=262144 … --name=randread-4k-qd32-31 --offset=8126464 --size=262144
fio --name=randread-4k-qd32 --filename=DEV --rw=randread --bs=4096 --direct=1 --randseed=9223372040899026669 --ioengine=io_uring --iodepth=32 --numjobs=1 --size=8388608
fio --filename=DEV --rw=read --bs=131072 --direct=1 --randseed=9223372040899026669 --ioengine=psync --group_reporting --name=read-128k-qd1-0 --offset=0 --size=33554432
```

The figures map onto fio's output like this:

| `ferric-iobench` | fio | Notes |
| --- | --- | --- |
| `iops` | `read: IOPS=` | Reads over the wall-clock time from the first submission to the last completion. Like fio, the runner starts its threads before its clock: they wait at a barrier, and the clock starts when they are all there. |
| `mib_s` | `BW=… MiB/s` | Both use `kb_base=1024`. |
| `lat_*_ns` | `clat` (and `lat`) | Measured per read, from just before the `pread` to just after it returns. With a synchronous engine, fio's `slat` is close to zero, so `lat` and `clat` agree. fio prints microseconds by default. |
| `lat_p50_ns`, `lat_p99_ns`, `lat_p999_ns` | `clat percentiles` 50.00th, 99.00th, 99.90th | The bucket layout is the same: exact below 128 ns, then 64 linear buckets per power of two, so each bucket is within 1/64 (1.6%). Like fio, the value reported is the middle of the bucket that holds the read of that rank, with the rank rounded up. |
| `lat_min_ns`, `lat_max_ns`, `lat_mean_ns` | `clat min`, `max`, `avg` | Exact, not from the buckets. |

The random order is a seeded permutation (a bijective mix over the next power
of two, with cycle-walking), not fio's LFSR. The same `--randseed` therefore
gives a different sequence on each system. Both read every block exactly once,
with no locality. A flash drive's random-read figure does not depend on the
exact sequence, so the numbers compare. The disk scheme has no page cache, so
every read reaches the device, as `--direct=1` does on Linux. Neither side has
a ramp-up period (`ramp_time` is 0 by default).

## From PCI discovery to a driver's DMA address

1. Bootstrap exposes `/scheme/kernel.iommu`. `pcid` opens its root as root
   to obtain the attach capability and reports bridge information.
2. `pcid` creates an empty `domain`, attaches the function and its requester
   aliases, and passes the domain descriptor through its driver interface.
3. The driver interface binds the first domain to the driver's address space
   before DMA allocation. Additional functions join their devices into that
   domain. A process binds at most one domain, and a domain is bound by at
   most one live address space; a forked child starts unbound.
4. `memory:translation` now returns an IOVA in that domain. The first
   translation pins and maps the frames; repeated translations reuse the
   mapping. Contiguous allocations receive contiguous IOVA ranges.
5. Removing write access with `mprotect` narrows device permissions too.
   Unmapping memory, process exit and domain closure retire translations
   with the required invalidations before frames can be released. A
   translation whose installation could not confirm its invalidation may
   still be reachable by the device, so the kernel keeps its IOVA block and
   frame pins owned, by IOVA, without recording a mapping; only a domain
   closure that blocks every device releases them, and exactly once. A
   closure that cannot block them keeps them pinned. No test injects a failed
   installation: the shared crate's unit test covers the retained entries and
   the ordering of their release, and the kernel path is compiled for the
   target but only runs when hardware fails an invalidation.

The entry points are `userspace/base/drivers/pcid/src/iommu.rs`,
`src/driver_interface/mod.rs`, `crates/kernel/src/scheme/iommu.rs`, and
`crates/kernel/src/arch/x86_64/iommu/dma.rs`.

`enforce` intentionally rejects callers without a device domain. In-memory
drivers such as `lived`, `nbdd` and `usbscsid` that use DMA allocation interfaces
without their own PCI device need an adapted allocation path. Do not assume
every existing image configuration works unchanged under enforcement.

## Mapping grants for device access

With registered buffers enabled, a driver can issue `MapGrant` on the domain
it has bound, naming a grant descriptor it holds. The request maps the
registered frames at consecutive IOVAs with the grant's access and returns
the starting IOVA. DMA requires a whole-page-aligned grant; byte grants using
partial-page bounce views cannot expose those pages directly to a device.

`MapGrant` and `UnmapGrant` both require the caller's address space to be bound
to the supplied domain. Possessing its descriptor does not grant permission
to remove another address space's mappings. A caller that has not bound this
domain, including one bound to a different domain, receives `ENODEV`. A mapping
ticket distinguishes successive uses of an IOVA so a delayed revocation
cannot remove a newer mapping. The normal grant-revocation path removes the
DMA mapping and waits for invalidation. Revocation does not free the
mapping's IOVA range: the kernel *retires* it, and it stays reserved until the
driver's `UnmapGrant` of that IOVA (which then succeeds) or the domain's close.
A driver that still has a command queued for the revoked grant can only make
the device fault, never reach a buffer mapped later at a reused IOVA. `UnmapGrant`
answers `ENOENT` for an IOVA that is neither mapped nor reserved. The
interface never hands out physical addresses of another process's buffer as
a fallback. Drivers needing a fallback must manage their own bounce buffers.

Domain closure blocks its devices before removing the handle and records
whether that blocking succeeded. On success, it retires the domain's grant
links and completes their matching region records, including restored records
on closed grants. Closed regions release their frames and pin charges when
the last mapping completes. Region records are reconciled outside the handle
lock to preserve the region-before-handle lock order.

When a failed unmap is still in flight during domain closure, the link may
reach its region before the revoker publishes that failure. Before restoring
the record, teardown rechecks confirmed domain closure under the region lock.
It completes an already blocked mapping instead of stranding a restored record
after its link was removed. Failed device blocking still requires retention.

Failed DMA-grant unmapping retains its mapping record, grant link, frame
references and allocated IOVA. Once hardware unmapping fails, later attempts
return `EIO` rather than treating cleared page-table entries as proof of
completed invalidation. Resources are retired only after confirmed unmapping
or a safely blocked domain. If device blocking fails during domain close,
its failure marker remains after handle removal: later revocation returns
`EIO`, and the grant records and pinned frames remain retained. Concurrent
close and revocation retire a mapping only once. See `scheme/iommu.rs` and
`arch/x86_64/iommu/dma.rs` in the kernel.

Failed grant installation is also part of the mapping lifecycle. A confirmed
rollback releases its IOVA and pins. If installation or rollback cannot
confirm invalidation, the kernel retains the reservation, frame pins and a
failed mapping record in both the domain and grant before returning `EIO`.
Revoking or closing that grant keeps reporting failure and retains its region
records until the domain's devices are successfully blocked. This applies to
failures on the first physical run and after earlier runs succeeded; cleared
page-table entries alone do not prove cached DMA translations are gone.

Host fault-injection tests in `crates/iommu/tests/grant_failures.rs` cover
those installation and rollback failures, grant and region closure, and IOVA
retention and release. Hardware boot tests exercise the normal mapping path;
they do not inject invalidation failures into a real IOMMU.

With `kernel.trace_iommu` and an info-or-more-verbose maximum, `iommu.map`,
`iommu.protect` and `iommu.unmap` record completed domain operations after
required invalidations, including translation and grant paths. Failed/partial
operations and whole-domain destruction are absent; see [DMA tracing](ferric-tracing.md#completed-dma-domain-operations)
for the domain identity, fields and scope.

## Storage DMA into registered buffers

`drivers.grant_dma = true` (Drivers menu, default `false`) lets an application
have `nvmed` move disk blocks straight between the NVMe device and its
registered buffer: no bounce buffer, no copy and no per-request mapping. It
depends on `kernel.regbuf` and on `kernel.iommu` other than `never`, and sets
`FERRIC_INITFS_FEATURES=nvmed/grant-dma`, which `userspace/base/Makefile`
passes to the initfs build. Without the option, `nvmed` behaves as upstream.
The call ABI is defined once in `crates/blkgrant/src/abi.rs`; the driver side is
`userspace/base/drivers/storage/driver-block/src/grant.rs` (feature
`grant-dma`) and `nvmed`'s `PcidGrantDma`.

On an `nvmed` disk or partition descriptor:

1. **Attach.** The application passes a whole, page-aligned grant descriptor
   with `SYS_CALL` and `CallFlags::FD`, one metadata word naming a slot
   (0 to 63). The driver checks the grant is not revoked, maps it once with
   `MapGrant` in its bound domain, and keeps the descriptor. Attaching to an
   occupied slot replaces its grant. Errors: `EINVAL` for a bad slot or a
   descriptor that is not a grant, `ECANCELED` for a revoked grant, `ENODEV`
   without an IOMMU domain, and `EOPNOTSUPP` from drivers that do not map grants.
2. **Read and Write.** A `SYS_CALL` with `Request::Read` or `Request::Write`
   moves `len` bytes between block `lba` of the descriptor (relative to the
   partition) and the slot's grant at `offset`, and returns `len`. Errors:
   `ENOENT` for an empty slot, `EINVAL` for a length that is not whole blocks
   or a range outside the grant, `EACCES` for a read into a read-only grant,
   `ECANCELED` once the grant is revoked, and `EOVERFLOW` past the partition.
3. **Detach.** `Request::Detach` empties a slot. Closing the descriptor
   detaches every slot.

The kernel passes a scheme exactly three metadata words for a call on one
descriptor, and fills any the caller did not send with zeros. A request
therefore packs into three words. Word 0 holds the verb (bits 0 to 7), the slot
(bits 8 to 15) and the offset in the grant in whole 4 KiB pages (bits 16 to
63); words 1 and 2 are the LBA and the length. Offsets in a grant are
therefore page-granular, and `Request::encode` refuses an unaligned offset or
a zero length; `decode` reads missing words as zero.

Before each transfer the driver asks the kernel (`Info`) whether the grant was
revoked and refuses it with `ECANCELED`, so the device is normally never
pointed at an unmapped IOVA. A revocation that races a running command makes
the device fault at the IOMMU instead, because the retired IOVA stays reserved
until the driver unmaps it. The driver unmaps only once nothing uses the IOVA:
each transfer holds a lease on its grant until the device completes it, and a
detached, replaced or closed grant is unmapped when its last lease is dropped
(`crates/blkgrant/src/slots.rs`). With the option on, `nvmed` also builds PRP
lists for zero-copy commands, so a command carries up to the controller's
MDTS (one list page; 512 KiB on QEMU) instead of two pages; larger transfers
are split into several commands.

This is a direct-call path on `nvmed` descriptors only. RedoxFS does not
forward grants yet, the ring interface (`redox-rings` SQEs) has no grant slots,
and `ahcid` and `virtio-blkd` do not map grants.

`ferric-blkgrant-test` covers attach, reads and writes compared with `pread`
(writes go only to the BIOS boot partition and are restored), the refusals
above, revocation (`ECANCELED`, buffer untouched), detach releasing the retired
IOVA, and a copy versus grant read benchmark. Without an IOMMU its grant steps
skip, and it still prints the copy benchmark.
`scripts/ferric-ci/check-blkgrant-log.sh` checks those markers and the kernel's
map, unmap and IOVA-release lines in each IOMMU boot variant. Host tests in
`crates/blkgrant` cover the encoding, the slot checks, PRP layout and the lease
rule, including a revocation and detach while a transfer is in flight.

These are available primitives and tested integration paths. Apart from the
`nvmed` path above, they do not make the filesystem, network stack or other
drivers use an end-to-end registered-buffer protocol. The dedicated image tests exercise CPU
grants, forwarding, sub-grants, device DMA and revocation; see
[Validation and debugging](ferric-testing.md).

## Submission/completion rings

`kernel.rings = true` (Kernel menu, default `false`) enables `/scheme/kernel.ring`.
`crates/ring` (ferric-ring) is the one definition of the ring format: the
memory layout, the 64-byte SQE and 32-byte CQE, opcodes (the kernel's scheme
request numbers, so `Read` is 5 in both), flags, notify modes, chain rules and
the scheme's calls. It is `no_std` with no `unsafe` code, and the kernel,
bootstrap and clients all use it. The kernel side is
`crates/kernel/src/scheme/ring.rs`.

1. Open `ring` beneath the scheme root. `Request::Setup` allocates the ring:
   one header page, then the SQ and CQ slots, each on pages of its own. Entry
   counts are powers of two, at most 4096 SQ and 8192 CQ entries, with a CQ at
   least as large as the SQ (twice the SQ by default). Map the whole
   `Layout::map_len()` with `MAP_SHARED`.
2. Write SQEs with a `Producer<Sqe>` and publish them, then call
   `Request::Enter`. The kernel takes complete chains from the SQ and runs
   each SQE in the caller's context as the system call it stands for, on the
   caller's descriptor and buffer: `Read`/`Write` at an offset as
   `SYS_READ2`/`SYS_WRITE2`, at `CURRENT_POSITION` as `read`/`write`, and
   `Fsize`, `Fstat` and `Fsync` as the system calls of those names. A user
   scheme receives an ordinary scheme request, so every scheme works with
   rings unchanged.
3. Read one CQE per SQE with a `Consumer<Cqe>`: the SQE's `user_data` and the
   result, or minus the errno. `fevent` on the ring descriptor reports
   `EVENT_READ` while completions are waiting, and `Enter` triggers it.

Linked SQEs (`SqeFlags::LINK`) form a chain that runs in order, possibly
across schemes. A failure, or a `Read`/`Write` that moves fewer bytes than
asked, cancels the rest of the chain with `ECANCELED`. A chain is taken only
once its last SQE is published and only when the CQ has room for all of its
completions, so completions are never dropped: `Enter` returns `EBUSY` when
not even the first chain fits. Chains of more than 32 SQEs, or ones that fill
the SQ without ending, complete with `EINVAL`.

`Enter` looks up every descriptor a chain names when it takes the chain, before
any SQE runs, and holds those open file descriptions until the chain is done.
If another thread closes a descriptor, or replaces it with `dup2`, while the
chain waits, the chain still acts on the files it named when it was taken, as
one system call would. Whichever holder lets go of a description last (the
descriptor table, the ring, or a file system call still running on it, through
`syscall::fs::release_description`) closes it, so a description closed while a
ring or a call holds it is still closed exactly once.

The ring memory is shared with the application, which may write any of it at
any time. The kernel reads it atomically through its own mapping, keeps its
own indices, copies each SQE once before checking it, and fails `Enter` with
`EINVAL` when the application's SQ tail is out of range. One `Enter` runs on
a ring at a time; a concurrent one gets `EBUSY`.

Each ring has a file table of 1024 slots. `Request::RegisterFiles { first }`
is a `SYS_CALL` with `CallFlags::FD` whose payload holds descriptors (add
`FD_CLONE` to keep them open in the caller); they go into consecutive slots
from `first`, all or none. An SQE with `SqeFlags::FIXED_FILE` names a slot
instead of a descriptor, which skips the descriptor lookup and keeps working
after the application closes its descriptor. A slot keeps its file until
`Request::UnregisterFiles { first, count }` or until the ring is closed. A
full slot is refused with `EBUSY`; slots past the table and ring descriptors
(a ring may not hold a ring open) with `EINVAL`; the descriptors of a
refused call are closed. An empty slot completes with `EBADF`.

A chain whose first SQE has `SqeFlags::ASYNC` runs on the ring's worker, a
kernel context the ring starts on first use. `Enter` takes the chain, sets
aside room in the CQ for its completions, looks up the files it names
(refusing rings with `EINVAL`, since a chain holding its ring would keep the
ring from closing), and returns without waiting; the worker runs asynchronous chains in order, with
the submitter's address space and credentials, and posts each chain's CQEs
together, then triggers `EVENT_READ`. After each chain and address-space cleanup,
the worker restores its original PID and credentials before becoming idle
or exiting; its lifecycle start and exit therefore both identify a kernel
worker (PID 0). `Request::Enter { to_submit,
min_complete }` waits after submitting until the CQ holds `min_complete`
completions or nothing submitted is still running, on the worker or in
another thread's `Enter` (`EINTR` on a signal, `EINVAL` above the CQ's size);
`Enter` announces each chain it runs itself as soon as its CQEs are posted.
Closing the ring cancels the worker for good, as a signal that never clears:
the wait the running chain is in ends, as does any it starts later, and the
queued chains complete with `ECANCELED`.

Registered-buffer grants as SQE buffers, polled (`SqPoll`) and `UMWAIT`
notification, and rings served natively by user schemes come later (see the design in
`io/rings-design.md` in the project files). Their flags and modes already
have encodings and return `EOPNOTSUPP` today. `ferric-ring-test` covers the
calls, chains across RedoxFS, the pipe scheme and the null scheme, the file
table, `Fstat`, asynchronous chains and waits, closing a ring with a chain
blocked, the refusals, back-pressure and a corrupt tail, and prints ring
versus system call timings, by descriptor, by slot and on the worker.

## Page cache (read path)

With `kernel.page_cache = true` in `ferric.toml` (Kernel menu, off by
default), the kernel has a `kernel.pcache` scheme that caches file contents in
kernel memory and serves `read` and `pread` from it, without a round trip to
the file system once the pages are there (roadmap Phase 2.6, design in
`io/pagecache-design.md` in the project files). This step is the read path:
RedoxFS serves the files its clients open read-only from it, and writes stay
with RedoxFS, which drops the pages they change. Mappings, cached writes,
writeback and reclaim are later steps, so an object keeps every page it was
given until it closes. Without the option the kernel has no such scheme and
every read goes to the file system, as upstream.

How a file system uses it:

1. It opens `/scheme/kernel.pcache/queue` (root only) and binds the queue to
   its own scheme with `Bind`, passing its scheme socket as a `CallFlags::FD`
   descriptor. The queue then serves only that scheme's descriptions. Only the
   process that opened the queue can use it or its objects, binding included:
   calls from any other process, including a child that inherited the
   descriptors, get `EPERM`. The process is its pid as the process manager
   sets it on every thread, so all of its threads may call, before and after
   it `exec`s.
2. It opens `object/<hex id>` from the queue for each file it caches (an empty
   object, `EEXIST` if the ID is in use), sets its size with `SetSize`, and
   attaches each client description it opens with `Attach`, naming the handle
   number its scheme gave the description.
3. A positioned `read` or `pread` of an attached description is served from
   the object. Pages that are there are copied to the reader. Absent ones
   become 32-byte `Fill` records on the queue, with readahead past them (a
   window that starts at 4 pages and doubles on sequential reads, up to the
   queue's `SetReadaheadMax`, 32 pages by default); the reader waits.
4. The file system reads records from the queue (`fevent` reports
   `EVENT_READ` while there are some) and answers each with `Filled` on the
   object: the pages' bytes as the payload, zero past the end of the file,
   which the kernel copies into new frames. A failed answer carries a non-zero
   errno and an empty payload; the readers waiting for those pages get `EIO`,
   and the next read fills them again. A reader that was already waiting when
   the fill failed gets `EIO` even if a newer read has queued a new fill
   for the page by the time it wakes. Every record carries the
   object's 64-bit generation, which is new each time an object is opened and
   never repeats on a queue, and its answer must repeat it. Each fill record
   also carries a fill ID, numbered from 1 up on each object and never reused
   (an object that has used all 2^64 refuses reads that need a fill with
   `EOVERFLOW`),
   which the answer repeats too; each page being filled remembers the fill it
   waits for, and an answer acts only on the pages its fill still owns. An
   answer to a fill that owns none of its pages any more (it failed, was
   answered or was invalidated, and the pages may be filling again for a newer
   fill), or to a record of an earlier object with the same ID, is stale: the
   call returns 0 and changes nothing, however malformed the answer, so a late
   answer never fails a newer fill or the queue. Any other answer returns how
   many of its pages it acted on. A queue read
   that faults copying records out puts back those of objects still open and
   reports `EVENT_READ` again.
5. Shrinking an object with `SetSize` drops the cached pages past the new end,
   including the page the end now falls in, so a later read fills it again
   with zeros past the end. A reader keeps the outcome of each fill it waits
   for, so a failure reaches it whatever happens to the pages meanwhile, and
   the cache keeps nothing for a failed page.
6. When the file system changes a file's bytes itself (it served a write or a
   truncation), it calls `Invalidate` on the object with the pages that
   changed, then `SetSize`, before it replies. The cached copies are dropped;
   a fill in flight over them is stale, so its answer is not taken and its
   readers fill again. A read after a write therefore sees the new bytes.
7. Closing a description detaches it. Closing an object or its queue ends the
   cache of it: outstanding fills fail with `EIO`, its records still on the
   queue are dropped, and later reads of its
   descriptions go to the file system's scheme as before.

Every call fits the three metadata words the kernel passes a kernel scheme.
`ferric-pcache` (`crates/pcache`) is the one definition of the interface and
holds the policies, host-tested:

- `abi`: the scheme's paths, the `SYS_CALL` requests on queues and objects,
  and the fill-queue records. A record names 1 to `MAX_RECORD_PAGES` (1024)
  pages, which `RecordPages` enforces; longer runs are split. An answer is
  `[verb | count << 16 | errno << 32, first page, generation]` on the object
  the record named.
- `page`: the state of one cached page and the transitions fills, writes,
  writeback and truncation make.
- `object` (with the `alloc` feature): one object's page index, which turns a
  read into fills to queue or wait for and applies the file system's answers.
- `readahead`: the per-description window.
- `clock` and `rmap` (with the `alloc` feature): two-list CLOCK replacement
  for reclaim, and the reverse map from a file page to the mappings that show
  it, for the later steps.
- `client` (with the `client` feature, on Redox): the file system's side, the
  queue and object calls with typed errors, and `spawn_pump`, a thread that
  waits on the queue and hands each batch of records to the file system.
- `pump`: what that thread does with a batch it cannot hand on. Taking a
  record off the queue is final, so it answers every fill in the batch as
  failed with `EIO` (the readers get `EIO` and the next read fills again); if
  even that fails, it closes the queue, which fails every fill left. The
  thread retries a queue read a signal interrupts (`EINTR`), and closes the
  queue whenever it stops, so a queue is never left with nobody reading it.
  An answer to a fill that has ended, including the pump's late failure of a
  fill of an object RedoxFS has since reopened, is stale: the kernel ignores
  it and returns 0 (`client::Answered::Stale`), so it closes nothing.

RedoxFS (`userspace/redoxfs/src/mount/redox/pcache.rs`) binds a queue when it
mounts and the kernel has the scheme. Each regular file with a description
opened `O_RDONLY` gets an object (the node ID is the object ID), and those
descriptions are attached; writers, directories, `mmap` and `fstat` stay with
RedoxFS. Its pump thread forwards fill records to RedoxFS's own scheme (through a
description it makes from the scheme socket, since RedoxFS runs in the null
namespace once mounted and cannot open its own path) as a
call, so the main loop answers them in order with the writes it serves,
reading the pages the way a `read` does (after decompression and decryption,
and from a mapped buffer when there is one). Writes, appends, `ftruncate` and
`O_TRUNC` invalidate the pages they change; an append invalidates from the old
end, not from its descriptor's offset. A file with a shared writable mapping
is not cached while the mapping lasts, because RedoxFS writes those mappings
back on `fsync` and `munmap` without a write call. Each of RedoxFS's mapped
ranges counts its shared writable mappings (the kernel says on `munmap` which
kind went), and the page cache keeps their total per file, updated on every
`mmap` and `munmap` whatever happens to the descriptors or to read-only
mappings beside them; the first closes the file's object, which drops its
pages, and once the count is back to 0 new read-only descriptions are cached
again. A private mapping never changes RedoxFS's buffer (the kernel copies a
page on its first store), so RedoxFS no longer writes it back, nor ranges
nothing maps any more, whose retained buffers can be older than later writes.

The host-tools workflow runs its tests, Clippy and Rustdoc with and without
`alloc`. `ferric-pcache-test` runs in the smoke image, which CI builds with
the option on: it serves a small file system scheme of its own and checks a
miss, a hit with no scheme read, readahead, a failed fill, a done answer it
cannot take (which fails the fill rather than leave its reader waiting), records put back
after a faulting queue read, the owner check, invalidation (with a stale fill
answered with the wrong bytes), generations across a reopened object, and a
closed queue, and prints the time of a cached 4 KiB `pread` next to the same
read through the scheme. Then,
on RedoxFS, it checks that a read-only description sees writes, a write past
the end, truncations and an append made through another one (the append
leaving the pages before it cached), that a shared writable mapping keeps the
file out of the cache exactly while it lasts, even after its descriptor closes
or beside a read-only mapping, and a private one changes neither the file nor
its caching (the
access time shows whether a read was served from the cache: RedoxFS moves it
when it reads a file, the cache does not), and benchmarks warm 4 KiB
`pread`s and 1 MiB sequential reads of one file through a read-only
description (cached) against a read-write one (served by RedoxFS, as with the
option off) in the same boot, with the paired A/B rule of
`ferric_blkqueue::ab`; it fails if the cache is slower.

The kernel fix this test turned up is not specific to the page cache. When a
request is sent to a user scheme, the caller switches straight to the context
that created the scheme, so the handler can answer without a trip through the
scheduler. The test creates its scheme on its main thread, serves it from
another thread, and opens the file from the main thread. The caller was then
switching to itself. `direct_switch_to` returns at once in that case, so the
caller spun in `UserInner::call_inner` without ever blocking. Its CPU never
ran the serving thread, and no other CPU stole it, because work stealing does
not take a lone context. The caller now direct-switches only when the handler
context is a different context.

## ext4, read-only

`ext4d` (`userspace/ext4d/`, features.toml `fs.ext4`, off by default) serves an
ext4 volume read-only as a scheme, the way RedoxFS serves its own:

```text
ext4d [--offset BYTES] [--length BYTES] DEVICE SCHEME
```

`DEVICE` is a disk, a partition or an image file; `--offset` and `--length` pick
the volume's bytes out of a whole disk. The volume appears at `/scheme/SCHEME`.
The daemon registers the scheme, enters the null namespace (it already holds the
device) and serves in the foreground until it is stopped.

- **Reading** is done by the `ext4-view` crate (MIT or Apache-2.0): extents,
  hashed (htree) directories, metadata checksums, 64-bit block numbers and the
  journal. ext4d reads the superblock's block counts itself for `statvfs`,
  written from "The Super Block" in "Ext4 Disk Layout" on the ext4 wiki at
  kernel.org. No code comes from Linux.
- **Read-only:** opens for writing, creating or truncating, and `unlinkat`,
  `ftruncate`, `fchmod`, `fchown`, `futimens`, `frename` and `flink`, fail with
  `EROFS`.
- **Access:** the scheme uses the caller's UID and primary GID. It checks
  directory search permissions at each path component, including expanded
  symlink targets, and read permission on the final inode. UID zero bypasses
  these checks. Supplementary groups and ACLs are unsupported; permissions
  use the inode's owner/group/other mode bits. `O_STAT` handles expose metadata
  without permitting data reads, and `O_STAT | O_NOFOLLOW` stats the link itself.
  `O_SYMLINK` on a regular file fails with `EINVAL`; relative opens require a
  directory handle, including paths containing `..`.
- **Layers:** ext4 on LUKS or LVM is ext4d over a volume `ferric-blockd`
  serves: `ext4d /scheme/NAME SCHEME` reads it like any disk (see
  [Block layers](#block-layers-luks-and-lvm-read-only)).
- **Limits:** `ext4-view` does not expose inode numbers, so `st_ino` is a
  hash of the path (hard links get different numbers) and `st_ctime` is the
  modification time; `mmap` is not served; encrypted files and devices are
  refused (`EOPNOTSUPP`).

### Journal replay

A volume that was not cleanly unmounted has `needs_recovery` set: its journal
holds committed transactions that may not have reached their home blocks.
Without help, ext4d reads it through `ext4-view`'s view of the journal in
memory, which is enough for most volumes but not all: as of `ext4-view` 1.0.0
it refuses a journal that holds an escaped block (one whose data starts with
the JBD2 magic number), and on a log that wraps round from the end of the
journal to its start it ignores the committed transactions and serves the old
contents. The tests pin both behaviours down.

`fs.ext4_journal_replay` (default off, needs `fs.ext4`; Cargo feature
`journal-replay`) has ext4d replay the journal onto the disk before it serves
the volume, as `e2fsck -E journal_only` does. ext4d opens the device for
writing only when the volume needs recovery, and only for the replay; it still
serves the volume read-only. This is the groundwork for writable ext4, which
has to start from a replayed journal.

- **`ferric-jbd2`** (`crates/jbd2`, `no_std`, host-tested) reads the JBD2
  log: the journal superblock, descriptor blocks and their tags, revoke blocks
  and commit blocks, with version 2 and 3 CRC32C checksums. It keeps the
  transactions that have a valid commit block, applies the revoke records (a
  revoke in transaction *r* cancels copies of the block logged in transactions
  up to *r*) and yields the newest surviving copy of each block. Every logged
  block is checked against its tag checksum when its transaction's commit
  block is reached, before anything is written. Under a journal with
  `INCOMPAT_ASYNC_COMMIT` (where the commit block need not be written last) a
  damaged block marks its transaction as torn: the log ends there and the
  transactions before it are replayed. Otherwise a damaged block stops the
  replay if it is the copy that would be written; a copy that a newer one
  replaces or that is revoked is never written, so it does not. Fast commits
  and external journal devices are refused; version 1 commit checksums are not
  verified.
- **ext4d** (`userspace/ext4d/src/journal/`) finds the journal inode through
  its group descriptor and extent tree (or ext3 block map). It checks the map
  before trusting it: the inode's block map and size must match the backup in
  the superblock (`s_jnl_blocks`), the superblock checksum must hold, and the
  journal must lie inside the volume, without holes, and clear of the
  superblocks, group descriptors, reserved GDT blocks, bitmaps and inode
  tables (volumes with `meta_bg` are refused for now). No replayed block may
  land inside the journal. It then writes the planned
  blocks and flushes, then empties the journal (`s_start` 0) and flushes, then
  clears `needs_recovery`, updates the superblock checksum and flushes. A replay
  interrupted at any point can be run again and gives the same volume; the
  tests crash a replay before each write to check that.

Both are written from "Journal (jbd2)" and the superblock, group descriptor,
inode and extent tree sections of "Ext4 Disk Layout" on the ext4 wiki at
kernel.org. Where that document is silent, the behaviour was settled by
experiment with e2fsprogs 1.47: `debugfs` (`journal_open -c -v N`,
`journal_write`) wrote transactions into test volumes, `e2fsck -E
journal_only` replayed them, and the bytes were compared. That is how these
were found:

- every checksum is CRC32C started from `0xFFFF_FFFF` and not inverted at the
  end, run over the journal UUID and then the covered bytes;
- with version 2 checksums a tag is two bytes longer than its fields (10
  bytes, or 14 with 64-bit block numbers);
- the tag checksum of an escaped block covers the block as stored, with its
  magic number zeroed (ext4d accepts the other form too, since the layout does
  not say);
- after a replay, `s_sequence` is one past the first sequence that had no
  commit.

The fixtures are volumes `make-fixtures.sh` builds this way, gzipped (about
70 KB in all): version 3 and version 2 checksums with 32- and 64-bit tags, an
ext3 volume with no checksums and an indirect-mapped journal, 4 KiB blocks, and
a log that wraps round. Each comes with `e2fsck`'s replay of it, and the tests
compare ext4d's replay with it block by block (apart from the write time and
write count e2fsck also updates). CI also builds fresh ones with the runner's
e2fsprogs and runs the same tests on them.

Host tests run against `tests/fixtures/basic.ext4`, made by `mke2fs` with
`tests/fixtures/make-fixtures.sh`, and the journal fixtures above;
`ferric-ext4-test` mounts the same image on the target, and with
`fs.ext4_journal_replay` replays and mounts `journal-v3` (`ext4-journal`), including `symlink_metadata` and refusal of `read_link` on a regular
file. A UID/GID 2000 child must read a public file and receive `EACCES` for
protected files, directories and symlink targets. Host fixtures additionally
cover owner and primary-group access.
The image workflow checks the ext4 markers only for smoke boots, since the
nightly fuzz script runs only the fuzzer and package verification.

## Block layers: LUKS and LVM, read-only

`ferric-blocklayer` (`crates/blocklayer/`) is a stack of read-only block
layers, each a `BlockDevice` (a size and a positioned read) built on the
devices below it, so they nest in whatever order the disk was set up in:

- **`Slice`**: a byte window of a device, such as a partition.
- **`Crypt`** (feature `luks`): opens a LUKS1 or LUKS2 volume through
  `ferric-luks` (header, passphrase, volume key, data segment) and decrypts
  AES-XTS sectors of 512 to 4096 bytes with `plain64` IVs, or `plain` ones,
  which wrap to 0 at sector 2^32 (2 TiB in). Whole sectors are
  decrypted in the caller's buffer; a partial sector at either end goes through
  one sector of scratch space. Segments that are flagged (reencryption),
  integrity-protected, or more than one are refused.
- **`Lvm`** (feature `lvm`): reads every PV's label and metadata through
  `ferric-lvm`, takes the newest metadata (highest `seqno`), matches each PV of
  the group to a device by its label's UUID, and splits reads wherever the
  logical volume moves to another extent run, stripe chunk or PV. Every PV of the
  group must be given; mirror, RAID, thin, cache and snapshot segments are
  refused.
- **`Stack`**: one type for a stack chosen at run time, whose error
  (`StackError`) keeps each layer's error in its source chain.

The crate is `no_std` (`alloc` for the LUKS, LVM and `Stack` layers) and has
no `unsafe` code. It builds for bare-metal targets, with
`RUSTFLAGS="--cfg aes_force_soft"` and `ferric-luks/force-soft` where SIMD is
not available, as for the bootloader.

**IVs of large sectors.** The LUKS2 specification (1.1.4, section 3.3) gives a
crypt segment's `sector_size` and its `iv_tweak` ("the starting offset for the
Initialization Vector") but not the unit the IV counts in when sectors are
larger than 512 bytes. This was settled by experiment, without reading any
implementation's source: cryptsetup 2.7 encrypted a volume with 4096-byte
sectors (`crates/blocklayer/tests/fixtures/make-fixtures.sh`), and its data
decrypts only when the IV of the sector at byte `b` of the segment is
`iv_tweak + b / 512`; counting in 4096-byte units gives noise from the second
sector on. So the IV always counts 512-byte units (`ferric_luks::data::IV_UNIT`),
and the fixture test `lvm_on_luks_with_4k_sectors_decrypts` reads sectors whose
IVs differ under the two readings.

`ferric-blockd` (`userspace/blockd/`, features.toml `fs.luks` and `fs.lvm`, off
by default; each builds in its layer) serves the top of a stack as a read-only
scheme:

```text
ferric-blockd SCHEME STEP...
  dev:PATH              push a disk, partition or image file
  slice:OFFSET:LENGTH   the top volume's bytes OFFSET..OFFSET+LENGTH (LENGTH "-": to the end)
  luks:KEYFILE          the LUKS volume on the top volume
  lvm:NAME              logical volume NAME of the group whose PVs are all the volumes so far
```

For example, LVM on LUKS on a partition, with ext4 on the logical volume:

```text
ferric-blockd root dev:/scheme/disk.nvme/0p2 luks:/etc/keys/root lvm:root
ext4d /scheme/root rootfs
```

- **Disks:** a Redox disk scheme serves only whole, aligned blocks, so every
  read is widened to the device's blocks (`st_blksize`): the aligned middle goes
  straight into the caller's buffer and a partial first or last block through a
  one-block bounce buffer.
- **Passphrase:** the whole content of `KEYFILE`, byte for byte (a trailing
  newline is part of it), as cryptsetup reads `--key-file`; never from the
  command line. The daemon reads key files and opens devices before it enters
  the null namespace.
- **Access:** the scheme holds one file, the volume, at its root. Only UID 0
  may open it (it is decrypted data), and the scheme root handle itself serves
  no data or metadata (`EBADF`), so every read goes through a checked open.
  Opens for writing, creating or truncating, and writes, `ftruncate`, `fchmod`, `fchown` and `futimens`, fail
  with `EROFS`. Reads stop at the end of the volume.
- **Limits:** read-only; no keyslot management, tokens, detached headers or
  reencryption; ciphers other than AES-XTS (`plain`/`plain64`) are refused;
  started by hand, not by init, the installer or the bootloader.

The fixtures (`crates/blocklayer/tests/fixtures/`, made by `make-fixtures.sh`
as root with cryptsetup, lvm2 and Python's `cryptography`, no device-mapper)
hold a known byte pattern in every volume: LUKS2 (512-byte sectors) on an LV
split over two extent runs, an LV on LUKS2 with 4096-byte sectors, and LUKS1
data OpenSSL encrypted with the key of `crates/luks/tests/fixtures/luks1.img`.
Bytes no test reads are zeroed to keep them small. `ferric-blocklayer-test`
serves the same images through `ferric-blockd` on the target.

## Kernel-bypass network queues

`ferric-netq` (`crates/netq`) is the ABI of the kernel-bypass network queues
(roadmap Phase 2.5, design in `io/netbypass-design.md` in the project files),
and the driver's side of copy mode. It is `no_std` with no `unsafe` code and
is host-tested. `drivers.net_bypass = true` (Drivers menu, default `false`,
needs `kernel.regbuf`) builds `driver-network` with its `bypass` feature
(through `e1000d/bypass` and `virtio-netd/bypass`), which adds the `queue`
path in copy mode to both drivers. Without
the option the network schemes have no `queue` path, as upstream.
Zero-copy mode, `igbd` and unprivileged sockets come in later changes.

An application opens `queue` beneath a network scheme
(`/scheme/network.<nic>/queue`), registers a UMEM (a regbuf region split into
2 or 4 KiB frames) and exchanges frames with the driver through four rings in
shared memory: Fill and TX from the application, RX and Completion from the
driver. Filters say which received frames go to the socket instead of
netstack.

- `umem`: frame geometry, and the check that an offset names one of its
  frames.
- `desc`: the ring entries (8-byte frame entries for Fill and Completion,
  16-byte packet descriptors for RX and TX), checked against the UMEM on
  decode, since neither side trusts the other.
- `frame`: the Ethernet, VLAN, IP and TCP/UDP header fields filters match on.
  Priority tags (VLAN ID 0) name no VLAN, as in IEEE 802.1Q.
- `filter`: steering filters (an exact match, every frame of the port, or a
  secondary MAC address), their validation, a canonical 64-byte encoding and
  the software match copy mode will use. A match can select one VLAN ID or
  untagged traffic.
- `call`: the queue socket's `SYS_CALL` requests and the statistics record.
  `Attach` passes the UMEM grant and the ring grant as `CallFlags::FD`
  descriptors (the kernel gives a scheme only the first metadata word of such
  a call), then `Bind` starts the queue with its parameters (frame size, ring
  sizes, mode, CPU, and `BindFlags`) in a 32-byte payload. `Own`,
  `AddFilter`, `RemoveFilter`, `Kick` and `Stats` follow. Every request fits
  the three metadata words the kernel passes on a plain call.
- `own`: port ownership and TX validation. `Own` passes a netstack `udp` or
  `tcp` socket bound on a port (another `CallFlags::FD` call). The driver
  reads the port from the socket's path, `/scheme/udp/<remote>/<local>`, and
  keeps the socket open, so netstack keeps the port reserved; an `Own` of a
  port the socket already owns keeps no second descriptor. The netstack
  socket must be bound to one address: one bound to `0.0.0.0` (or `::`) is
  refused with `EINVAL` (`OwnError::UnspecifiedAddress`), since owning a port
  on every address would let the socket send from any source address. So an
  owned port is always an (address, port) pair. A filter on a TCP or UDP
  destination port needs that port owned on the filter's destination
  address, which it must name. TX validation drops a frame unless it has the
  NIC's source MAC, no VLAN header (a priority tag counts), and is TCP or UDP
  from an owned port and that port's address: the source IP is checked on
  every validated frame, so a socket cannot spoof another host on its
  network. Validated TX sends no IP
  fragments, the first one included, since a fragment's ports cannot be
  checked without reassembly; `BindFlags::RAW_TX` skips validation and is the
  supported path for fragments and anything else it does not cover. Dropped frames still
  come back on Completion and are counted in `Stats::tx_rejected`. The path
  check guards against mistakes, not a hostile root, since queue sockets are
  root's and root can serve any scheme. Unprivileged sockets will need the
  kernel to vouch for the scheme that answered.
- `ring`: the four rings on `ferric-ring`'s index protocol. `RingLayout`
  places them in the ring grant (one header page of indices and flags, then
  each ring's slots on pages of their own, Fill, RX, TX, Completion), and
  hands each side its producers and consumers. A peer's out-of-range index is
  `ferric-ring`'s `RingError::Corrupt`, and the driver sets `NEED_WAKEUP` before it sleeps on
  an empty Fill or TX ring, so the application only `Kick`s when asked.
  `CopyQueue::transmit` also leaves `NEED_WAKEUP` set on TX when it stops with
  entries still there, because its budget ran out or Completion was full: the
  driver sends only when kicked, so the application must kick again (after
  reaping Completion) for the rest. Each `transmit` clears the flag first.

- `copy`: one socket in copy mode (`CopyQueue`): up to 16 filters, the copy
  of a matched received frame into the next Fill frame and onto RX, and the
  copy of TX frames out of the UMEM for the NIC, handed back on Completion
  only when it has room, so no frame is lost. Frames that cannot be delivered
  are dropped and counted in `Stats`. UMEM and ring memory are seen as atomic
  words, so the application writing them at any time cannot cause undefined
  behaviour. `write_frame` and `read_frame` are those copies; applications
  use them for their side too.

In `driver-network` (`src/bypass.rs`), `Attach` maps both grants read-write
and shared; each must be a read-write grant of whole pages. `Bind` starts the
socket in copy mode (`Mode::ZeroCopy` fails with `ENOSPC`). While any socket
is bound, every tick drains the adapter, gives each frame to the first socket
with a matching filter (`ferric_netq::copy::offer`; `EVENT_READ` is posted
only when a frame reached the socket's RX ring), and keeps the rest, in
order, for netstack's reads (up to 256 frames; the oldest is dropped beyond
that). `Kick` sends up to 256 TX packets through the adapter, after TX
validation unless the socket was bound with `RAW_TX`. A filter on a port the
socket does not own fails with `EACCES`, and an `Own` with anything but a
netstack TCP or UDP socket bound to one address with `EINVAL`. Every TX
descriptor is checked before its frame is read: its offset must be the start
of one of the socket's UMEM frames, its length at most a frame and its
reserved flags clear, so the driver never copies (or, in a later zero-copy
mode, DMAs) from outside the registered pages; a descriptor that fails is
counted in `Stats::invalid_descriptors` and is not handed back on Completion,
since it names no frame of the socket's. A socket whose rings turn
out corrupt, or whose grant its owner revokes (checked on every tick and
call), is stopped, and its calls fail with `EIO`; a socket stopped during a
pass gets none of that pass's later frames. Closing a socket unmaps its
grants and closes its netstack sockets. Every queue socket is root's, since
the scheme root is root-only, so any socket may ask for raw TX.

`ferric-netq-test` (`recipes/tests/ferric-netq-test`, in the CI smoke image)
is the on-target test. It checks netstack answers an ICMP echo to the
gateway, attaches a UMEM and a ring region to a queue socket on the NIC's
driver (e1000d; one CI boot uses a virtio-net card and virtio-netd), and
checks the refusals (calls before `Bind`, a zero-copy `Bind`, a second
`Bind`, an unknown filter, a short `Stats` payload) and that `Mode::Auto`
runs in copy mode. With an ARP filter it sends an ARP request for the
gateway through its TX ring and receives the reply on RX, while netstack's
echo still works beside it; with an ICMP filter it sends its own echo
request and receives the reply; this socket is bound with `RAW_TX`. A second
socket, bound without it, owns a UDP port through a netstack socket bound to
the interface's address. Filters on unowned ports or on the owned port at
another address, an `Own` with a file and an `Own` with a socket bound to
`0.0.0.0` are refused. It exchanges a DNS query with the configured server
from that port, and has four spoofed frames dropped and counted: a foreign
source MAC, an unowned source port, the owned port from another source
address, and ICMP. `netq-tx-bounds` puts TX descriptors outside the UMEM on
TX (past its end, near the top of the address space, inside it but not at a
frame's start, longer than a frame, with a reserved flag) and checks each is
counted as invalid and none is sent. A TX benchmark then sends 20,000 60-byte
UDP frames through the socket and the same number through netstack's UDP
socket, and prints both rates (`FERRIC_BENCH netq-tx-udp60`), and a latency
benchmark (`netq-latency`) times 200 ICMP echoes to the gateway through the
socket's rings and 200 through netstack's `icmp` scheme, both polled without
sleeping, and prints the p50 and p99 of each (`FERRIC_BENCH netq-echo-rtt`).
These are QEMU numbers, for trend only. Last, it
checks the counters, closes both sockets with requests in flight, and checks
netstack answers again. It skips when the driver has no queue sockets or
there is no `kernel.regbuf` scheme.

Frame formats follow IEEE 802.3, IEEE 802.1Q, RFC 791, RFC 8200, RFC 768 and
RFC 9293, parsed with the `etherparse` crate.

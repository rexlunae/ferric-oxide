# ferric-sched-core

Allocation-free, safe Rust, `no_std` accounting used by the kernel's EEVDF
timeslice owners. `FairState` stores measured remaining execution time and
weighted virtual runtime/deadlines. `FairClock` advances a CPU's virtual timeline
using its timestamped population changes under the run-queue lock. `set_weight`
and `set_activity` settle the preceding interval first; `settle` never rewinds
for an older observation. Remote steals settle both queues at one timestamp
before moving weight and translating virtual lag. This shared clock has its own
observation time, independent of a timeslice owner's full elapsed execution.
Both clocks preserve division residue when weights are unchanged, so frequent
short observations do not erase service. Virtual times use `u128` and saturate
rather than wrap.

Only `ScheduleReason::Yield` forfeits unconsumed service. Wakeups, preemption,
blocking and internal reconsideration do not infer a yield from elapsed time.
Charge a blocked owner before deactivating it. Direct scheme execution remains
charged to the original timeslice owner. Migration translates virtual lag without
changing remaining real service. The requested base slice stays 20.25 ms; it is
independent of the periodic IRQ cadence and is not a dispatch-latency guarantee.

The kernel still owns runnable selection, affinity, locks, direct handoffs and
architecture switching. This crate does not suppress periodic interrupts,
implement a unified deadline/rearm protocol, make the scheduler IRQ-safe, bind
async workers or provide reservations. Those remain Phase S work.

```sh
cargo test --locked -p ferric-sched-core
cargo miri test --locked -p ferric-sched-core
cargo clippy --locked --all-targets -p ferric-sched-core -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps -p ferric-sched-core
cargo run --locked --release -p ferric-sched-core --example accounting_bench
```

Tests cover event granularity, explicit yields, overruns, blocked-owner charging,
fractional progress, migration, wide timestamps and a competing-task model with
irregular event intervals. Regressions cover remote population changes, a stale
local timestamp after a steal, idle/zero-weight intervals, migration observations
and full-width timestamp deltas. The competing-task model selects independently
of the kernel: it does not validate `select_next_context`, queue-key maintenance,
direct physical handoffs or native fairness. Live priority writes still require
coordinated reweight events under
[#187](https://github.com/rexlunae/ferric-oxide/issues/187).

The example compares the old runnable-owner arithmetic with the new accounting
at identical event inputs, then the preceding relative CPU clock with timestamped
settling at fixed weight. It measures host arithmetic cost, not clock-source
reads, lock contention or kernel scheduling speed/fairness; the old yield
heuristic and the new explicit reasons intentionally have different behavior.
Native image/boot and controlled baseline/adoption validation remain separate
requirements.

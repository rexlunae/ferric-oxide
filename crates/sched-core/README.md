# ferric-sched-core

Allocation-free, safe Rust, `no_std` accounting used by the kernel's EEVDF
timeslice owners. `FairState` stores measured remaining execution time and
weighted virtual runtime/deadlines. `FairClock` advances a CPU's virtual timeline
using the runnable weight from the interval being charged. Both preserve division
residue when weights are unchanged, so frequent short observations do not erase
service. Virtual times use `u128` and saturate rather than wrap.

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
irregular event intervals. The example compares the old runnable-owner arithmetic
with the new accounting at identical event inputs. It measures host arithmetic
cost, not kernel scheduling speed or fairness; the old yield heuristic and the new
explicit reasons intentionally have different behavior. Native image/boot and
controlled baseline/adoption validation remain separate requirements.

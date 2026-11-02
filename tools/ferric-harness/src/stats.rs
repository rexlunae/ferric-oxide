//! Host-side cost of a running guest.
//!
//! Two sources, sampled every few seconds and at the end of a run:
//!
//! - the CPU time of each QEMU host thread, from `/proc/<pid>/task/<tid>/stat` (user and
//!   system time; the vCPU threads are matched to vCPUs by QMP `query-cpus-fast`);
//! - KVM's own statistics through QMP `query-stats` (per vCPU: exits, halt exits, I/O and
//!   MMIO exits, interrupt injections and so on; per VM: the VM-wide counters). Only scalar
//!   values are kept; histograms are dropped. Under TCG there are none.
//!
//! Both are cumulative since QEMU started (KVM counts from vCPU creation), so the last sample
//! holds the run's totals; [`summarize`] turns the samples into the report's figures.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// CPU time of one host thread.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThreadCpu {
    /// Thread ID.
    pub tid: u32,
    /// The thread's name (`CPU 0/KVM`, `qemu-system-x86`, ...).
    pub comm: String,
    /// User time in milliseconds.
    pub user_ms: f64,
    /// System time in milliseconds.
    pub system_ms: f64,
}

impl ThreadCpu {
    /// User plus system time.
    pub fn cpu_ms(&self) -> f64 {
        self.user_ms + self.system_ms
    }
}

/// Parses one `/proc/.../stat` line: `tid (comm) state ... utime stime ...`. The name may
/// contain spaces and parentheses, so the fields are counted from the last `)`.
pub fn parse_stat(line: &str, ticks_per_second: u64) -> Option<ThreadCpu> {
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    let tid = line[..open].trim().parse().ok()?;
    let comm = line[open + 1..close].to_owned();
    let fields: Vec<&str> = line[close + 1..].split_whitespace().collect();
    // After the name: state is field 3, so utime (field 14) and stime (15) are at 11 and 12.
    let ticks = |i: usize| fields.get(i)?.parse::<u64>().ok();
    let to_ms = |t: u64| t as f64 * 1000.0 / ticks_per_second.max(1) as f64;
    Some(ThreadCpu {
        tid,
        comm,
        user_ms: to_ms(ticks(11)?),
        system_ms: to_ms(ticks(12)?),
    })
}

/// The CPU time of every thread of process `pid`.
///
/// # Errors
///
/// The error reading `/proc/<pid>/task` (the process has exited, say).
pub fn read_threads(pid: u32) -> io::Result<Vec<ThreadCpu>> {
    read_threads_in(&Path::new("/proc").join(pid.to_string()))
}

fn read_threads_in(proc_dir: &Path) -> io::Result<Vec<ThreadCpu>> {
    let ticks = rustix::param::clock_ticks_per_second();
    let mut threads = Vec::new();
    for entry in fs::read_dir(proc_dir.join("task"))? {
        let path = entry?.path().join("stat");
        // A thread can exit between listing and reading; it no longer costs anything.
        if let Ok(line) = fs::read_to_string(&path)
            && let Some(thread) = parse_stat(&line, ticks)
        {
            threads.push(thread);
        }
    }
    threads.sort_by_key(|t| t.tid);
    Ok(threads)
}

/// A vCPU and its host thread, from QMP `query-cpus-fast`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcpuThread {
    /// vCPU index.
    pub index: u32,
    /// Host thread ID.
    pub thread_id: u32,
    /// QOM path, which `query-stats` uses to name the vCPU.
    pub qom_path: String,
}

/// Parses the `return` value of `query-cpus-fast`.
pub fn parse_cpus(value: &Value) -> Vec<VcpuThread> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|cpu| {
            Some(VcpuThread {
                index: u32::try_from(cpu.get("cpu-index")?.as_u64()?).ok()?,
                thread_id: u32::try_from(cpu.get("thread-id")?.as_u64()?).ok()?,
                qom_path: cpu.get("qom-path")?.as_str()?.to_owned(),
            })
        })
        .collect()
}

/// Scalar statistics by QOM path (`""` for VM-wide entries), from `query-stats`.
pub type KvmStats = BTreeMap<String, BTreeMap<String, f64>>;

/// Parses the `return` value of `query-stats`, keeping numeric and boolean values.
pub fn parse_stats(value: &Value) -> KvmStats {
    let mut out = KvmStats::new();
    for entry in value.as_array().into_iter().flatten() {
        let path = entry
            .get("qom-path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let stats = out.entry(path).or_default();
        for stat in entry
            .get("stats")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(name) = stat.get("name").and_then(Value::as_str) else {
                continue;
            };
            let value = match stat.get("value") {
                Some(Value::Number(n)) => n.as_f64(),
                Some(Value::Bool(b)) => Some(f64::from(u8::from(*b))),
                _ => None,
            };
            if let Some(v) = value {
                stats.insert(name.to_owned(), v);
            }
        }
    }
    out
}

/// One vCPU in a sample.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VcpuSample {
    /// vCPU index.
    pub index: u32,
    /// Host thread ID.
    pub thread_id: u32,
    /// The thread's user time in ms (absent if the thread could not be read).
    pub user_ms: Option<f64>,
    /// The thread's system time in ms.
    pub system_ms: Option<f64>,
    /// KVM statistics for this vCPU.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub kvm: BTreeMap<String, f64>,
}

/// One sample of a running guest.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// Milliseconds since QEMU started.
    pub t_ms: f64,
    /// Every QEMU thread.
    pub threads: Vec<ThreadCpu>,
    /// The vCPUs.
    pub vcpus: Vec<VcpuSample>,
    /// VM-wide KVM statistics.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vm: BTreeMap<String, f64>,
}

impl Sample {
    /// Builds a sample from its parts.
    pub fn assemble(
        t_ms: f64,
        threads: Vec<ThreadCpu>,
        cpus: &[VcpuThread],
        vcpu_stats: &KvmStats,
        vm_stats: &KvmStats,
    ) -> Self {
        let vcpus = cpus
            .iter()
            .map(|cpu| {
                let thread = threads.iter().find(|t| t.tid == cpu.thread_id);
                VcpuSample {
                    index: cpu.index,
                    thread_id: cpu.thread_id,
                    user_ms: thread.map(|t| t.user_ms),
                    system_ms: thread.map(|t| t.system_ms),
                    kvm: vcpu_stats.get(&cpu.qom_path).cloned().unwrap_or_default(),
                }
            })
            .collect();
        let vm = vm_stats
            .values()
            .flatten()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        Self {
            t_ms,
            threads,
            vcpus,
            vm,
        }
    }
}

/// One vCPU over a run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VcpuSummary {
    /// vCPU index.
    pub index: u32,
    /// Host thread ID.
    pub thread_id: u32,
    /// Total CPU time of the vCPU thread at the last sample, in ms.
    pub cpu_ms: Option<f64>,
    /// Of which user time.
    pub user_ms: Option<f64>,
    /// Of which system time (KVM guest time shows up here and in user, by kernel).
    pub system_ms: Option<f64>,
    /// KVM counters at the last sample (cumulative since the vCPU was created).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub kvm: BTreeMap<String, f64>,
}

/// What the stats samples say about a run.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StatsSummary {
    /// Samples taken.
    pub samples: usize,
    /// Time between the first and last sample, in ms.
    pub span_ms: f64,
    /// CPU time of all QEMU threads at the last sample, in ms.
    pub qemu_cpu_ms: f64,
    /// Per vCPU.
    pub vcpus: Vec<VcpuSummary>,
    /// VM-wide KVM counters at the last sample.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vm: BTreeMap<String, f64>,
    /// Every QEMU thread at the last sample.
    pub threads: Vec<ThreadCpu>,
}

/// Summarizes samples (in time order): CPU time from the last sample, KVM counters from
/// the last sample that has them (QEMU stops answering `query-stats` while it shuts down,
/// so the final sample may have CPU times but no counters), and how many samples there
/// were over what span.
pub fn summarize(samples: &[Sample]) -> StatsSummary {
    let (Some(first), Some(last)) = (samples.first(), samples.last()) else {
        return StatsSummary::default();
    };
    let kvm_of = |index: u32| {
        samples
            .iter()
            .rev()
            .filter_map(|s| s.vcpus.iter().find(|v| v.index == index))
            .map(|v| &v.kvm)
            .find(|kvm| !kvm.is_empty())
            .cloned()
            .unwrap_or_default()
    };
    let vcpus = last
        .vcpus
        .iter()
        .map(|v| VcpuSummary {
            index: v.index,
            thread_id: v.thread_id,
            cpu_ms: v.user_ms.zip(v.system_ms).map(|(u, s)| u + s),
            user_ms: v.user_ms,
            system_ms: v.system_ms,
            kvm: kvm_of(v.index),
        })
        .collect();
    StatsSummary {
        samples: samples.len(),
        span_ms: last.t_ms - first.t_ms,
        qemu_cpu_ms: last.threads.iter().map(ThreadCpu::cpu_ms).sum(),
        vcpus,
        vm: samples
            .iter()
            .rev()
            .map(|s| &s.vm)
            .find(|vm| !vm.is_empty())
            .cloned()
            .unwrap_or_default(),
        threads: last.threads.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stat_lines() {
        let line = "4242 (CPU 0/KVM) S 1 2 3 4 5 6 7 8 9 10 250 50 0 0 20 0 1 0 100 0 0";
        let t = parse_stat(line, 100).expect("parse");
        assert_eq!((t.tid, t.comm.as_str()), (4242, "CPU 0/KVM"));
        assert_eq!(
            (t.user_ms, t.system_ms, t.cpu_ms()),
            (2500.0, 500.0, 3000.0)
        );
        let odd = "7 (a) (b)) R 1 2 3 4 5 6 7 8 9 10 1 2 0";
        assert_eq!(parse_stat(odd, 100).expect("parse").comm, "a) (b)");
        assert!(parse_stat("garbage", 100).is_none());
    }

    #[test]
    fn this_process_has_threads() {
        let threads = read_threads(std::process::id()).expect("read");
        assert!(!threads.is_empty());
    }

    #[test]
    fn qmp_values_and_summary() {
        let cpus = parse_cpus(&json!([
            {"cpu-index": 0, "thread-id": 11, "qom-path": "/machine/unattached/device[0]", "target": "x86_64"},
            {"cpu-index": 1, "thread-id": 12, "qom-path": "/machine/unattached/device[1]", "target": "x86_64"},
        ]));
        assert_eq!(cpus.len(), 2);
        let stats = |exits: u64| {
            parse_stats(&json!([
                {"provider": "kvm", "qom-path": "/machine/unattached/device[0]", "stats": [
                    {"name": "exits", "value": exits},
                    {"name": "halt_poll_success_hist", "value": [1, 2, 3]},
                    {"name": "guest_mode", "value": true},
                ]},
            ]))
        };
        let vm = |n: u64| {
            parse_stats(
                &json!([{"provider": "kvm", "stats": [{"name": "remote_tlb_flush", "value": n}]}]),
            )
        };
        let thread = |tid, user| ThreadCpu {
            tid,
            comm: format!("CPU {}/KVM", tid - 11),
            user_ms: user,
            system_ms: 1.0,
        };
        let first = Sample::assemble(0.0, vec![thread(11, 10.0)], &cpus, &stats(100), &vm(5));
        let last = Sample::assemble(
            5000.0,
            vec![thread(11, 30.0), thread(12, 4.0)],
            &cpus,
            &stats(350),
            &vm(9),
        );
        assert_eq!(first.vcpus[1].user_ms, None);
        assert!(!first.vcpus[0].kvm.contains_key("halt_poll_success_hist"));
        // The final sample, taken while QEMU shuts down, has CPU times but no counters.
        let shutdown = Sample::assemble(
            5100.0,
            vec![thread(11, 31.0), thread(12, 4.0)],
            &cpus,
            &KvmStats::new(),
            &KvmStats::new(),
        );
        let summary = summarize(&[first, last, shutdown]);
        assert_eq!(summary.samples, 3);
        assert_eq!(summary.span_ms, 5100.0);
        assert_eq!(summary.qemu_cpu_ms, 37.0);
        assert_eq!(summary.vcpus[0].cpu_ms, Some(32.0));
        assert_eq!(summary.vcpus[0].kvm["exits"], 350.0);
        assert_eq!(summary.vcpus[0].kvm["guest_mode"], 1.0);
        assert_eq!(summary.vm["remote_tlb_flush"], 9.0);
        assert_eq!(summarize(&[]), StatsSummary::default());
    }
}

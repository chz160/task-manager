//! Stateful `/proc` sampler. CPU percentages need two samples, so the
//! sampler keeps the previous counters between calls to [`Sampler::sample`].

use procfs::process::Stat;
use procfs::{CpuTime, Current, CurrentSI, FromRead, KernelStats, Meminfo};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::MetadataExt;

#[derive(Debug, Clone)]
pub struct MemInfo {
    pub total_kib: u64,
    pub available_kib: u64,
    pub swap_total_kib: u64,
    pub swap_free_kib: u64,
}

impl MemInfo {
    pub fn used_kib(&self) -> u64 {
        self.total_kib.saturating_sub(self.available_kib)
    }
}

#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: i32,
    pub ppid: i32,
    pub name: String,
    pub state: char,
    pub threads: i64,
    pub uid: Option<u32>,
    pub rss_kib: u64,
    /// Share of total system CPU (0..=100), Windows style. 0 on the first sample.
    pub cpu_percent: f64,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Overall CPU busy percentage. 0 on the first sample.
    pub cpu_percent: f64,
    pub per_core_percent: Vec<f64>,
    pub mem: MemInfo,
    pub processes: Vec<ProcessInfo>,
}

#[derive(Clone, Copy, Default)]
struct CoreTicks {
    busy: u64,
    total: u64,
}

fn core_ticks(t: &CpuTime) -> CoreTicks {
    // guest time is already included in user/nice.
    let idle = t.idle + t.iowait.unwrap_or(0);
    let total = t.user
        + t.nice
        + t.system
        + t.idle
        + t.iowait.unwrap_or(0)
        + t.irq.unwrap_or(0)
        + t.softirq.unwrap_or(0)
        + t.steal.unwrap_or(0);
    CoreTicks {
        busy: total - idle,
        total,
    }
}

fn percent(prev: CoreTicks, cur: CoreTicks) -> f64 {
    let dt = cur.total.saturating_sub(prev.total);
    if dt == 0 {
        return 0.0;
    }
    cur.busy.saturating_sub(prev.busy) as f64 / dt as f64 * 100.0
}

#[derive(Clone, Copy)]
struct ProcPrev {
    ticks: u64,
    /// A process never changes owner, so look it up once instead of every sample.
    uid: Option<u32>,
}

/// Reads and parses `/proc/<pid>/stat` with one open, one read and one close.
/// `procfs::process::Process` costs about three times as many syscalls per process,
/// which dominates the sampling cost with a thousand processes.
fn read_stat(pid: i32) -> Option<Stat> {
    // The stat line is far below 4 KiB (comm is at most 16 bytes); a full buffer means
    // something unexpected, so skip it.
    let mut buf = [0u8; 4096];
    let n = File::open(format!("/proc/{pid}/stat"))
        .ok()?
        .read(&mut buf)
        .ok()?;
    if n == buf.len() {
        return None;
    }
    Stat::from_read(&buf[..n]).ok()
}

fn list_pids() -> procfs::ProcResult<Vec<i32>> {
    Ok(fs::read_dir("/proc")?
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect())
}

#[derive(Default)]
pub struct Sampler {
    prev_total: Option<CoreTicks>,
    prev_cores: Vec<CoreTicks>,
    /// (pid, starttime) -> previous counters. Start time guards against PID reuse.
    prev_procs: HashMap<(i32, u64), ProcPrev>,
}

impl Sampler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sample(&mut self) -> procfs::ProcResult<Snapshot> {
        let ks = KernelStats::current()?;
        let total = core_ticks(&ks.total);
        let cores: Vec<CoreTicks> = ks.cpu_time.iter().map(core_ticks).collect();

        let cpu_percent = self.prev_total.map_or(0.0, |p| percent(p, total));
        let per_core_percent = cores
            .iter()
            .enumerate()
            .map(|(i, c)| self.prev_cores.get(i).map_or(0.0, |p| percent(*p, *c)))
            .collect();

        // Aggregate "cpu" line sums all cores, so one process can use at most
        // total_delta / ncores; scale to the whole machine like Windows does.
        let denom = self
            .prev_total
            .map_or(0.0, |p| total.total.saturating_sub(p.total) as f64);

        let page_kib = procfs::page_size() / 1024;
        let mut next_procs = HashMap::with_capacity(self.prev_procs.len());
        let mut processes = Vec::new();
        for pid in list_pids()? {
            // Processes can exit between listing and reading; skip them.
            let Some(stat) = read_stat(pid) else { continue };
            let ticks = stat.utime + stat.stime;
            let key = (stat.pid, stat.starttime);
            let prev = self.prev_procs.get(&key);
            let cpu = match (prev, denom > 0.0) {
                (Some(prev), true) => ticks.saturating_sub(prev.ticks) as f64 / denom * 100.0,
                _ => 0.0,
            };
            let uid = match prev {
                Some(prev) => prev.uid,
                None => fs::metadata(format!("/proc/{pid}")).ok().map(|m| m.uid()),
            };
            next_procs.insert(key, ProcPrev { ticks, uid });
            processes.push(ProcessInfo {
                pid: stat.pid,
                ppid: stat.ppid,
                name: stat.comm,
                state: stat.state,
                threads: stat.num_threads,
                uid,
                rss_kib: stat.rss * page_kib,
                cpu_percent: cpu,
            });
        }

        let mi = Meminfo::current()?;
        let mem = MemInfo {
            total_kib: mi.mem_total / 1024,
            available_kib: mi.mem_available.unwrap_or(mi.mem_free) / 1024,
            swap_total_kib: mi.swap_total / 1024,
            swap_free_kib: mi.swap_free / 1024,
        };

        self.prev_total = Some(total);
        self.prev_cores = cores;
        self.prev_procs = next_procs;
        Ok(Snapshot {
            cpu_percent,
            per_core_percent,
            mem,
            processes,
        })
    }
}

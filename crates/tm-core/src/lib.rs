//! Stateful `/proc` sampler. CPU percentages need two samples, so the
//! sampler keeps the previous counters between calls to [`Sampler::sample`].

mod app;
mod gpu;
mod net;
mod services;
mod startup;

pub use app::{app_from_cgroup, service_unit_from_cgroup};
pub use services::{ServiceInfo, list_services};
pub use startup::{StartupDirs, StartupEntry, list_startup, set_startup_enabled};

use gpu::GpuCounters;
use net::NetTracker;
use procfs::{CpuTime, Current, CurrentSI, KernelStats, Meminfo};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often the expensive per-process readings (PSS, disk I/O, GPU) are refreshed.
/// They cost far more than `/proc/<pid>/stat`, and change slowly enough for this.
pub const DEFAULT_EXTRAS_INTERVAL: Duration = Duration::from_secs(5);
/// `smaps_rollup` makes the kernel walk every mapping, so it is by far the costliest reading
/// (about half the sampler's whole CPU use if done for every process every few seconds).
/// A process's PSS is re-read when its RSS, which comes free with `stat`, has moved by more
/// than [`PSS_RSS_DRIFT`], or when the reading is this old. The first read of each process is
/// aged by its PID so the age-based refreshes spread over the period instead of bunching up.
const PSS_MAX_AGE: Duration = Duration::from_secs(60);
const PSS_RSS_DRIFT: f64 = 0.05;
/// A process known to have no GPU fds is looked at again after this many extras rounds.
const GPU_RECHECK_ROUNDS: u64 = 24;

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
    /// Proportional set size: shared pages split between the processes mapping them.
    /// `None` when `/proc/<pid>/smaps_rollup` is unreadable (other users' processes).
    pub pss_kib: Option<u64>,
    /// Bytes per second that reached or left the storage device. `None` when unreadable.
    pub disk_read_bps: Option<u64>,
    pub disk_write_bps: Option<u64>,
    /// Busiest GPU engine, 0..=100, and GPU memory in use. `None` when the process is not
    /// ours to inspect or the driver does not expose DRM fdinfo.
    pub gpu_percent: Option<f64>,
    pub gpu_mem_kib: Option<u64>,
    /// TCP bytes per second received / sent. `None` when the kernel's `sock_diag` is not
    /// available or the process's fd table cannot be read (another user's, when not root).
    pub net_recv_bps: Option<u64>,
    pub net_send_bps: Option<u64>,
    /// Unified-hierarchy cgroup path, e.g. `/user.slice/.../app-gnome-firefox-1.scope`.
    pub cgroup: Option<Arc<str>>,
    /// Arguments joined by spaces; `None` for kernel threads and unreadable processes.
    pub cmdline: Option<Arc<str>>,
}

impl AsRef<ProcessInfo> for ProcessInfo {
    fn as_ref(&self) -> &ProcessInfo {
        self
    }
}

impl ProcessInfo {
    /// The application this process belongs to: its cgroup's app or service name, else its own name.
    pub fn app_name(&self) -> String {
        self.cgroup
            .as_deref()
            .and_then(app_from_cgroup)
            .unwrap_or_else(|| self.name.clone())
    }
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

/// Slow-changing readings, cached between extras rounds.
#[derive(Clone, Default)]
struct Extras {
    pss_kib: Option<u64>,
    /// When PSS was read (back-dated by a per-PID jitter) and the RSS at that time.
    pss_at: Option<(Instant, u64)>,
    /// Cumulative `read_bytes`, `write_bytes` at the last round.
    io: Option<(u64, u64)>,
    /// CPU ticks (utime + stime) when `io` was read. A process that has not run since cannot
    /// have done any I/O, so its file is not read again.
    io_ticks: u64,
    disk_read_bps: Option<u64>,
    disk_write_bps: Option<u64>,
    gpu_prev: Option<GpuCounters>,
    gpu_percent: Option<f64>,
    gpu_mem_kib: Option<u64>,
    net: Option<(u64, u64)>,
    /// Round at which the fd table was last scanned for GPU clients.
    gpu_checked: Option<u64>,
}

struct ProcPrev {
    ticks: u64,
    /// A process never changes owner or (in practice) cgroup, so look them up once.
    uid: Option<u32>,
    cgroup: Option<Arc<str>>,
    cmdline: Option<Arc<str>>,
    extras: Extras,
}

/// One open, one read, one close; `fs::read_to_string` adds a size probe and a second read
/// to find EOF, which is a lot of extra syscalls over a thousand processes. The files read
/// this way are all well under 4 KiB; a full buffer means something unexpected, so give up.
fn read_small(path: &str) -> Option<String> {
    let mut buf = [0u8; 4096];
    let n = File::open(path).ok()?.read(&mut buf).ok()?;
    if n == buf.len() {
        return None;
    }
    Some(String::from_utf8_lossy(&buf[..n]).into_owned())
}

fn read_pss_kib(pid: i32) -> Option<u64> {
    let text = read_small(&format!("/proc/{pid}/smaps_rollup"))?;
    parse_pss_kib(&text)
}

fn parse_pss_kib(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix("Pss:"))?
        .trim()
        .strip_suffix("kB")?
        .trim()
        .parse()
        .ok()
}

fn read_io(pid: i32) -> Option<(u64, u64)> {
    parse_io(&read_small(&format!("/proc/{pid}/io"))?)
}

/// Storage-level bytes, not `rchar`/`wchar`, which also count page-cache hits.
fn parse_io(text: &str) -> Option<(u64, u64)> {
    let field = |name: &str| -> Option<u64> {
        text.lines()
            .find_map(|l| l.strip_prefix(name))?
            .trim()
            .parse()
            .ok()
    };
    Some((field("read_bytes:")?, field("write_bytes:")?))
}

fn read_cgroup(pid: i32) -> Option<Arc<str>> {
    let text = read_small(&format!("/proc/{pid}/cgroup"))?;
    parse_cgroup(&text).map(Arc::from)
}

const CMDLINE_MAX: usize = 4096;

fn read_cmdline(pid: i32) -> Option<Arc<str>> {
    let mut buf = Vec::with_capacity(256);
    File::open(format!("/proc/{pid}/cmdline"))
        .ok()?
        .take(CMDLINE_MAX as u64)
        .read_to_end(&mut buf)
        .ok()?;
    parse_cmdline(&buf, buf.len() >= CMDLINE_MAX).map(Arc::from)
}

/// Arguments are NUL-separated. Kernel threads have none.
fn parse_cmdline(raw: &[u8], truncated: bool) -> Option<String> {
    let mut args: Vec<&[u8]> = raw.split(|b| *b == 0).collect();
    while args.last().is_some_and(|a| a.is_empty()) {
        args.pop();
    }
    if args.is_empty() {
        return None;
    }
    let mut out = args
        .iter()
        .map(|a| String::from_utf8_lossy(a))
        .collect::<Vec<_>>()
        .join(" ");
    if truncated {
        out.push('…');
    }
    Some(out)
}

/// cgroup v2 has a single `0::<path>` line; on a v1 or hybrid system there is none and
/// processes are grouped by name instead.
fn parse_cgroup(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| p.trim_end().to_string())
}

fn refresh_extras(
    pid: i32,
    rss_kib: u64,
    ticks: u64,
    round: u64,
    now: Instant,
    elapsed: Option<f64>,
    e: &mut Extras,
) {
    let stale = e.pss_at.is_none_or(|(t, rss_then)| {
        now - t >= PSS_MAX_AGE
            || (rss_kib.abs_diff(rss_then) as f64) > rss_then as f64 * PSS_RSS_DRIFT
    });
    if stale {
        // Back-date only the first read; doing it on every re-read would shorten the period.
        let jitter = if e.pss_at.is_none() {
            Duration::from_secs(u64::from(pid.unsigned_abs()) % PSS_MAX_AGE.as_secs())
        } else {
            Duration::ZERO
        };
        e.pss_kib = read_pss_kib(pid);
        e.pss_at = Some((now.checked_sub(jitter).unwrap_or(now), rss_kib));
    }

    if e.io.is_none() || ticks != e.io_ticks {
        let io = read_io(pid);
        (e.disk_read_bps, e.disk_write_bps) = match (e.io, io, elapsed) {
            (Some(p), Some(c), Some(dt)) if dt > 0.0 => (
                Some((c.0.saturating_sub(p.0) as f64 / dt) as u64),
                Some((c.1.saturating_sub(p.1) as f64 / dt) as u64),
            ),
            (_, Some(_), _) => (Some(0), Some(0)),
            _ => (None, None),
        };
        e.io = io;
        e.io_ticks = ticks;
    } else {
        (e.disk_read_bps, e.disk_write_bps) = (Some(0), Some(0));
    }

    let due = match e.gpu_checked {
        None => true,
        Some(_) => e.gpu_prev.is_some() || (round + pid as u64).is_multiple_of(GPU_RECHECK_ROUNDS),
    };
    if due {
        e.gpu_checked = Some(round);
        match gpu::read_counters(pid) {
            Ok(Some(cur)) => {
                e.gpu_percent = Some(match (&e.gpu_prev, elapsed) {
                    (Some(prev), Some(dt)) => gpu::utilisation(prev, &cur, dt),
                    _ => 0.0,
                });
                e.gpu_mem_kib = Some(cur.mem_kib);
                e.gpu_prev = Some(cur);
            }
            Ok(None) => {
                e.gpu_prev = None;
                e.gpu_percent = Some(0.0);
                e.gpu_mem_kib = Some(0);
            }
            Err(_) => {
                e.gpu_prev = None;
                e.gpu_percent = None;
                e.gpu_mem_kib = None;
            }
        }
    }
}

/// The fields of `/proc/<pid>/stat` the sampler uses.
#[derive(Debug, PartialEq)]
struct StatLine {
    pid: i32,
    ppid: i32,
    comm: String,
    state: char,
    utime: u64,
    stime: u64,
    num_threads: i64,
    starttime: u64,
    /// In pages.
    rss: u64,
}

/// Parses only what is needed from a stat line; `procfs::process::Stat` parses all fifty-odd
/// fields, which costs about a fifth of the sampling time with a thousand processes.
fn parse_stat(raw: &[u8]) -> Option<StatLine> {
    // comm may contain spaces and parentheses, so it ends at the last ')'.
    let open = raw.iter().position(|b| *b == b'(')?;
    let close = raw.iter().rposition(|b| *b == b')')?;
    let pid = std::str::from_utf8(raw.get(..open)?)
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let comm = String::from_utf8_lossy(raw.get(open + 1..close)?).into_owned();
    let rest = std::str::from_utf8(raw.get(close + 1..)?).ok()?;
    // Fields after comm, numbered from the state (field 3 in proc(5)).
    let mut f = rest.split_ascii_whitespace();
    let state = f.next()?.chars().next()?;
    let ppid = f.next()?.parse().ok()?;
    let mut f = f.skip(9); // pgrp .. cmajflt (fields 5..13)
    let utime = f.next()?.parse().ok()?; // 14
    let stime = f.next()?.parse().ok()?; // 15
    let mut f = f.skip(4); // cutime .. nice (16..19)
    let num_threads = f.next()?.parse().ok()?; // 20
    let mut f = f.skip(1); // itrealvalue (21)
    let starttime = f.next()?.parse().ok()?; // 22
    let mut f = f.skip(1); // vsize (23)
    let rss = f.next()?.parse::<i64>().ok()?.max(0) as u64; // 24
    Some(StatLine {
        pid,
        ppid,
        comm,
        state,
        utime,
        stime,
        num_threads,
        starttime,
        rss,
    })
}

/// Reads and parses `/proc/<pid>/stat` with one open, one read and one close.
/// `procfs::process::Process` costs about three times as many syscalls per process,
/// which dominates the sampling cost with a thousand processes.
fn read_stat(pid: i32, path: &mut String, buf: &mut [u8; 4096]) -> Option<StatLine> {
    use std::fmt::Write;
    // The stat line is far below 4 KiB (comm is at most 16 bytes); a full buffer means
    // something unexpected, so skip it. The caller owns the buffer so that it is zeroed once
    // per sweep, not once per process.
    path.clear();
    let _ = write!(path, "/proc/{pid}/stat");
    let n = File::open(&*path).ok()?.read(&mut buf[..]).ok()?;
    if n == buf.len() {
        return None;
    }
    parse_stat(&buf[..n])
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
    extras_interval: Option<Duration>,
    last_extras: Option<Instant>,
    extras_round: u64,
    net: NetTracker,
}

impl Sampler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets how often PSS, disk I/O and GPU readings are refreshed (default 5 s).
    pub fn set_extras_interval(&mut self, every: Duration) {
        self.extras_interval = Some(every);
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

        // Others' processes only expose these files to root, so don't even try.
        let my_uid = fs::metadata("/proc/self").ok().map(|m| m.uid());
        let now = Instant::now();
        let every = self.extras_interval.unwrap_or(DEFAULT_EXTRAS_INTERVAL);
        let extras_due = self.last_extras.is_none_or(|t| now - t >= every);
        let elapsed = self.last_extras.map(|t| (now - t).as_secs_f64());
        if extras_due {
            self.extras_round += 1;
        }
        let round = self.extras_round;

        let page_kib = procfs::page_size() / 1024;
        let mut next_procs = HashMap::with_capacity(self.prev_procs.len());
        let mut processes = Vec::with_capacity(self.prev_procs.len());
        let mut path = String::with_capacity(32);
        let mut buf = [0u8; 4096];
        let mut net_scan: Vec<i32> = Vec::new();
        let mut keys: Vec<(i32, u64)> = Vec::with_capacity(self.prev_procs.len());
        for pid in list_pids()? {
            // Processes can exit between listing and reading; skip them.
            let Some(stat) = read_stat(pid, &mut path, &mut buf) else {
                continue;
            };
            let ticks = stat.utime + stat.stime;
            let key = (stat.pid, stat.starttime);
            let prev = self.prev_procs.remove(&key);
            let cpu = match (&prev, denom > 0.0) {
                (Some(prev), true) => ticks.saturating_sub(prev.ticks) as f64 / denom * 100.0,
                _ => 0.0,
            };
            let (uid, cgroup, cmdline, mut extras, fresh) = match prev {
                Some(p) => (p.uid, p.cgroup, p.cmdline, p.extras, false),
                None => (
                    fs::metadata(format!("/proc/{pid}")).ok().map(|m| m.uid()),
                    read_cgroup(pid),
                    read_cmdline(pid),
                    Extras::default(),
                    true,
                ),
            };
            // Kernel threads have no memory to inspect.
            let rss_kib = stat.rss * page_kib;
            let inspectable = stat.rss > 0 && uid.is_some() && (my_uid == Some(0) || uid == my_uid);
            if inspectable && (extras_due || fresh) {
                // A process that appeared since the last round has no baseline to diff against.
                refresh_extras(
                    pid,
                    rss_kib,
                    ticks,
                    round,
                    now,
                    if fresh { None } else { elapsed },
                    &mut extras,
                );
            }
            if inspectable && extras_due {
                net_scan.push(pid);
            }
            keys.push(key);
            processes.push(ProcessInfo {
                pid: stat.pid,
                ppid: stat.ppid,
                name: stat.comm,
                state: stat.state,
                threads: stat.num_threads,
                uid,
                rss_kib,
                cpu_percent: cpu,
                pss_kib: extras.pss_kib,
                disk_read_bps: extras.disk_read_bps,
                disk_write_bps: extras.disk_write_bps,
                gpu_percent: extras.gpu_percent,
                gpu_mem_kib: extras.gpu_mem_kib,
                net_recv_bps: extras.net.map(|n| n.0),
                net_send_bps: extras.net.map(|n| n.1),
                cgroup: cgroup.clone(),
                cmdline: cmdline.clone(),
            });
            next_procs.insert(
                key,
                ProcPrev {
                    ticks,
                    uid,
                    cgroup,
                    cmdline,
                    extras,
                },
            );
        }

        if extras_due {
            // `None` from the tracker means no sock_diag: every process then shows no value.
            let rates = self.net.update(&net_scan, elapsed);
            for (p, key) in processes.iter_mut().zip(&keys) {
                if let Some(e) = next_procs.get_mut(key) {
                    e.extras.net = rates.as_ref().and_then(|r| r.get(&p.pid).copied());
                    p.net_recv_bps = e.extras.net.map(|n| n.0);
                    p.net_send_bps = e.extras.net.map(|n| n.1);
                }
            }
        }

        let mi = Meminfo::current()?;
        let mem = MemInfo {
            total_kib: mi.mem_total / 1024,
            available_kib: mi.mem_available.unwrap_or(mi.mem_free) / 1024,
            swap_total_kib: mi.swap_total / 1024,
            swap_free_kib: mi.swap_free / 1024,
        };

        if extras_due {
            self.last_extras = Some(now);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stat_line() {
        let line = b"1234 (my (odd) app) S 1 1234 1234 0 -1 4194560 100 0 0 0 11 22 0 0 20 0 7 0 5555 1000000 321 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0\n";
        assert_eq!(
            parse_stat(line),
            Some(StatLine {
                pid: 1234,
                ppid: 1,
                comm: "my (odd) app".into(),
                state: 'S',
                utime: 11,
                stime: 22,
                num_threads: 7,
                starttime: 5555,
                rss: 321,
            })
        );
        assert_eq!(parse_stat(b"1 (x) S 1 2"), None);
        assert_eq!(parse_stat(b""), None);
    }

    #[test]
    fn parses_pss() {
        let text = "55a-55b ---p 0 [rollup]\nRss:                1000 kB\nPss:                 512 kB\nPss_Anon: 5 kB\n";
        assert_eq!(parse_pss_kib(text), Some(512));
        assert_eq!(parse_pss_kib("Rss: 1 kB\n"), None);
    }

    #[test]
    fn parses_io_storage_bytes() {
        let text = "rchar: 10\nwchar: 20\nsyscr: 1\nsyscw: 1\nread_bytes: 4096\nwrite_bytes: 8192\ncancelled_write_bytes: 0\n";
        assert_eq!(parse_io(text), Some((4096, 8192)));
        assert_eq!(parse_io("rchar: 1\n"), None);
    }

    #[test]
    fn parses_cmdline() {
        assert_eq!(
            parse_cmdline(b"/usr/bin/foo\0--bar\0baz\0", false).as_deref(),
            Some("/usr/bin/foo --bar baz")
        );
        assert_eq!(parse_cmdline(b"", false), None);
        assert_eq!(parse_cmdline(b"abc\0", true).as_deref(), Some("abc…"));
    }

    #[test]
    fn parses_unified_cgroup() {
        assert_eq!(
            parse_cgroup("0::/user.slice/a.scope\n").as_deref(),
            Some("/user.slice/a.scope")
        );
        assert_eq!(parse_cgroup("12:cpu:/x\n"), None);
    }
}

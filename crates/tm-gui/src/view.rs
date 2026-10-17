//! Presentation logic kept free of Slint types so it can be unit tested.

use std::collections::{HashMap, VecDeque};
use tm_core::ProcessInfo;

/// uid -> login name, from `/etc/passwd`. Unknown uids show as their number.
#[derive(Default)]
pub struct UserNames(HashMap<u32, String>);

impl UserNames {
    pub fn load() -> Self {
        Self::parse(&std::fs::read_to_string("/etc/passwd").unwrap_or_default())
    }

    pub fn parse(passwd: &str) -> Self {
        let map = passwd
            .lines()
            .filter_map(|l| {
                let mut f = l.split(':');
                let name = f.next()?;
                let uid = f.nth(1)?.parse().ok()?;
                Some((uid, name.to_string()))
            })
            .collect();
        Self(map)
    }

    pub fn name(&self, uid: Option<u32>) -> String {
        match uid {
            Some(u) => self.0.get(&u).cloned().unwrap_or_else(|| u.to_string()),
            None => "?".to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortColumn {
    Name,
    Pid,
    User,
    Cpu,
    /// Proportional set size.
    Memory,
    Resident,
    Disk,
    Gpu,
    Threads,
}

impl SortColumn {
    /// Column order of the Processes table in `main.slint`.
    pub fn from_index(i: i32) -> Option<Self> {
        Some(match i {
            0 => Self::Name,
            1 => Self::Pid,
            2 => Self::User,
            3 => Self::Cpu,
            4 => Self::Memory,
            5 => Self::Resident,
            6 => Self::Disk,
            7 => Self::Gpu,
            8 => Self::Threads,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sort {
    pub column: SortColumn,
    pub ascending: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Self {
            column: SortColumn::Cpu,
            ascending: false,
        }
    }
}

fn disk_bps(p: &ProcessInfo) -> u64 {
    p.disk_read_bps.unwrap_or(0) + p.disk_write_bps.unwrap_or(0)
}

/// Sorts in place. Ties fall back to PID so the order is stable between refreshes.
pub fn sort_processes(procs: &mut [ProcessInfo], users: &UserNames, sort: Sort) {
    match sort.column {
        SortColumn::Name => procs.sort_by_cached_key(|p| (p.name.to_lowercase(), p.pid)),
        SortColumn::User => procs.sort_by_cached_key(|p| (users.name(p.uid).to_lowercase(), p.pid)),
        SortColumn::Pid => procs.sort_by_key(|p| p.pid),
        SortColumn::Threads => procs.sort_by_key(|p| (p.threads, p.pid)),
        SortColumn::Memory => procs.sort_by_key(|p| (p.pss_kib.unwrap_or(0), p.pid)),
        SortColumn::Resident => procs.sort_by_key(|p| (p.rss_kib, p.pid)),
        SortColumn::Disk => procs.sort_by_key(|p| (disk_bps(p), p.pid)),
        SortColumn::Gpu => procs.sort_by(|a, b| {
            a.gpu_percent
                .unwrap_or(0.0)
                .total_cmp(&b.gpu_percent.unwrap_or(0.0))
                .then(a.pid.cmp(&b.pid))
        }),
        SortColumn::Cpu => procs.sort_by(|a, b| {
            a.cpu_percent
                .total_cmp(&b.cpu_percent)
                .then(a.pid.cmp(&b.pid))
        }),
    }
    if !sort.ascending {
        procs.reverse();
    }
}

#[derive(Debug, PartialEq)]
pub struct UserRow {
    pub name: String,
    pub processes: usize,
    pub cpu_percent: f64,
    pub rss_kib: u64,
}

pub fn aggregate_users(procs: &[ProcessInfo], users: &UserNames) -> Vec<UserRow> {
    let mut by_uid: HashMap<Option<u32>, UserRow> = HashMap::new();
    for p in procs {
        let row = by_uid.entry(p.uid).or_insert_with(|| UserRow {
            name: users.name(p.uid),
            processes: 0,
            cpu_percent: 0.0,
            rss_kib: 0,
        });
        row.processes += 1;
        row.cpu_percent += p.cpu_percent;
        row.rss_kib += p.rss_kib;
    }
    let mut rows: Vec<UserRow> = by_uid.into_values().collect();
    rows.sort_by(|a, b| {
        b.cpu_percent
            .total_cmp(&a.cpu_percent)
            .then(b.rss_kib.cmp(&a.rss_kib))
            .then_with(|| a.name.cmp(&b.name))
    });
    rows
}

/// "1,234,567 K", as the Windows 7 memory column shows it.
pub fn format_kib(kib: u64) -> String {
    let digits = kib.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 2);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out.push_str(" K");
    out
}

pub fn format_percent(p: f64) -> String {
    format!("{p:.1}")
}

/// Shown where a value exists but this user may not read it.
pub const UNKNOWN: &str = "—";

/// "1.5 MB/s". Decimal units, as storage vendors and Windows show them.
pub fn format_rate(bps: u64) -> String {
    let b = bps as f64;
    if b < 1000.0 {
        format!("{bps} B/s")
    } else if b < 1e6 {
        format!("{:.1} KB/s", b / 1e3)
    } else if b < 1e9 {
        format!("{:.1} MB/s", b / 1e6)
    } else {
        format!("{:.1} GB/s", b / 1e9)
    }
}

pub fn process_row(p: &ProcessInfo, users: &UserNames) -> [String; 9] {
    let disk = match (p.disk_read_bps, p.disk_write_bps) {
        (None, None) => UNKNOWN.to_string(),
        _ => format_rate(disk_bps(p)),
    };
    [
        p.name.clone(),
        p.pid.to_string(),
        users.name(p.uid),
        format_percent(p.cpu_percent),
        p.pss_kib.map_or(UNKNOWN.to_string(), format_kib),
        format_kib(p.rss_kib),
        disk,
        p.gpu_percent
            .map_or(UNKNOWN.to_string(), |g| format!("{g:.0}%")),
        p.threads.to_string(),
    ]
}

pub fn user_row(u: &UserRow) -> [String; 4] {
    [
        u.name.clone(),
        u.processes.to_string(),
        format_percent(u.cpu_percent),
        format_kib(u.rss_kib),
    ]
}

/// Fixed-length window of 0..=100 samples drawn right-aligned, newest on the right.
pub struct History {
    cap: usize,
    values: VecDeque<f64>,
}

impl History {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            values: VecDeque::with_capacity(cap),
        }
    }

    pub fn push(&mut self, v: f64) {
        if self.values.len() == self.cap {
            self.values.pop_front();
        }
        self.values.push_back(v.clamp(0.0, 100.0));
    }

    /// SVG path in a 100x100 box for a filled area graph; empty until there are two samples.
    pub fn area_path(&self) -> String {
        let n = self.values.len();
        if n < 2 {
            return String::new();
        }
        let step = 100.0 / (self.cap - 1) as f64;
        let x = |i: usize| 100.0 - (n - 1 - i) as f64 * step;
        let mut s = format!("M {:.2} 100", x(0));
        for (i, v) in self.values.iter().enumerate() {
            s.push_str(&format!(" L {:.2} {:.2}", x(i), 100.0 - v));
        }
        s.push_str(&format!(" L {:.2} 100 Z", x(n - 1)));
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: i32, name: &str, uid: u32, cpu: f64, rss: u64) -> ProcessInfo {
        ProcessInfo {
            pid,
            ppid: 1,
            name: name.into(),
            state: 'S',
            threads: 1,
            uid: Some(uid),
            rss_kib: rss,
            cpu_percent: cpu,
            pss_kib: None,
            disk_read_bps: None,
            disk_write_bps: None,
            gpu_percent: None,
            gpu_mem_kib: None,
            cgroup: None,
        }
    }

    fn users() -> UserNames {
        UserNames::parse(
            "root:x:0:0:root:/root:/bin/bash\nnoah:x:1000:1000::/home/noah:/bin/sh\nbad line\n",
        )
    }

    #[test]
    fn parses_passwd_and_falls_back_to_uid() {
        let u = users();
        assert_eq!(u.name(Some(0)), "root");
        assert_eq!(u.name(Some(1000)), "noah");
        assert_eq!(u.name(Some(4242)), "4242");
        assert_eq!(u.name(None), "?");
    }

    #[test]
    fn sorts_by_each_column_in_both_directions() {
        let u = users();
        let base = vec![
            proc(3, "bash", 1000, 1.0, 500),
            proc(1, "Zsh", 0, 9.0, 100),
            proc(2, "awk", 1000, 1.0, 900),
        ];
        let pids = |col, asc| {
            let mut v = base.clone();
            sort_processes(
                &mut v,
                &u,
                Sort {
                    column: col,
                    ascending: asc,
                },
            );
            v.iter().map(|p| p.pid).collect::<Vec<_>>()
        };
        assert_eq!(pids(SortColumn::Cpu, false), [1, 3, 2]);
        assert_eq!(pids(SortColumn::Cpu, true), [2, 3, 1]);
        assert_eq!(pids(SortColumn::Name, true), [2, 3, 1], "case-insensitive");
        assert_eq!(pids(SortColumn::Pid, true), [1, 2, 3]);
        assert_eq!(pids(SortColumn::Resident, false), [2, 3, 1]);
        assert_eq!(pids(SortColumn::User, true), [2, 3, 1]);
    }

    #[test]
    fn aggregates_per_user() {
        let u = users();
        let rows = aggregate_users(
            &[
                proc(1, "a", 0, 2.0, 10),
                proc(2, "b", 1000, 3.0, 20),
                proc(3, "c", 1000, 4.0, 30),
            ],
            &u,
        );
        assert_eq!(
            rows,
            vec![
                UserRow {
                    name: "noah".into(),
                    processes: 2,
                    cpu_percent: 7.0,
                    rss_kib: 50
                },
                UserRow {
                    name: "root".into(),
                    processes: 1,
                    cpu_percent: 2.0,
                    rss_kib: 10
                },
            ]
        );
    }

    #[test]
    fn formats_numbers() {
        assert_eq!(format_kib(0), "0 K");
        assert_eq!(format_kib(999), "999 K");
        assert_eq!(format_kib(1_234_567), "1,234,567 K");
        assert_eq!(format_percent(0.04), "0.0");
        assert_eq!(format_rate(999), "999 B/s");
        assert_eq!(format_rate(1_500), "1.5 KB/s");
        assert_eq!(format_rate(2_500_000), "2.5 MB/s");
        assert_eq!(format_rate(3_000_000_000), "3.0 GB/s");
    }

    #[test]
    fn unreadable_values_show_a_dash_and_readable_ones_do_not() {
        let u = users();
        let mut p = proc(1, "a", 0, 0.0, 100);
        let row = process_row(&p, &u);
        assert_eq!(
            (row[4].as_str(), row[6].as_str(), row[7].as_str()),
            (UNKNOWN, UNKNOWN, UNKNOWN)
        );
        p.pss_kib = Some(1234);
        p.disk_read_bps = Some(1_000);
        p.disk_write_bps = Some(500);
        p.gpu_percent = Some(12.4);
        let row = process_row(&p, &u);
        assert_eq!(row[4], "1,234 K");
        assert_eq!(row[6], "1.5 KB/s");
        assert_eq!(row[7], "12%");
    }

    #[test]
    fn sorts_by_the_new_columns() {
        let u = users();
        let mut a = proc(1, "a", 0, 0.0, 10);
        let mut b = proc(2, "b", 0, 0.0, 10);
        a.pss_kib = Some(5);
        b.pss_kib = Some(50);
        a.disk_write_bps = Some(900);
        b.disk_read_bps = Some(100);
        a.gpu_percent = Some(80.0);
        let order = |col| {
            let mut v = vec![a.clone(), b.clone()];
            sort_processes(
                &mut v,
                &u,
                Sort {
                    column: col,
                    ascending: false,
                },
            );
            v.iter().map(|p| p.pid).collect::<Vec<_>>()
        };
        assert_eq!(order(SortColumn::Memory), [2, 1]);
        assert_eq!(order(SortColumn::Disk), [1, 2]);
        assert_eq!(order(SortColumn::Gpu), [1, 2]);
    }

    #[test]
    fn history_path_is_right_aligned_and_bounded() {
        let mut h = History::new(5);
        assert_eq!(h.area_path(), "");
        h.push(50.0);
        assert_eq!(h.area_path(), "", "one sample is not a line");
        h.push(150.0);
        assert_eq!(
            h.area_path(),
            "M 75.00 100 L 75.00 50.00 L 100.00 0.00 L 100.00 100 Z"
        );
        for _ in 0..10 {
            h.push(0.0);
        }
        assert!(h.area_path().starts_with("M 0.00 100"));
        assert_eq!(h.values.len(), 5);
    }
}

//! Presentation logic kept free of Slint types so it can be unit tested.

use std::collections::{HashMap, VecDeque};
use tm_core::{ProcessInfo, ServiceInfo, StartupEntry, service_unit_from_cgroup};

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
    Network,
    Gpu,
    Threads,
    Status,
    CommandLine,
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
            7 => Self::Network,
            8 => Self::Gpu,
            9 => Self::Threads,
            _ => return None,
        })
    }

    /// Column order of the Details table in `main.slint`.
    pub fn from_details_index(i: i32) -> Option<Self> {
        Some(match i {
            0 => Self::Name,
            1 => Self::Pid,
            2 => Self::Status,
            3 => Self::User,
            4 => Self::Cpu,
            5 => Self::Memory,
            6 => Self::Disk,
            7 => Self::Network,
            8 => Self::Gpu,
            9 => Self::Threads,
            10 => Self::CommandLine,
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

fn net_bps(p: &ProcessInfo) -> u64 {
    p.net_recv_bps.unwrap_or(0) + p.net_send_bps.unwrap_or(0)
}

fn disk_bps(p: &ProcessInfo) -> u64 {
    p.disk_read_bps.unwrap_or(0) + p.disk_write_bps.unwrap_or(0)
}

/// Sorts in place. Ties fall back to PID so the order is stable between refreshes.
pub fn sort_processes<T: AsRef<ProcessInfo>>(procs: &mut [T], users: &UserNames, sort: Sort) {
    match sort.column {
        SortColumn::Name => {
            procs.sort_by_cached_key(|p| (p.as_ref().name.to_lowercase(), p.as_ref().pid))
        }
        SortColumn::User => procs
            .sort_by_cached_key(|p| (users.name(p.as_ref().uid).to_lowercase(), p.as_ref().pid)),
        SortColumn::Pid => procs.sort_by_key(|p| p.as_ref().pid),
        SortColumn::Threads => procs.sort_by_key(|p| (p.as_ref().threads, p.as_ref().pid)),
        SortColumn::Memory => {
            procs.sort_by_key(|p| (p.as_ref().pss_kib.unwrap_or(0), p.as_ref().pid))
        }
        SortColumn::Resident => procs.sort_by_key(|p| (p.as_ref().rss_kib, p.as_ref().pid)),
        SortColumn::Disk => procs.sort_by_key(|p| (disk_bps(p.as_ref()), p.as_ref().pid)),
        SortColumn::Network => procs.sort_by_key(|p| (net_bps(p.as_ref()), p.as_ref().pid)),
        SortColumn::Gpu => procs.sort_by(|a, b| {
            let (a, b) = (a.as_ref(), b.as_ref());
            a.gpu_percent
                .unwrap_or(0.0)
                .total_cmp(&b.gpu_percent.unwrap_or(0.0))
                .then(a.pid.cmp(&b.pid))
        }),
        SortColumn::Status => procs.sort_by_key(|p| (p.as_ref().state, p.as_ref().pid)),
        SortColumn::CommandLine => procs.sort_by_cached_key(|p| {
            let p = p.as_ref();
            (p.cmdline.as_deref().unwrap_or("").to_lowercase(), p.pid)
        }),
        SortColumn::Cpu => procs.sort_by(|a, b| {
            let (a, b) = (a.as_ref(), b.as_ref());
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

pub fn process_row(p: &ProcessInfo, users: &UserNames) -> [String; 10] {
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
        match (p.net_recv_bps, p.net_send_bps) {
            (None, None) => UNKNOWN.to_string(),
            _ => format_rate(net_bps(p)),
        },
        p.gpu_percent
            .map_or(UNKNOWN.to_string(), |g| format!("{g:.0}%")),
        p.threads.to_string(),
    ]
}

/// One row of the Processes table: a single process, or all processes of one application
/// owned by one user. `info` holds the sums (or, for a lone process, the process itself) and
/// carries the lowest member PID, which keys the row.
pub struct Group {
    pub info: ProcessInfo,
    pub pids: Vec<i32>,
}

impl AsRef<ProcessInfo> for Group {
    fn as_ref(&self) -> &ProcessInfo {
        &self.info
    }
}

fn sum_opt<T: Copy + std::ops::Add<Output = T>>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a + b),
        (x, None) | (None, x) => x,
    }
}

/// With `grouped` off every process is its own group.
pub fn group_processes(procs: &[ProcessInfo], grouped: bool) -> Vec<Group> {
    let single = |p: &ProcessInfo| Group {
        info: p.clone(),
        pids: vec![p.pid],
    };
    if !grouped {
        return procs.iter().map(single).collect();
    }
    let mut by_app: HashMap<(String, Option<u32>), Vec<&ProcessInfo>> = HashMap::new();
    for p in procs {
        by_app.entry((p.app_name(), p.uid)).or_default().push(p);
    }
    by_app
        .into_iter()
        .map(|((app, _), members)| {
            let [only] = members[..] else {
                return merge(app, &members);
            };
            single(only)
        })
        .collect()
}

fn merge(app: String, members: &[&ProcessInfo]) -> Group {
    let first = members
        .iter()
        .min_by_key(|p| p.pid)
        .expect("non-empty group");
    let mut info = ProcessInfo {
        name: app,
        cmdline: None,
        threads: 0,
        rss_kib: 0,
        cpu_percent: 0.0,
        pss_kib: None,
        disk_read_bps: None,
        disk_write_bps: None,
        gpu_percent: None,
        gpu_mem_kib: None,
        net_recv_bps: None,
        net_send_bps: None,
        ..(*first).clone()
    };
    let mut pids = Vec::with_capacity(members.len());
    for p in members {
        pids.push(p.pid);
        info.threads += p.threads;
        info.rss_kib += p.rss_kib;
        info.cpu_percent += p.cpu_percent;
        info.pss_kib = sum_opt(info.pss_kib, p.pss_kib);
        info.disk_read_bps = sum_opt(info.disk_read_bps, p.disk_read_bps);
        info.disk_write_bps = sum_opt(info.disk_write_bps, p.disk_write_bps);
        info.gpu_percent = sum_opt(info.gpu_percent, p.gpu_percent);
        info.gpu_mem_kib = sum_opt(info.gpu_mem_kib, p.gpu_mem_kib);
        info.net_recv_bps = sum_opt(info.net_recv_bps, p.net_recv_bps);
        info.net_send_bps = sum_opt(info.net_send_bps, p.net_send_bps);
    }
    info.gpu_percent = info.gpu_percent.map(|g| g.min(100.0));
    pids.sort_unstable();
    Group { info, pids }
}

/// "Idle" is the kernel's name for an idle kernel thread.
pub fn status_text(state: char) -> &'static str {
    match state {
        'R' => "Running",
        'S' => "Sleeping",
        'D' => "Disk sleep",
        'Z' => "Zombie",
        'T' => "Stopped",
        't' => "Tracing stop",
        'X' | 'x' => "Dead",
        'I' => "Idle",
        _ => "Unknown",
    }
}

pub fn group_row(g: &Group, users: &UserNames) -> [String; 10] {
    let mut row = process_row(&g.info, users);
    if g.pids.len() > 1 {
        row[0] = format!("{} ({})", g.info.name, g.pids.len());
        row[1] = String::new();
    }
    row
}

pub fn detail_row(p: &ProcessInfo, users: &UserNames) -> [String; 11] {
    let r = process_row(p, users);
    [
        r[0].clone(),
        r[1].clone(),
        status_text(p.state).to_string(),
        r[2].clone(),
        r[3].clone(),
        r[4].clone(),
        r[6].clone(),
        r[7].clone(),
        r[8].clone(),
        r[9].clone(),
        p.cmdline.as_deref().unwrap_or("").to_string(),
    ]
}

/// One row of the Services table.
#[derive(Debug, PartialEq)]
pub struct ServiceRow {
    pub name: String,
    /// Lowest PID in the service's cgroup.
    pub pid: Option<i32>,
    pub description: String,
    pub status: String,
}

pub fn service_rows(services: &[ServiceInfo], procs: &[ProcessInfo]) -> Vec<ServiceRow> {
    let mut pid_of: HashMap<String, i32> = HashMap::new();
    for p in procs {
        if let Some(unit) = p.cgroup.as_deref().and_then(service_unit_from_cgroup) {
            let e = pid_of.entry(unit).or_insert(p.pid);
            *e = (*e).min(p.pid);
        }
    }
    services
        .iter()
        .map(|s| ServiceRow {
            pid: pid_of.get(&s.name).copied(),
            name: s.name.clone(),
            description: s.description.clone(),
            status: s.status().to_string(),
        })
        .collect()
}

/// Column order of the Services table in `main.slint`: Name, PID, Description, Status.
pub fn sort_services(rows: &mut [ServiceRow], column: i32, ascending: bool) {
    match column {
        1 => rows.sort_by(|a, b| a.pid.cmp(&b.pid).then_with(|| a.name.cmp(&b.name))),
        2 => rows.sort_by_cached_key(|r| (r.description.to_lowercase(), r.name.clone())),
        3 => rows.sort_by(|a, b| a.status.cmp(&b.status).then_with(|| a.name.cmp(&b.name))),
        _ => rows.sort_by_cached_key(|r| r.name.to_lowercase()),
    }
    if !ascending {
        rows.reverse();
    }
}

pub fn service_cells(r: &ServiceRow) -> [String; 4] {
    [
        r.name.clone(),
        r.pid.map_or(String::new(), |p| p.to_string()),
        r.description.clone(),
        r.status.clone(),
    ]
}

pub fn startup_cells(e: &StartupEntry) -> [String; 4] {
    [
        e.name.clone(),
        e.exec.clone(),
        if e.enabled { "Enabled" } else { "Disabled" }.to_string(),
        if e.user { "User" } else { "System" }.to_string(),
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
            net_recv_bps: None,
            net_send_bps: None,
            cgroup: None,
            cmdline: None,
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

    fn app_proc(pid: i32, name: &str, uid: u32, app: &str) -> ProcessInfo {
        let mut p = proc(pid, name, uid, 1.0, 100);
        p.cgroup = Some(format!("/user.slice/app.slice/app-gnome-{app}-1.scope").into());
        p.threads = 2;
        p
    }

    #[test]
    fn groups_by_application_and_owner() {
        let mut a = app_proc(10, "firefox-bin", 1000, "firefox");
        let mut b = app_proc(12, "Web Content", 1000, "firefox");
        a.pss_kib = Some(100);
        b.pss_kib = Some(50);
        a.gpu_percent = Some(70.0);
        b.gpu_percent = Some(60.0);
        a.disk_read_bps = Some(5);
        let other_user = app_proc(14, "firefox-bin", 1001, "firefox");
        let lone = proc(20, "bash", 1000, 0.5, 10);
        let mut groups = group_processes(&[b, a, other_user, lone.clone()], true);
        groups.sort_by_key(|g| g.info.pid);
        assert_eq!(groups.len(), 3);
        let ff = &groups[0];
        assert_eq!(ff.pids, [10, 12]);
        assert_eq!(ff.info.pid, 10);
        assert_eq!(ff.info.name, "firefox");
        assert_eq!(ff.info.rss_kib, 200);
        assert_eq!(ff.info.threads, 4);
        assert_eq!(ff.info.pss_kib, Some(150));
        assert_eq!(ff.info.gpu_percent, Some(100.0), "capped");
        assert_eq!(ff.info.disk_read_bps, Some(5));
        assert_eq!(ff.info.disk_write_bps, None, "nobody could read it");
        let row = group_row(ff, &users());
        assert_eq!((row[0].as_str(), row[1].as_str()), ("firefox (2)", ""));
        assert_eq!(groups[1].pids, [14], "same app, other owner");
        assert_eq!(
            group_row(&groups[2], &users())[0],
            "bash",
            "lone process keeps its name"
        );
        assert_eq!(group_processes(&[lone], true)[0].info.name, "bash");
    }

    #[test]
    fn ungrouped_keeps_every_process() {
        let ps = [app_proc(1, "a", 0, "x"), app_proc(2, "b", 0, "x")];
        assert_eq!(group_processes(&ps, false).len(), 2);
    }

    #[test]
    fn details_row_has_status_and_command_line() {
        let mut p = proc(5, "sleep", 0, 0.0, 10);
        p.cmdline = Some("sleep 60".into());
        let r = detail_row(&p, &users());
        assert_eq!(r[2], "Sleeping");
        assert_eq!(r[3], "root");
        assert_eq!(r[10], "sleep 60");
        assert_eq!(
            SortColumn::from_details_index(10),
            Some(SortColumn::CommandLine)
        );
        assert_eq!(SortColumn::from_details_index(11), None);
    }

    #[test]
    fn services_get_the_lowest_pid_of_their_cgroup() {
        let mut a = proc(30, "cron", 0, 0.0, 1);
        a.cgroup = Some("/system.slice/cron.service".into());
        let mut b = proc(20, "cron", 0, 0.0, 1);
        b.cgroup = Some("/system.slice/cron.service".into());
        let svc = |n: &str, active: &str, sub: &str| ServiceInfo {
            name: n.into(),
            description: format!("{n} d"),
            active_state: active.into(),
            sub_state: sub.into(),
        };
        let mut rows = service_rows(
            &[
                svc("cron", "active", "running"),
                svc("fstrim", "inactive", "dead"),
            ],
            &[a, b],
        );
        assert_eq!(rows[0].pid, Some(20));
        assert_eq!(rows[1].pid, None);
        assert_eq!(service_cells(&rows[0]), ["cron", "20", "cron d", "Running"]);
        assert_eq!(service_cells(&rows[1])[1], "");
        sort_services(&mut rows, 3, true);
        assert_eq!(rows[0].name, "cron", "Running sorts before Stopped");
        sort_services(&mut rows, 0, false);
        assert_eq!(rows[0].name, "fstrim");
    }

    #[test]
    fn startup_cells_show_status_and_origin() {
        let e = StartupEntry {
            id: "a.desktop".into(),
            name: "A".into(),
            exec: "a --x".into(),
            enabled: false,
            user: true,
        };
        assert_eq!(startup_cells(&e), ["A", "a --x", "Disabled", "User"]);
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
            (
                row[4].as_str(),
                row[6].as_str(),
                row[7].as_str(),
                row[8].as_str()
            ),
            (UNKNOWN, UNKNOWN, UNKNOWN, UNKNOWN)
        );
        p.pss_kib = Some(1234);
        p.disk_read_bps = Some(1_000);
        p.disk_write_bps = Some(500);
        p.gpu_percent = Some(12.4);
        p.net_recv_bps = Some(2_000_000);
        p.net_send_bps = Some(500_000);
        let row = process_row(&p, &u);
        assert_eq!(row[4], "1,234 K");
        assert_eq!(row[6], "1.5 KB/s");
        assert_eq!(row[7], "2.5 MB/s");
        assert_eq!(row[8], "12%");
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
        b.net_recv_bps = Some(10);
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
        assert_eq!(order(SortColumn::Network), [2, 1]);
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

mod kill;
mod rows;
mod view;

use rows::{KeyedRows, Row, RowCache};
use slint::{Model, ModelRc, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tm_core::{Sampler, ServiceInfo, Snapshot, StartupDirs, StartupEntry};
use view::{Group, History, Sort, SortColumn, UserNames};

slint::include_modules!();

const HISTORY_LEN: usize = 60;
/// Order matches the "Refresh" combo box in `main.slint`.
const INTERVALS: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
];

/// Order of the tabs in `main.slint`. Only the visible one is rendered each refresh.
const TAB_PROCESSES: i32 = 0;
const TAB_PERFORMANCE: i32 = 1;
const TAB_STARTUP: i32 = 2;
const TAB_USERS: i32 = 3;
const TAB_DETAILS: i32 = 4;
const TAB_SERVICES: i32 = 5;

/// The service list comes over D-Bus, so it is fetched off the UI thread and at most this often.
const SERVICES_EVERY: Duration = Duration::from_secs(5);

type ServicesReply = Result<Vec<ServiceInfo>, String>;

struct Services {
    list: Vec<ServiceInfo>,
    error: Option<String>,
    /// Column and direction; starts at Name, A to Z.
    sort: (i32, bool),
    fetched_at: Option<Instant>,
    in_flight: bool,
    /// Where the fetch thread leaves its result for `on_services_ready` to pick up.
    mailbox: Arc<Mutex<Option<ServicesReply>>>,
}

impl Default for Services {
    fn default() -> Self {
        Self {
            list: Vec::new(),
            error: None,
            sort: (0, true),
            fetched_at: None,
            in_flight: false,
            mailbox: Arc::default(),
        }
    }
}

/// A process table as the user sees it: how it is sorted and which row is selected.
#[derive(Default)]
struct TableState {
    sort: Sort,
    /// Row key (the lowest PID of the row) of the selected row.
    selected: Option<i32>,
    /// Row keys in the order the table currently shows them.
    order: Vec<i32>,
}

impl TableState {
    /// Keeps the selection on the same row as rows move; drops it if the row is gone.
    fn selected_index(&mut self) -> i32 {
        let idx = self
            .selected
            .and_then(|k| self.order.iter().position(|o| *o == k));
        if idx.is_none() {
            self.selected = None;
        }
        idx.map_or(-1, |i| i as i32)
    }

    fn select(&mut self, row: i32) {
        self.selected = usize::try_from(row)
            .ok()
            .and_then(|i| self.order.get(i).copied());
    }
}

struct Startup {
    dirs: StartupDirs,
    list: Vec<StartupEntry>,
    /// File name of the selected entry; survives the list being re-read.
    selected: Option<String>,
}

struct State {
    sampler: Sampler,
    users: UserNames,
    tab: i32,
    grouped: bool,
    /// Processes tab (index 0) and Details tab (index 1).
    tables: [TableState; 2],
    /// Rows of the Processes table, for ending every process of a group.
    groups: Vec<Group>,
    /// PIDs the open confirmation dialog will end.
    pending_kill: Vec<i32>,
    last: Option<Snapshot>,
    cpu: History,
    mem: History,
    cores: Vec<History>,
    proc_cache: KeyedRows<9>,
    detail_cache: KeyedRows<10>,
    user_cache: RowCache<4>,
    services: Services,
    service_cache: RowCache<4>,
    startup: Startup,
    startup_cache: RowCache<4>,
}

struct Models {
    procs: Rc<VecModel<Row>>,
    details: Rc<VecModel<Row>>,
    users: Rc<VecModel<Row>>,
    services: Rc<VecModel<Row>>,
    startup: Rc<VecModel<Row>>,
    cores: Rc<VecModel<GraphData>>,
}

/// Takes a sample and records the history; drawing is `render`'s job.
fn refresh(ui: &MainWindow, st: &mut State, m: &Models) {
    let Ok(snap) = st.sampler.sample() else {
        ui.set_status_text("Could not read /proc".into());
        return;
    };

    st.cpu.push(snap.cpu_percent);
    st.mem
        .push(snap.mem.used_kib() as f64 / snap.mem.total_kib.max(1) as f64 * 100.0);
    st.cores
        .resize_with(snap.per_core_percent.len(), || History::new(HISTORY_LEN));
    for (h, v) in st.cores.iter_mut().zip(&snap.per_core_percent) {
        h.push(*v);
    }
    st.last = Some(snap);
    render(ui, st, m);
}

/// Draws the visible tab and the status bar from the last sample. Hidden tabs cost nothing.
fn render(ui: &MainWindow, st: &mut State, m: &Models) {
    let Some(snap) = st.last.take() else { return };
    let mem_pct = snap.mem.used_kib() as f64 / snap.mem.total_kib.max(1) as f64 * 100.0;

    match st.tab {
        TAB_PROCESSES => render_processes(ui, st, m, &snap),
        TAB_DETAILS => render_details(ui, st, m, &snap),
        TAB_USERS => {
            let rows = view::aggregate_users(&snap.processes, &st.users)
                .iter()
                .map(view::user_row)
                .collect();
            st.user_cache.sync(&m.users, rows);
        }
        TAB_PERFORMANCE => render_performance(ui, st, m, &snap, mem_pct),
        TAB_SERVICES => render_services(ui, st, m, &snap),
        TAB_STARTUP => render_startup(ui, st, m),
        _ => {}
    }

    ui.set_status_text(
        format!(
            "Processes: {}   CPU Usage: {:.0}%   Physical Memory: {mem_pct:.0}%",
            snap.processes.len(),
            snap.cpu_percent
        )
        .into(),
    );
    st.last = Some(snap);
}

fn render_processes(ui: &MainWindow, st: &mut State, m: &Models, snap: &Snapshot) {
    let mut groups = view::group_processes(&snap.processes, st.grouped);
    view::sort_processes(&mut groups, &st.users, st.tables[0].sort);
    st.tables[0].order = groups.iter().map(|g| g.info.pid).collect();
    let rows = groups
        .iter()
        .map(|g| (g.info.pid, view::group_row(g, &st.users)))
        .collect();
    st.proc_cache.sync(&m.procs, rows);
    st.groups = groups;
    ui.set_selected_row(st.tables[0].selected_index());
}

fn render_details(ui: &MainWindow, st: &mut State, m: &Models, snap: &Snapshot) {
    let mut procs: Vec<&tm_core::ProcessInfo> = snap.processes.iter().collect();
    view::sort_processes(&mut procs, &st.users, st.tables[1].sort);
    st.tables[1].order = procs.iter().map(|p| p.pid).collect();
    let rows = procs
        .iter()
        .map(|p| (p.pid, view::detail_row(p, &st.users)))
        .collect();
    st.detail_cache.sync(&m.details, rows);
    ui.set_detail_selected_row(st.tables[1].selected_index());
}

fn render_startup(ui: &MainWindow, st: &mut State, m: &Models) {
    let su = &mut st.startup;
    su.list = tm_core::list_startup(&su.dirs);
    st.startup_cache.sync(
        &m.startup,
        su.list.iter().map(view::startup_cells).collect(),
    );
    let idx = su
        .selected
        .as_ref()
        .and_then(|id| su.list.iter().position(|e| &e.id == id));
    if idx.is_none() {
        su.selected = None;
    }
    ui.set_startup_selected_row(idx.map_or(-1, |i| i as i32));
    let enabled = idx.is_some_and(|i| su.list[i].enabled);
    ui.set_startup_action(if enabled { "Disable" } else { "Enable" }.into());
}

fn render_services(ui: &MainWindow, st: &mut State, m: &Models, snap: &Snapshot) {
    let sv = &mut st.services;
    if !sv.in_flight && sv.fetched_at.is_none_or(|t| t.elapsed() >= SERVICES_EVERY) {
        sv.in_flight = true;
        let (mailbox, weak) = (sv.mailbox.clone(), ui.as_weak());
        std::thread::spawn(move || {
            let reply = tm_core::list_services();
            *mailbox.lock().unwrap_or_else(|e| e.into_inner()) = Some(reply);
            let _ = weak.upgrade_in_event_loop(|ui| ui.invoke_services_ready());
        });
    }
    let mut rows = view::service_rows(&sv.list, &snap.processes);
    view::sort_services(&mut rows, sv.sort.0, sv.sort.1);
    st.service_cache
        .sync(&m.services, rows.iter().map(view::service_cells).collect());
    ui.set_services_message(sv.error.clone().unwrap_or_default().into());
}

fn render_performance(ui: &MainWindow, st: &State, m: &Models, snap: &Snapshot, mem_pct: f64) {
    ui.set_cpu_text(format!("{:.0}%", snap.cpu_percent).into());
    ui.set_cpu_commands(st.cpu.area_path().into());
    ui.set_mem_text(
        format!(
            "{} / {} MiB ({mem_pct:.0}%)",
            snap.mem.used_kib() / 1024,
            snap.mem.total_kib / 1024
        )
        .into(),
    );
    ui.set_mem_commands(st.mem.area_path().into());

    let graphs: Vec<GraphData> = st
        .cores
        .iter()
        .zip(&snap.per_core_percent)
        .enumerate()
        .map(|(i, (h, v))| GraphData {
            label: format!("CPU {i}  {v:.0}%").into(),
            commands: h.area_path().into(),
        })
        .collect();
    if graphs.len() == m.cores.row_count() {
        for (i, g) in graphs.into_iter().enumerate() {
            m.cores.set_row_data(i, g);
        }
    } else {
        m.cores.set_vec(graphs);
    }

    let threads: i64 = snap.processes.iter().map(|p| p.threads).sum();
    ui.set_stats_text(
        format!(
            "Threads: {}   Swap used: {} / {} MiB",
            threads,
            (snap.mem.swap_total_kib - snap.mem.swap_free_kib) / 1024,
            snap.mem.swap_total_kib / 1024
        )
        .into(),
    );
}

fn set_interval(
    timer: &Timer,
    ui: &MainWindow,
    st: &Rc<RefCell<State>>,
    m: &Rc<Models>,
    idx: usize,
) {
    timer.stop();
    let Some(every) = INTERVALS.get(idx).copied() else {
        return;
    };
    let (weak, st, m) = (ui.as_weak(), st.clone(), m.clone());
    timer.start(TimerMode::Repeated, every, move || {
        if let Some(ui) = weak.upgrade() {
            refresh(&ui, &mut st.borrow_mut(), &m);
        }
    });
}

fn main() {
    let ui = MainWindow::new().unwrap();
    let mut sampler = Sampler::new();
    // Baseline so the first displayed sample has CPU deltas.
    let _ = sampler.sample();
    let st = Rc::new(RefCell::new(State {
        sampler,
        users: UserNames::load(),
        tab: TAB_PROCESSES,
        grouped: true,
        tables: Default::default(),
        groups: Vec::new(),
        pending_kill: Vec::new(),
        last: None,
        cpu: History::new(HISTORY_LEN),
        mem: History::new(HISTORY_LEN),
        cores: Vec::new(),
        proc_cache: KeyedRows::new(),
        detail_cache: KeyedRows::new(),
        user_cache: RowCache::new(),
        services: Services::default(),
        service_cache: RowCache::new(),
        startup: Startup {
            dirs: StartupDirs::from_env(),
            list: Vec::new(),
            selected: None,
        },
        startup_cache: RowCache::new(),
    }));
    let m = Rc::new(Models {
        procs: Rc::new(VecModel::default()),
        details: Rc::new(VecModel::default()),
        users: Rc::new(VecModel::default()),
        services: Rc::new(VecModel::default()),
        startup: Rc::new(VecModel::default()),
        cores: Rc::new(VecModel::default()),
    });
    ui.set_process_rows(ModelRc::from(m.procs.clone()));
    ui.set_detail_rows(ModelRc::from(m.details.clone()));
    ui.set_startup_rows(ModelRc::from(m.startup.clone()));
    ui.set_service_rows(ModelRc::from(m.services.clone()));
    ui.set_user_rows(ModelRc::from(m.users.clone()));
    ui.set_cores(ModelRc::from(m.cores.clone()));

    let timer = Rc::new(Timer::default());
    std::thread::sleep(Duration::from_millis(200));
    refresh(&ui, &mut st.borrow_mut(), &m);
    set_interval(&timer, &ui, &st, &m, 0);

    let on_sort = |ascending: bool| {
        let (weak, st, m) = (ui.as_weak(), st.clone(), m.clone());
        move |table: i32, col: i32| {
            let Some(ui) = weak.upgrade() else { return };
            let column = if table == 0 {
                SortColumn::from_index(col)
            } else {
                SortColumn::from_details_index(col)
            };
            let (Some(column), Some(t)) = (column, usize::try_from(table).ok()) else {
                return;
            };
            let mut st = st.borrow_mut();
            if let Some(t) = st.tables.get_mut(t) {
                t.sort = Sort { column, ascending };
            }
            render(&ui, &mut st, &m);
        }
    };
    ui.on_sort_ascending(on_sort(true));
    ui.on_sort_descending(on_sort(false));

    {
        let st = st.clone();
        ui.on_row_selected(move |table, row| {
            if let Some(t) = st.borrow_mut().tables.get_mut(table as usize) {
                t.select(row);
            }
        });
    }
    {
        let (weak, st, m) = (ui.as_weak(), st.clone(), m.clone());
        ui.on_services_ready(move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut st = st.borrow_mut();
            let sv = &mut st.services;
            let reply = sv.mailbox.lock().unwrap_or_else(|e| e.into_inner()).take();
            sv.in_flight = false;
            sv.fetched_at = Some(Instant::now());
            match reply {
                Some(Ok(list)) => (sv.list, sv.error) = (list, None),
                Some(Err(e)) => sv.error = Some(e),
                None => {}
            }
            render(&ui, &mut st, &m);
        });
    }
    {
        let (weak, st) = (ui.as_weak(), st.clone());
        ui.on_startup_row_selected(move |row| {
            let Some(ui) = weak.upgrade() else { return };
            let mut st = st.borrow_mut();
            let su = &mut st.startup;
            su.selected = usize::try_from(row)
                .ok()
                .and_then(|i| su.list.get(i))
                .map(|e| e.id.clone());
            let enabled = su
                .selected
                .as_ref()
                .and_then(|id| su.list.iter().find(|e| &e.id == id))
                .is_some_and(|e| e.enabled);
            ui.set_startup_action(if enabled { "Disable" } else { "Enable" }.into());
        });
    }
    {
        let (weak, st, m) = (ui.as_weak(), st.clone(), m.clone());
        ui.on_startup_toggled(move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut st = st.borrow_mut();
            let su = &st.startup;
            let Some(entry) = su
                .selected
                .as_ref()
                .and_then(|id| su.list.iter().find(|e| &e.id == id))
            else {
                return;
            };
            if let Err(e) = tm_core::set_startup_enabled(&su.dirs, &entry.id, !entry.enabled) {
                ui.set_message(format!("Could not change \"{}\": {e}", entry.name).into());
                ui.set_confirming(false);
            }
            render(&ui, &mut st, &m);
        });
    }
    {
        let (weak, st, m) = (ui.as_weak(), st.clone(), m.clone());
        ui.on_service_sorted(move |col, ascending| {
            let Some(ui) = weak.upgrade() else { return };
            let mut st = st.borrow_mut();
            st.services.sort = (col, ascending);
            render(&ui, &mut st, &m);
        });
    }
    {
        let (weak, st, m) = (ui.as_weak(), st.clone(), m.clone());
        ui.on_tab_selected(move |tab| {
            let Some(ui) = weak.upgrade() else { return };
            let mut st = st.borrow_mut();
            st.tab = tab;
            render(&ui, &mut st, &m);
        });
    }
    {
        let (weak, st, m) = (ui.as_weak(), st.clone(), m.clone());
        ui.on_group_toggled(move |on| {
            let Some(ui) = weak.upgrade() else { return };
            let mut st = st.borrow_mut();
            st.grouped = on;
            // Row keys mean something different now, so don't carry the selection over.
            st.tables[0].selected = None;
            render(&ui, &mut st, &m);
        });
    }
    {
        let (weak, st) = (ui.as_weak(), st.clone());
        ui.on_end_process_clicked(move |table| {
            let Some(ui) = weak.upgrade() else { return };
            let mut st = st.borrow_mut();
            let Some(key) = st.tables.get(table as usize).and_then(|t| t.selected) else {
                return;
            };
            let (name, pids) = if table == 0 {
                let Some(g) = st.groups.iter().find(|g| g.info.pid == key) else {
                    return;
                };
                (g.info.name.clone(), g.pids.clone())
            } else {
                let Some(p) = st
                    .last
                    .as_ref()
                    .and_then(|s| s.processes.iter().find(|p| p.pid == key))
                else {
                    return;
                };
                (p.name.clone(), vec![p.pid])
            };
            let text = match pids[..] {
                [pid] => format!(
                    "Do you want to end \"{name}\" (PID {pid})? Unsaved data in it will be lost."
                ),
                _ => format!(
                    "Do you want to end \"{name}\" and all {} of its processes? Unsaved data in them will be lost.",
                    pids.len()
                ),
            };
            st.pending_kill = pids;
            ui.set_message(text.into());
            ui.set_confirming(true);
        });
    }
    {
        let (weak, st) = (ui.as_weak(), st.clone());
        ui.on_end_process_confirmed(move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_message("".into());
            ui.set_confirming(false);
            let pids = std::mem::take(&mut st.borrow_mut().pending_kill);
            if pids.is_empty() {
                return;
            }
            // pkexec blocks on the authentication dialog, so keep it off the UI thread.
            let weak = ui.as_weak();
            std::thread::spawn(move || {
                if let Err(msg) = kill::end_processes(&pids) {
                    let _ = weak.upgrade_in_event_loop(move |ui| {
                        ui.set_message(msg.into());
                        ui.set_confirming(false);
                    });
                }
            });
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_message_dismissed(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_message("".into());
                ui.set_confirming(false);
            }
        });
    }
    {
        let (weak, st, m, timer) = (ui.as_weak(), st.clone(), m.clone(), timer.clone());
        ui.on_interval_selected(move |i| {
            if let Some(ui) = weak.upgrade() {
                set_interval(&timer, &ui, &st, &m, i as usize);
            }
        });
    }

    ui.run().unwrap();
}

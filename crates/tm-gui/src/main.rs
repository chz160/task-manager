mod kill;
mod rows;
mod view;

use rows::{KeyedRows, Row, RowCache};
use slint::{Model, ModelRc, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;
use tm_core::{ProcessInfo, Sampler, Snapshot};
use view::{History, Sort, SortColumn, UserNames};

slint::include_modules!();

const HISTORY_LEN: usize = 60;
/// Order matches the "Refresh" combo box in `main.slint`.
const INTERVALS: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
];

struct State {
    sampler: Sampler,
    users: UserNames,
    sort: Sort,
    selected_pid: Option<i32>,
    /// PIDs in the order the table currently shows them.
    order: Vec<i32>,
    last: Option<Snapshot>,
    cpu: History,
    mem: History,
    cores: Vec<History>,
    proc_cache: KeyedRows<6>,
    user_cache: RowCache<4>,
}

struct Models {
    procs: Rc<VecModel<Row>>,
    users: Rc<VecModel<Row>>,
    cores: Rc<VecModel<GraphData>>,
}

fn refresh(ui: &MainWindow, st: &mut State, m: &Models) {
    let Ok(mut snap) = st.sampler.sample() else {
        ui.set_status_text("Could not read /proc".into());
        return;
    };

    st.cpu.push(snap.cpu_percent);
    let mem_pct = snap.mem.used_kib() as f64 / snap.mem.total_kib.max(1) as f64 * 100.0;
    st.mem.push(mem_pct);
    st.cores
        .resize_with(snap.per_core_percent.len(), || History::new(HISTORY_LEN));
    for (h, v) in st.cores.iter_mut().zip(&snap.per_core_percent) {
        h.push(*v);
    }

    view::sort_processes(&mut snap.processes, &st.users, st.sort);
    st.order = snap.processes.iter().map(|p| p.pid).collect();
    let rows = snap
        .processes
        .iter()
        .map(|p| (p.pid, view::process_row(p, &st.users)))
        .collect();
    st.proc_cache.sync(&m.procs, rows);
    // Keep the selection on the same process even as rows move; drop it if it exited.
    let idx = st
        .selected_pid
        .and_then(|pid| st.order.iter().position(|p| *p == pid));
    if idx.is_none() {
        st.selected_pid = None;
    }
    ui.set_selected_row(idx.map_or(-1, |i| i as i32));

    let rows = view::aggregate_users(&snap.processes, &st.users)
        .iter()
        .map(view::user_row)
        .collect();
    st.user_cache.sync(&m.users, rows);

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

fn selected_process(st: &State) -> Option<&ProcessInfo> {
    let pid = st.selected_pid?;
    st.last.as_ref()?.processes.iter().find(|p| p.pid == pid)
}

fn main() {
    let ui = MainWindow::new().unwrap();
    let mut sampler = Sampler::new();
    // Baseline so the first displayed sample has CPU deltas.
    let _ = sampler.sample();
    let st = Rc::new(RefCell::new(State {
        sampler,
        users: UserNames::load(),
        sort: Sort::default(),
        selected_pid: None,
        order: Vec::new(),
        last: None,
        cpu: History::new(HISTORY_LEN),
        mem: History::new(HISTORY_LEN),
        cores: Vec::new(),
        proc_cache: KeyedRows::new(),
        user_cache: RowCache::new(),
    }));
    let m = Rc::new(Models {
        procs: Rc::new(VecModel::default()),
        users: Rc::new(VecModel::default()),
        cores: Rc::new(VecModel::default()),
    });
    ui.set_process_rows(ModelRc::from(m.procs.clone()));
    ui.set_user_rows(ModelRc::from(m.users.clone()));
    ui.set_cores(ModelRc::from(m.cores.clone()));

    let timer = Rc::new(Timer::default());
    std::thread::sleep(Duration::from_millis(200));
    refresh(&ui, &mut st.borrow_mut(), &m);
    set_interval(&timer, &ui, &st, &m, 0);

    let on_sort = |ascending: bool| {
        let (weak, st, m) = (ui.as_weak(), st.clone(), m.clone());
        move |col: i32| {
            let (Some(ui), Some(column)) = (weak.upgrade(), SortColumn::from_index(col)) else {
                return;
            };
            st.borrow_mut().sort = Sort { column, ascending };
            refresh(&ui, &mut st.borrow_mut(), &m);
        }
    };
    ui.on_sort_ascending(on_sort(true));
    ui.on_sort_descending(on_sort(false));

    {
        let st = st.clone();
        ui.on_row_selected(move |i| {
            let mut st = st.borrow_mut();
            st.selected_pid = usize::try_from(i)
                .ok()
                .and_then(|i| st.order.get(i).copied());
        });
    }
    {
        let (weak, st) = (ui.as_weak(), st.clone());
        ui.on_end_process_clicked(move || {
            let (Some(ui), st) = (weak.upgrade(), st.borrow()) else {
                return;
            };
            if let Some(p) = selected_process(&st) {
                ui.set_message(
                    format!(
                        "Do you want to end \"{}\" (PID {})? Unsaved data in it will be lost.",
                        p.name, p.pid
                    )
                    .into(),
                );
                ui.set_confirming(true);
            }
        });
    }
    {
        let (weak, st) = (ui.as_weak(), st.clone());
        ui.on_end_process_confirmed(move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_message("".into());
            ui.set_confirming(false);
            let Some(pid) = st.borrow().selected_pid else {
                return;
            };
            // pkexec blocks on the authentication dialog, so keep it off the UI thread.
            let weak = ui.as_weak();
            std::thread::spawn(move || {
                if let Err(msg) = kill::end_process(pid) {
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

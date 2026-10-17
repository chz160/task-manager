//! Spike: 1,000 live rows in a Slint StandardTableView (software renderer),
//! updated in place every second. Prints RSS and CPU time like the GTK spike.
use slint::{Model, ModelRc, SharedString, StandardListViewItem, Timer, TimerMode, VecModel};
use std::rc::Rc;
use std::time::Duration;

slint::include_modules!();

const ROWS: usize = 1000;

fn item(s: String) -> StandardListViewItem {
    let mut i = StandardListViewItem::default();
    i.text = SharedString::from(s);
    i
}

fn row(i: usize, tick: usize) -> ModelRc<StandardListViewItem> {
    ModelRc::new(VecModel::from(vec![
        item((i + 1).to_string()),
        item(format!("process-{i}")),
        item(format!("{:.1}", ((i + tick) % 97) as f64 / 10.0)),
        item(format!("{:.1}", (i % 200) as f64 + tick as f64 * 0.1)),
    ]))
}

fn proc_stat() -> (u64, f64) {
    let rss = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()))
        .unwrap_or(0)
        * 4
        / 1024;
    let st = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let f: Vec<&str> = st.rsplit(')').next().unwrap_or("").split_whitespace().collect();
    let t: f64 = f.get(11).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0)
        + f.get(12).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    (rss, t / 100.0)
}

fn main() {
    let ui = Table::new().unwrap();
    let model = Rc::new(VecModel::from((0..ROWS).map(|i| row(i, 0)).collect::<Vec<_>>()));
    ui.set_rows(ModelRc::from(model.clone()));
    ui.show().unwrap();

    let timer = Timer::default();
    let mut tick = 0;
    timer.start(TimerMode::Repeated, Duration::from_secs(1), move || {
        tick += 1;
        for i in 0..model.row_count() {
            model.set_row_data(i, row(i, tick));
        }
        let (rss, cpu) = proc_stat();
        println!("t={tick:>3}s rss={rss} MiB cpu_time={cpu:.2}s");
    });
    slint::run_event_loop().unwrap();
}

//! Spike: 1,000 live rows in a GTK4 ColumnView, updated in place every second.
//! Prints RSS and process CPU time once a second so idle cost can be read off.
use gtk4::prelude::*;
use gtk4::{gio, glib, Application, ApplicationWindow, ColumnView, ColumnViewColumn, Label,
    ScrolledWindow, SignalListItemFactory, SingleSelection, SortListModel};
use std::cell::RefCell;
use std::time::Duration;

struct Row {
    pid: u32,
    name: String,
    cpu: f64,
    mem_mib: f64,
}

const ROWS: u32 = 1000;

fn proc_stat() -> (u64, f64) {
    let rss = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()))
        .unwrap_or(0)
        * 4
        / 1024;
    let st = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let f: Vec<&str> = st.rsplit(')').next().unwrap_or("").split_whitespace().collect();
    let ticks: f64 = f.get(11).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0)
        + f.get(12).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    (rss, ticks / 100.0)
}

fn text_column(
    view: &ColumnView,
    title: &str,
    fmt: fn(&Row) -> String,
    sort: fn(&Row, &Row) -> std::cmp::Ordering,
) {
    let factory = SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let l = Label::new(None);
        l.set_xalign(0.0);
        item.set_child(Some(&l));
    });
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk4::ListItem>().unwrap();
        let obj = item.item().and_downcast::<glib::BoxedAnyObject>().unwrap();
        let l = item.child().and_downcast::<Label>().unwrap();
        l.set_text(&fmt(&obj.borrow::<RefCell<Row>>().borrow()));
    });
    let col = ColumnViewColumn::new(Some(title), Some(factory));
    col.set_sorter(Some(&gtk4::CustomSorter::new(move |a, b| {
        let a = a.downcast_ref::<glib::BoxedAnyObject>().unwrap();
        let b = b.downcast_ref::<glib::BoxedAnyObject>().unwrap();
        let (a, b) = (a.borrow::<RefCell<Row>>(), b.borrow::<RefCell<Row>>());
        let ord = sort(&a.borrow(), &b.borrow());
        ord.into()
    })));
    view.append_column(&col);
}

fn main() -> glib::ExitCode {
    let app = Application::builder().application_id("dev.example.spike").build();
    app.connect_activate(|app| {
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        for i in 0..ROWS {
            store.append(&glib::BoxedAnyObject::new(RefCell::new(Row {
                pid: i + 1,
                name: format!("process-{i}"),
                cpu: 0.0,
                mem_mib: (i % 200) as f64,
            })));
        }
        let sorted = SortListModel::new(Some(store.clone()), None::<gtk4::Sorter>);
        let sel = SingleSelection::new(Some(sorted.clone()));
        let view = ColumnView::new(Some(sel));
        sorted.set_sorter(view.sorter().as_ref());
        text_column(&view, "PID", |r| r.pid.to_string(), |a, b| a.pid.cmp(&b.pid));
        text_column(&view, "Name", |r| r.name.clone(), |a, b| a.name.cmp(&b.name));
        text_column(&view, "CPU %", |r| format!("{:.1}", r.cpu), |a, b| a.cpu.total_cmp(&b.cpu));
        text_column(&view, "Mem MiB", |r| format!("{:.1}", r.mem_mib), |a, b| a.mem_mib.total_cmp(&b.mem_mib));

        let win = ApplicationWindow::builder()
            .application(app)
            .default_width(700)
            .default_height(500)
            .child(&ScrolledWindow::builder().child(&view).build())
            .build();
        win.present();

        let mut tick = 0u32;
        glib::timeout_add_local(Duration::from_secs(1), move || {
            tick += 1;
            // Mutate every row in place, then notify: the selection and sort
            // state must survive.
            for i in 0..ROWS {
                let obj = store.item(i).and_downcast::<glib::BoxedAnyObject>().unwrap();
                {
                    let cell = obj.borrow::<RefCell<Row>>();
                    let mut r = cell.borrow_mut();
                    r.cpu = ((i + tick) % 97) as f64 / 10.0;
                    r.mem_mib += 0.1;
                }
                store.items_changed(i, 1, 1);
            }
            let (rss, cpu_s) = proc_stat();
            println!("t={tick:>3}s rss={rss} MiB cpu_time={cpu_s:.2}s");
            glib::ControlFlow::Continue
        });
    });
    app.run()
}

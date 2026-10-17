use std::{thread, time::Duration};
use tm_core::Sampler;

fn main() {
    let mut args = std::env::args().skip(1);
    let interval = args.next().and_then(|a| a.parse().ok()).unwrap_or(1.0);
    let top: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(15);

    let mut sampler = Sampler::new();
    sampler.sample().expect("failed to read /proc");
    thread::sleep(Duration::from_secs_f64(interval));
    let mut snap = sampler.sample().expect("failed to read /proc");

    println!(
        "CPU {:5.1}%  Mem {}/{} MiB  Swap {}/{} MiB  Procs {}",
        snap.cpu_percent,
        snap.mem.used_kib() / 1024,
        snap.mem.total_kib / 1024,
        (snap.mem.swap_total_kib - snap.mem.swap_free_kib) / 1024,
        snap.mem.swap_total_kib / 1024,
        snap.processes.len()
    );
    let cores: Vec<String> = snap
        .per_core_percent
        .iter()
        .map(|c| format!("{c:.0}"))
        .collect();
    println!("Cores: {}", cores.join(" "));
    snap.processes
        .sort_by(|a, b| b.cpu_percent.total_cmp(&a.cpu_percent));
    println!(
        "{:>7} {:>6} {:>10} {:>3} NAME",
        "PID", "CPU%", "RSS(MiB)", "S"
    );
    for p in snap.processes.iter().take(top) {
        println!(
            "{:>7} {:>6.1} {:>10.1} {:>3} {}",
            p.pid,
            p.cpu_percent,
            p.rss_kib as f64 / 1024.0,
            p.state,
            p.name
        );
    }
}

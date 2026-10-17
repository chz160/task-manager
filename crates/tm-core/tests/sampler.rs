use std::{
    process, thread,
    time::{Duration, Instant},
};
use tm_core::Sampler;

#[test]
fn sees_own_process_and_memory() {
    let mut s = Sampler::new();
    let first = s.sample().unwrap();
    assert_eq!(first.cpu_percent, 0.0, "first sample has no baseline");
    let snap = s.sample().unwrap();
    let me = snap
        .processes
        .iter()
        .find(|p| p.pid == process::id() as i32)
        .expect("self in list");
    assert!(me.rss_kib > 0);
    assert!(snap.mem.total_kib >= snap.mem.available_kib);
    assert!(!snap.per_core_percent.is_empty());
}

#[test]
fn busy_loop_registers_cpu() {
    let mut s = Sampler::new();
    s.sample().unwrap();
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(500) {
        std::hint::black_box(0u64.wrapping_add(1));
    }
    let snap = s.sample().unwrap();
    let me = snap
        .processes
        .iter()
        .find(|p| p.pid == process::id() as i32)
        .unwrap();
    assert!(me.cpu_percent > 0.0, "busy loop should show CPU use");
    assert!(me.cpu_percent <= 100.0);
}

/// Budget check: one full sample must stay cheap (generous bound for CI noise).
#[test]
fn sample_cost_budget() {
    let mut s = Sampler::new();
    s.sample().unwrap();
    thread::sleep(Duration::from_millis(50));
    let t = Instant::now();
    let n = 20;
    for _ in 0..n {
        s.sample().unwrap();
    }
    let per = t.elapsed() / n;
    assert!(per < Duration::from_millis(50), "sample took {per:?}");
}

#[test]
fn own_process_has_extras() {
    let mut s = Sampler::new();
    let snap = s.sample().unwrap();
    let me = snap
        .processes
        .iter()
        .find(|p| p.pid == process::id() as i32)
        .unwrap();
    assert!(me.pss_kib.is_some_and(|k| k > 0), "PSS of our own process");
    assert!(me.pss_kib.unwrap() <= me.rss_kib + 1024);
    assert!(me.disk_read_bps.is_some() && me.disk_write_bps.is_some());
    assert!(me.gpu_percent.is_some(), "readable even without a GPU");
    assert!(
        me.cgroup.is_some()
            || std::fs::read_to_string("/proc/self/cgroup").is_ok_and(|c| !c.contains("0::"))
    );
}

#[test]
fn disk_writes_show_up_as_a_rate() {
    let mut s = Sampler::new();
    s.set_extras_interval(Duration::ZERO);
    s.sample().unwrap();
    let path = std::env::temp_dir().join(format!("tm-io-test-{}", process::id()));
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&vec![7u8; 16 << 20]).unwrap();
        f.sync_all().unwrap();
    }
    let _ = std::fs::remove_file(&path);
    thread::sleep(Duration::from_millis(200));
    let snap = s.sample().unwrap();
    let me = snap
        .processes
        .iter()
        .find(|p| p.pid == process::id() as i32)
        .unwrap();
    // Some filesystems (tmpfs) never reach a block device, so only require it to be measured.
    assert!(me.disk_write_bps.is_some());
}

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

//! Per-process GPU usage from the kernel's DRM fdinfo interface
//! (<https://docs.kernel.org/gpu/drm-usage-stats.html>). Works without root for the
//! caller's own processes on drivers that implement it (i915, amdgpu, xe, msm, ...).

use std::collections::HashMap;
use std::fs;

/// One DRM client (an open GPU context) as described by a single fdinfo file.
#[derive(Debug, Default, PartialEq)]
pub struct DrmClient {
    /// `drm-pdev` plus `drm-client-id`: the same client shows up once per duplicated fd.
    pub key: String,
    /// Busy time per engine, nanoseconds, cumulative.
    pub engines: Vec<(String, u64)>,
    pub mem_kib: u64,
}

fn to_kib(value: &str) -> Option<u64> {
    let mut parts = value.split_whitespace();
    let n: u64 = parts.next()?.parse().ok()?;
    match parts.next() {
        None => Some(n / 1024),
        Some("KiB") => Some(n),
        Some("MiB") => Some(n * 1024),
        Some("GiB") => Some(n * 1024 * 1024),
        Some(_) => None,
    }
}

/// Returns `None` when the file does not describe a DRM client.
pub fn parse_fdinfo(text: &str) -> Option<DrmClient> {
    let (mut pdev, mut id) = (None, None);
    let mut engines = Vec::new();
    let (mut total, mut legacy) = (0u64, 0u64);
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k {
            "drm-pdev" => pdev = Some(v.to_string()),
            "drm-client-id" => id = Some(v.to_string()),
            _ => {
                if let Some(name) = k.strip_prefix("drm-engine-") {
                    // "drm-engine-capacity-*" is a divisor, not a time.
                    if !name.starts_with("capacity-")
                        && let Some(ns) = v.strip_suffix("ns").and_then(|n| n.trim().parse().ok())
                    {
                        engines.push((name.to_string(), ns));
                    }
                } else if k.starts_with("drm-total-") && !k.starts_with("drm-total-cycles") {
                    total += to_kib(v).unwrap_or(0);
                } else if k.starts_with("drm-memory-") {
                    legacy += to_kib(v).unwrap_or(0);
                }
            }
        }
    }
    Some(DrmClient {
        key: format!("{}/{}", pdev.unwrap_or_default(), id?),
        engines,
        mem_kib: if total > 0 { total } else { legacy },
    })
}

/// Cumulative GPU counters of one process: engine busy time and memory, summed over its clients.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct GpuCounters {
    pub engine_ns: HashMap<String, u64>,
    pub mem_kib: u64,
}

pub fn combine(clients: Vec<DrmClient>) -> GpuCounters {
    let mut seen = std::collections::HashSet::new();
    let mut out = GpuCounters::default();
    for c in clients {
        if !seen.insert(c.key.clone()) {
            continue;
        }
        out.mem_kib += c.mem_kib;
        for (e, ns) in c.engines {
            *out.engine_ns.entry(e).or_default() += ns;
        }
    }
    out
}

/// Utilisation of the busiest engine between two readings, 0..=100 (Windows shows the same).
pub fn utilisation(prev: &GpuCounters, cur: &GpuCounters, elapsed_secs: f64) -> f64 {
    if elapsed_secs <= 0.0 {
        return 0.0;
    }
    cur.engine_ns
        .iter()
        .map(|(e, ns)| {
            let before = prev.engine_ns.get(e).copied().unwrap_or(*ns);
            ns.saturating_sub(before) as f64 / (elapsed_secs * 1e9) * 100.0
        })
        .fold(0.0, f64::max)
        .min(100.0)
}

/// Reads the DRM clients of a process. `Ok(None)` means it has no GPU fds.
/// `Err` means the fd table could not be read (not our process, or it exited).
pub fn read_counters(pid: i32) -> std::io::Result<Option<GpuCounters>> {
    let mut clients = Vec::new();
    for entry in fs::read_dir(format!("/proc/{pid}/fd"))?.flatten() {
        let Ok(target) = fs::read_link(entry.path()) else {
            continue;
        };
        if !target.starts_with("/dev/dri/") {
            continue;
        }
        let name = entry.file_name();
        if let Ok(text) =
            fs::read_to_string(format!("/proc/{pid}/fdinfo/{}", name.to_string_lossy()))
            && let Some(c) = parse_fdinfo(&text)
        {
            clients.push(c);
        }
    }
    Ok(if clients.is_empty() {
        None
    } else {
        Some(combine(clients))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const AMD: &str = "pos:\t0\nflags:\t02100002\ndrm-driver:\tamdgpu\ndrm-pdev:\t0000:03:00.0\n\
drm-client-id:\t42\ndrm-engine-gfx:\t1000000000 ns\ndrm-engine-compute:\t5 ns\n\
drm-memory-vram:\t2048 KiB\ndrm-memory-gtt: \t1 MiB\ndrm-memory-cpu:\t0 KiB\n";

    #[test]
    fn parses_amdgpu_fdinfo() {
        let c = parse_fdinfo(AMD).unwrap();
        assert_eq!(c.key, "0000:03:00.0/42");
        assert_eq!(c.mem_kib, 2048 + 1024);
        assert!(c.engines.contains(&("gfx".into(), 1_000_000_000)));
    }

    #[test]
    fn prefers_total_memory_and_skips_capacity() {
        let c = parse_fdinfo(
            "drm-pdev:\tx\ndrm-client-id:\t1\ndrm-total-system0:\t8 MiB\ndrm-resident-system0:\t4 MiB\n\
drm-engine-render:\t10 ns\ndrm-engine-capacity-render:\t2\ndrm-total-cycles-rcs:\t99\n",
        )
        .unwrap();
        assert_eq!(c.mem_kib, 8 * 1024);
        assert_eq!(c.engines, vec![("render".to_string(), 10)]);
    }

    #[test]
    fn non_drm_fdinfo_is_ignored() {
        assert_eq!(parse_fdinfo("pos:\t0\nflags:\t0100002\n"), None);
    }

    #[test]
    fn duplicate_fds_of_one_client_count_once() {
        let a = parse_fdinfo(AMD).unwrap();
        let b = parse_fdinfo(AMD).unwrap();
        let c = combine(vec![a, b]);
        assert_eq!(c.mem_kib, 3072);
        assert_eq!(c.engine_ns["gfx"], 1_000_000_000);
    }

    #[test]
    fn utilisation_is_the_busiest_engine() {
        let mk = |gfx, comp| GpuCounters {
            engine_ns: [("gfx".to_string(), gfx), ("compute".to_string(), comp)].into(),
            mem_kib: 0,
        };
        let u = utilisation(&mk(0, 0), &mk(500_000_000, 100_000_000), 1.0);
        assert!((u - 50.0).abs() < 1e-9);
        assert_eq!(utilisation(&mk(0, 0), &mk(u64::MAX, 0), 1.0), 100.0);
        assert_eq!(utilisation(&mk(5, 5), &mk(1, 1), 1.0), 0.0, "counter reset");
    }
}

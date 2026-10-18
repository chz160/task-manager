//! NVIDIA GPU readings through NVML, loaded with `dlopen` at runtime so the binary has no
//! link-time dependency and simply reports "no NVIDIA" where the driver library is missing.
//! (The proprietary driver does not publish DRM fdinfo, so `gpu.rs` sees nothing there.)

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_ulonglong, c_void};
use std::mem::size_of;
use std::ptr;

type Handle = *mut c_void;
type Ret = c_int;

const SUCCESS: Ret = 0;
const INSUFFICIENT_SIZE: Ret = 7;
/// `usedGpuMemory` of a process whose memory use NVML cannot tell.
const VALUE_NOT_AVAILABLE: u64 = u64::MAX;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Utilization {
    gpu: c_uint,
    memory: c_uint,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct MemoryInfo {
    total: c_ulonglong,
    free: c_ulonglong,
    used: c_ulonglong,
}

/// `nvmlProcessInfo_t` of the `_v2`/`_v3` entry points.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct ProcessInfo {
    pid: c_uint,
    used_gpu_memory: c_ulonglong,
    gpu_instance_id: c_uint,
    compute_instance_id: c_uint,
}

/// `nvmlProcessUtilizationSample_t`.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct UtilSample {
    pid: c_uint,
    time_stamp: c_ulonglong,
    sm_util: c_uint,
    mem_util: c_uint,
    enc_util: c_uint,
    dec_util: c_uint,
}

/// The NVML entry points the sampler uses.
struct Api {
    device_count: unsafe extern "C" fn(*mut c_uint) -> Ret,
    handle_by_index: unsafe extern "C" fn(c_uint, *mut Handle) -> Ret,
    name: unsafe extern "C" fn(Handle, *mut c_char, c_uint) -> Ret,
    utilization: unsafe extern "C" fn(Handle, *mut Utilization) -> Ret,
    memory: unsafe extern "C" fn(Handle, *mut MemoryInfo) -> Ret,
    temperature: unsafe extern "C" fn(Handle, c_uint, *mut c_uint) -> Ret,
    /// Compute and graphics process lists; absent on very old drivers.
    compute_procs: Option<unsafe extern "C" fn(Handle, *mut c_uint, *mut ProcessInfo) -> Ret>,
    graphics_procs: Option<unsafe extern "C" fn(Handle, *mut c_uint, *mut ProcessInfo) -> Ret>,
    process_util:
        Option<unsafe extern "C" fn(Handle, *mut UtilSample, *mut c_uint, c_ulonglong) -> Ret>,
}

/// One NVIDIA GPU at one moment.
#[derive(Debug, Clone, PartialEq)]
pub struct NvidiaGpu {
    pub name: String,
    pub util_percent: Option<u32>,
    pub mem_used_kib: Option<u64>,
    pub mem_total_kib: Option<u64>,
    pub temp_c: Option<u32>,
}

/// What one process is doing on the NVIDIA GPUs, summed over all of them.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ProcUse {
    pub util_percent: Option<u32>,
    pub mem_kib: Option<u64>,
}

pub struct Nvml {
    api: Api,
    devices: Vec<Handle>,
    /// Newest per-process utilisation sample seen per device, so NVML only returns newer ones.
    last_seen: Vec<u64>,
}

/// Reinterprets a `dlsym` result as the function pointer type `F`.
///
/// # Safety
/// `F` must be a function pointer type matching the symbol's real signature.
unsafe fn cast<F: Copy>(p: *mut c_void) -> F {
    debug_assert_eq!(size_of::<F>(), size_of::<*mut c_void>());
    unsafe { std::mem::transmute_copy::<*mut c_void, F>(&p) }
}

fn ok(r: Ret) -> bool {
    r == SUCCESS
}

impl Nvml {
    /// `None` when the NVIDIA driver library is not installed or reports no device.
    pub fn load() -> Option<Self> {
        // SAFETY: dlopen/dlsym on a library name and symbol names we own; the resolved
        // pointers are cast to the signatures documented in nvml.h.
        unsafe {
            let lib = libc::dlopen(
                c"libnvidia-ml.so.1".as_ptr(),
                libc::RTLD_NOW | libc::RTLD_LOCAL,
            );
            if lib.is_null() {
                return None;
            }
            let sym = |name: &str| -> *mut c_void {
                let n = CString::new(name).unwrap_or_default();
                libc::dlsym(lib, n.as_ptr())
            };
            macro_rules! need {
                ($n:expr) => {{
                    let p = sym($n);
                    if p.is_null() {
                        return None;
                    }
                    cast(p)
                }};
            }
            macro_rules! maybe {
                ($($n:expr),+) => {{
                    let mut found = None;
                    for n in [$($n),+] {
                        let p = sym(n);
                        if !p.is_null() {
                            found = Some(cast(p));
                            break;
                        }
                    }
                    found
                }};
            }
            let init: unsafe extern "C" fn() -> Ret = need!("nvmlInit_v2");
            let api = Api {
                device_count: need!("nvmlDeviceGetCount_v2"),
                handle_by_index: need!("nvmlDeviceGetHandleByIndex_v2"),
                name: need!("nvmlDeviceGetName"),
                utilization: need!("nvmlDeviceGetUtilizationRates"),
                memory: need!("nvmlDeviceGetMemoryInfo"),
                temperature: need!("nvmlDeviceGetTemperature"),
                compute_procs: maybe!(
                    "nvmlDeviceGetComputeRunningProcesses_v3",
                    "nvmlDeviceGetComputeRunningProcesses_v2"
                ),
                graphics_procs: maybe!(
                    "nvmlDeviceGetGraphicsRunningProcesses_v3",
                    "nvmlDeviceGetGraphicsRunningProcesses_v2"
                ),
                process_util: maybe!("nvmlDeviceGetProcessUtilization"),
            };
            if !ok(init()) {
                return None;
            }
            Self::with_api(api)
        }
    }

    fn with_api(api: Api) -> Option<Self> {
        let mut n: c_uint = 0;
        // SAFETY: out-pointers to locals.
        unsafe {
            if !ok((api.device_count)(&mut n)) || n == 0 {
                return None;
            }
            let mut devices = Vec::new();
            for i in 0..n {
                let mut h: Handle = ptr::null_mut();
                if ok((api.handle_by_index)(i, &mut h)) && !h.is_null() {
                    devices.push(h);
                }
            }
            if devices.is_empty() {
                return None;
            }
            Some(Self {
                api,
                last_seen: vec![0; devices.len()],
                devices,
            })
        }
    }

    /// Device-level readings: cheap enough to take on every refresh.
    pub fn gpus(&self) -> Vec<NvidiaGpu> {
        self.devices.iter().map(|&h| self.gpu(h)).collect()
    }

    fn gpu(&self, h: Handle) -> NvidiaGpu {
        // SAFETY: each call writes only to the local it is given.
        unsafe {
            let mut name = [0 as c_char; 96];
            let name = if ok((self.api.name)(h, name.as_mut_ptr(), name.len() as c_uint)) {
                CStr::from_ptr(name.as_ptr()).to_string_lossy().into_owned()
            } else {
                "NVIDIA GPU".to_string()
            };
            let mut u = Utilization::default();
            let mut m = MemoryInfo::default();
            let mut t: c_uint = 0;
            let mem = ok((self.api.memory)(h, &mut m));
            NvidiaGpu {
                name,
                util_percent: ok((self.api.utilization)(h, &mut u)).then_some(u.gpu),
                mem_used_kib: mem.then_some(m.used / 1024),
                mem_total_kib: mem.then_some(m.total / 1024),
                temp_c: ok((self.api.temperature)(h, 0, &mut t)).then_some(t),
            }
        }
    }

    /// Per-process memory and utilisation, summed over devices. Utilisation comes from the
    /// samples NVML collected since the previous call, so the first call has little to report.
    pub fn processes(&mut self) -> HashMap<i32, ProcUse> {
        let mut out: HashMap<i32, ProcUse> = HashMap::new();
        for (i, &h) in self.devices.clone().iter().enumerate() {
            // A process with both a compute and a graphics context is listed by both calls.
            let mut mem: HashMap<u32, u64> = HashMap::new();
            for f in [self.api.compute_procs, self.api.graphics_procs]
                .into_iter()
                .flatten()
            {
                for p in list_processes(f, h) {
                    if p.used_gpu_memory != VALUE_NOT_AVAILABLE {
                        let e = mem.entry(p.pid).or_default();
                        *e = (*e).max(p.used_gpu_memory);
                    }
                }
            }
            for (pid, bytes) in mem {
                let e = out.entry(pid as i32).or_default();
                e.mem_kib = Some(e.mem_kib.unwrap_or(0) + bytes / 1024);
            }
            if let Some(f) = self.api.process_util {
                // NVML returns every sample since the last call; the newest one is the
                // current load of that process.
                let mut latest: HashMap<u32, (u64, u32)> = HashMap::new();
                for s in utilisation_samples(f, h, self.last_seen[i]) {
                    self.last_seen[i] = self.last_seen[i].max(s.time_stamp);
                    let busy = s.sm_util.max(s.enc_util).max(s.dec_util);
                    let e = latest.entry(s.pid).or_insert((0, 0));
                    if s.time_stamp >= e.0 {
                        *e = (s.time_stamp, busy);
                    }
                }
                for (pid, (_, busy)) in latest {
                    let e = out.entry(pid as i32).or_default();
                    e.util_percent = Some((e.util_percent.unwrap_or(0) + busy).min(100));
                }
            }
        }
        out
    }
}

fn list_processes(
    f: unsafe extern "C" fn(Handle, *mut c_uint, *mut ProcessInfo) -> Ret,
    h: Handle,
) -> Vec<ProcessInfo> {
    let mut count: c_uint = 0;
    // SAFETY: the first call only asks for the size; the second gets a buffer of that size
    // (plus slack for processes that appear in between).
    unsafe {
        let r = f(h, &mut count, ptr::null_mut());
        if r == SUCCESS && count == 0 {
            return Vec::new();
        }
        if r != INSUFFICIENT_SIZE && r != SUCCESS {
            return Vec::new();
        }
        let mut buf = vec![ProcessInfo::default(); count as usize + 8];
        count = buf.len() as c_uint;
        if !ok(f(h, &mut count, buf.as_mut_ptr())) {
            return Vec::new();
        }
        buf.truncate(count as usize);
        buf
    }
}

fn utilisation_samples(
    f: unsafe extern "C" fn(Handle, *mut UtilSample, *mut c_uint, c_ulonglong) -> Ret,
    h: Handle,
    since: u64,
) -> Vec<UtilSample> {
    let mut count: c_uint = 0;
    // SAFETY: as in `list_processes`.
    unsafe {
        if f(h, ptr::null_mut(), &mut count, since) != INSUFFICIENT_SIZE || count == 0 {
            return Vec::new();
        }
        let mut buf = vec![UtilSample::default(); count as usize + 8];
        count = buf.len() as c_uint;
        if !ok(f(h, buf.as_mut_ptr(), &mut count, since)) {
            return Vec::new();
        }
        buf.truncate(count as usize);
        buf
    }
}

/// True when an NVIDIA kernel driver is loaded, so a missing NVML is worth telling the user about.
pub fn driver_present() -> bool {
    std::path::Path::new("/proc/driver/nvidia").exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn count(n: *mut c_uint) -> Ret {
        unsafe { *n = 2 };
        SUCCESS
    }
    unsafe extern "C" fn by_index(i: c_uint, h: *mut Handle) -> Ret {
        // Handles are opaque; encode the index (+1 so it is never null).
        unsafe { *h = (i as usize + 1) as Handle };
        SUCCESS
    }
    unsafe extern "C" fn name(h: Handle, buf: *mut c_char, len: c_uint) -> Ret {
        let s = format!("Fake GPU {}\0", h as usize);
        assert!(s.len() <= len as usize);
        unsafe { ptr::copy_nonoverlapping(s.as_ptr().cast(), buf, s.len()) };
        SUCCESS
    }
    unsafe extern "C" fn util(_: Handle, u: *mut Utilization) -> Ret {
        unsafe { *u = Utilization { gpu: 37, memory: 5 } };
        SUCCESS
    }
    unsafe extern "C" fn mem(h: Handle, m: *mut MemoryInfo) -> Ret {
        if h as usize == 2 {
            return 999; // second device cannot report memory
        }
        unsafe {
            *m = MemoryInfo {
                total: 8 << 30,
                free: 6 << 30,
                used: 2 << 30,
            };
        }
        SUCCESS
    }
    unsafe extern "C" fn temp(_: Handle, sensor: c_uint, t: *mut c_uint) -> Ret {
        assert_eq!(sensor, 0);
        unsafe { *t = 61 };
        SUCCESS
    }
    unsafe extern "C" fn procs(h: Handle, n: *mut c_uint, out: *mut ProcessInfo) -> Ret {
        // Device 1 runs pids 100 (2 MiB) and 200 (memory unknown); device 2 runs pid 100 (1 MiB).
        let list: &[ProcessInfo] = if h as usize == 1 {
            &[
                ProcessInfo {
                    pid: 100,
                    used_gpu_memory: 2 << 20,
                    ..Default::default()
                },
                ProcessInfo {
                    pid: 200,
                    used_gpu_memory: VALUE_NOT_AVAILABLE,
                    ..Default::default()
                },
            ]
        } else {
            &[ProcessInfo {
                pid: 100,
                used_gpu_memory: 1 << 20,
                ..Default::default()
            }]
        };
        unsafe {
            if out.is_null() || (*n as usize) < list.len() {
                *n = list.len() as c_uint;
                return INSUFFICIENT_SIZE;
            }
            ptr::copy_nonoverlapping(list.as_ptr(), out, list.len());
            *n = list.len() as c_uint;
        }
        SUCCESS
    }
    unsafe extern "C" fn no_procs(_: Handle, n: *mut c_uint, _: *mut ProcessInfo) -> Ret {
        unsafe { *n = 0 };
        SUCCESS
    }
    unsafe extern "C" fn samples(
        h: Handle,
        out: *mut UtilSample,
        n: *mut c_uint,
        since: c_ulonglong,
    ) -> Ret {
        let all = [
            UtilSample {
                pid: 100,
                time_stamp: 10,
                sm_util: 30,
                mem_util: 0,
                enc_util: 0,
                dec_util: 40,
            },
            UtilSample {
                pid: 300,
                time_stamp: 20,
                sm_util: 90,
                ..Default::default()
            },
        ];
        let fresh: Vec<UtilSample> = if h as usize == 1 {
            all.iter()
                .copied()
                .filter(|s| s.time_stamp > since)
                .collect()
        } else {
            Vec::new()
        };
        unsafe {
            if out.is_null() || (*n as usize) < fresh.len() {
                *n = fresh.len() as c_uint;
                return if fresh.is_empty() {
                    6
                } else {
                    INSUFFICIENT_SIZE
                };
            }
            ptr::copy_nonoverlapping(fresh.as_ptr(), out, fresh.len());
            *n = fresh.len() as c_uint;
        }
        SUCCESS
    }

    fn fake() -> Nvml {
        Nvml::with_api(Api {
            device_count: count,
            handle_by_index: by_index,
            name,
            utilization: util,
            memory: mem,
            temperature: temp,
            compute_procs: Some(procs),
            graphics_procs: Some(no_procs),
            process_util: Some(samples),
        })
        .unwrap()
    }

    #[test]
    fn reads_device_level_values_and_tolerates_missing_ones() {
        let g = fake().gpus();
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].name, "Fake GPU 1");
        assert_eq!(g[0].util_percent, Some(37));
        assert_eq!(g[0].mem_used_kib, Some(2 << 20));
        assert_eq!(g[0].mem_total_kib, Some(8 << 20));
        assert_eq!(g[0].temp_c, Some(61));
        assert_eq!((g[1].mem_used_kib, g[1].mem_total_kib), (None, None));
    }

    #[test]
    fn sums_process_memory_over_devices_and_skips_unknown() {
        let mut n = fake();
        let p = n.processes();
        assert_eq!(p[&100].mem_kib, Some(2048 + 1024));
        assert_eq!(p.get(&200).and_then(|u| u.mem_kib), None);
    }

    #[test]
    fn utilisation_uses_busiest_engine_and_only_newer_samples() {
        let mut n = fake();
        let p = n.processes();
        assert_eq!(p[&100].util_percent, Some(40));
        assert_eq!(p[&300].util_percent, Some(90));
        let again = n.processes();
        assert_eq!(
            again.get(&100).and_then(|u| u.util_percent),
            None,
            "already seen"
        );
    }

    #[test]
    fn no_device_means_no_nvml() {
        unsafe extern "C" fn zero(n: *mut c_uint) -> Ret {
            unsafe { *n = 0 };
            SUCCESS
        }
        let api = Api {
            device_count: zero,
            handle_by_index: by_index,
            name,
            utilization: util,
            memory: mem,
            temperature: temp,
            compute_procs: None,
            graphics_procs: None,
            process_util: None,
        };
        assert!(Nvml::with_api(api).is_none());
    }
}

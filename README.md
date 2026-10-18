# task-manager
A Windows 7/8/10-like task manager for Linux written in Rust

## Status

Step 1 (data core): `tm-core` is a stateful `/proc` sampler (system and per-core CPU, memory, per-process CPU/RSS) and `tm-cli` prints a top-style table to check it.

```
cargo run --release -p tm-cli -- [interval_secs] [rows]
```

Step 2 (Windows 7-style MVP): `tm-gui` is a [Slint](https://slint.dev) window with three tabs.

- **Processes**: sortable columns (click a header), selection that follows a process as rows re-sort, and **End Process** with a confirmation prompt.
- **Performance**: CPU and memory history graphs plus one graph per core.
- **Users**: processes, CPU and memory totals per user.

The refresh rate (1 s by default, or 2 s, 5 s or 10 s) is in the bottom right.

```
cargo run --release -p tm-gui
```

Building needs `libfontconfig1-dev` (Debian/Ubuntu). Runtime needs: an X11 or Wayland session. On X11, `libxkbcommon-x11` must be installed (Debian/Ubuntu: `libxkbcommon-x11-0`). Slint is used under its royalty-free license terms.

### Ending other users' processes

The GUI never runs as root. It sends SIGTERM itself, and if that is refused with "permission denied" it runs `pkexec /usr/bin/kill -TERM -- <pid>`, so polkit shows its normal authentication dialog. To get a clear prompt text, install the policy file:

```
sudo install -m 644 packaging/org.taskmanager.end-process.policy /usr/share/polkit-1/actions/
```

Without it pkexec still works with polkit's generic prompt. Note the policy covers `/usr/bin/kill` as a whole, as `sudo kill` would; a dedicated signal helper is a possible later hardening.

### Network column

The **Network** column (Processes and Details) shows each process's TCP receive plus send rate. It needs no root and no setup: the kernel's `sock_diag` netlink interface hands any user per-socket byte counters, and a socket is matched to its process through `/proc/<pid>/fd`, which is readable for your own processes (all processes if the GUI runs as root). Other users' processes, and systems where `sock_diag` is unavailable (some containers or sandboxes), show "—". UDP traffic is not counted because the kernel keeps no byte counters for it. Rates refresh with the other slow readings (every 5 s).

### NVIDIA GPUs

With the NVIDIA proprietary driver installed, the Performance tab gets one graph per GPU (name, load, memory, temperature), and the GPU columns fill in per process (load from NVML's process samples, memory from its process lists). NVML (`libnvidia-ml.so.1`, shipped with the driver; Debian/Ubuntu package `libnvidia-compute-*` / `nvidia-utils-*`) is loaded with `dlopen` at runtime, so nothing is linked and no root is needed. Without an NVIDIA driver nothing is shown; with the kernel driver but no NVML library the tab shows a one-line hint. Intel, AMD and other DRM drivers are covered separately by the fdinfo reader.

### Performance budget

`scripts/perf-check.sh target/release/tm-gui` starts Xvfb and about 1,000 processes, runs the release binary at the default 1 s refresh and fails if it uses 2% or more of one core. It also prints the floor: merely reading `/proc/<pid>/stat` for 1,000 processes costs about 0.9% of a core per second on a GitHub runner, so the original 1% target was not reachable at a 1 s refresh. The GUI measures about 1.6% there, roughly 0.7% above that floor. CI runs it and uploads the release binary as an artifact.

Notes: user names come from `/etc/passwd` only (no LDAP/NSS lookups yet); the Users tab shows resource totals per user, not login sessions.

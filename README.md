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

The refresh rate (0.5 s, 1 s, 4 s or paused) is in the bottom right.

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

### Performance budget

`scripts/perf-check.sh target/release/tm-gui` starts Xvfb and about 1,000 processes, runs the release binary at the default 1 s refresh and fails if it uses 2% or more of one core. It also prints the floor: merely reading `/proc/<pid>/stat` for 1,000 processes costs about 0.9% of a core per second on a GitHub runner, so the original 1% target was not reachable at a 1 s refresh. The GUI measures about 1.6% there, roughly 0.7% above that floor. CI runs it and uploads the release binary as an artifact.

Notes: user names come from `/etc/passwd` only (no LDAP/NSS lookups yet); the Users tab shows resource totals per user, not login sessions.

#!/usr/bin/env bash
# CPU budget check: tm-gui at the default 1 s refresh with ~1000 processes must
# stay under MAX_CPU_PERCENT of one core (default 2.0). Just reading /proc/<pid>/stat
# for 1000 processes costs ~0.9% of a core per second on a CI runner (see the
# "floor" line it prints), so a 1% budget is below what any monitor can reach.
# Needs Xvfb.
# Usage: scripts/perf-check.sh target/release/tm-gui
set -euo pipefail

BIN=${1:?usage: perf-check.sh <tm-gui binary>}
MAX=${MAX_CPU_PERCENT:-2.0}
PROCS=${PROCS:-1000}
WARMUP=${WARMUP_SECS:-5}
WINDOW=${WINDOW_SECS:-30}
DISPLAY_NUM=${DISPLAY_NUM:-98}

pids=()
cleanup() { kill "${pids[@]}" 2>/dev/null || true; }
trap cleanup EXIT

existing=$(find /proc -maxdepth 1 -name '[0-9]*' | wc -l)
for ((i = existing; i < PROCS; i++)); do
    sleep 600 &
    pids+=($!)
done
echo "processes on system: $(find /proc -maxdepth 1 -name '[0-9]*' | wc -l)"

Xvfb ":$DISPLAY_NUM" -screen 0 1280x800x24 >/dev/null 2>&1 &
pids+=($!)
sleep 2
DISPLAY=":$DISPLAY_NUM" "$BIN" >/dev/null 2>&1 &
gui=$!
pids+=($gui)

# utime+stime in clock ticks; strip "pid (comm) " first because comm may contain spaces.
ticks() { sed 's/^.*) //' "/proc/$gui/stat" | awk '{print $12 + $13}'; }

# Informational: the floor for any monitor is one open/read/close of /proc/<pid>/stat per process.
python3 - <<'PY'
import os, time
pids = [p for p in os.listdir('/proc') if p.isdigit()]
t = time.perf_counter()
for _ in range(20):
    for p in pids:
        try:
            fd = os.open(f'/proc/{p}/stat', os.O_RDONLY)
            os.read(fd, 4096)
            os.close(fd)
        except OSError:
            pass
ms = (time.perf_counter() - t) / 20 * 1000
print(f"floor: reading stat of {len(pids)} processes takes {ms:.1f} ms per sweep = {ms / 10:.2f}% of a core at 1 s")
PY

sleep "$WARMUP"
kill -0 "$gui" # fail if the GUI crashed on startup
t0=$(ticks)
sleep "$WINDOW"
kill -0 "$gui"
t1=$(ticks)
for t in /proc/$gui/task/*; do echo "thread $(cat "$t/comm"): $(sed 's/^.*) //' "$t/stat" | awk '{print $12 + $13}') ticks total"; done
rss_kib=$(awk '/VmRSS/ {print $2}' "/proc/$gui/status")

hz=$(getconf CLK_TCK)
pct=$(awk -v a="$t0" -v b="$t1" -v hz="$hz" -v w="$WINDOW" 'BEGIN {printf "%.2f", (b - a) / hz / w * 100}')
echo "tm-gui CPU: ${pct}% of one core over ${WINDOW}s (budget ${MAX}%), RSS ${rss_kib} KiB"
awk -v p="$pct" -v m="$MAX" 'BEGIN {exit !(p < m)}' || { echo "FAIL: over CPU budget"; exit 1; }

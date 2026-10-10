#!/usr/bin/env bash
# Perf gate: drives a release build through scripted phases and checks peak RSS and CPU.
# Plays audio: the script sets the volume to 5% first.
# Default: measures the in-process `requiem`. With --daemon: measures a release `requiemd`
# driven by `requiem --connect`.
set -euo pipefail

daemon=0
case "${1:-}" in
  "") ;;
  --daemon) daemon=1 ;;
  *) echo "usage: $0 [--daemon]" >&2; exit 2 ;;
esac

cd "$(dirname "$0")/.."

CARGO_BIN="${CARGO:-cargo}"
export PERF_FOOTPRINT_KIB="${PERF_FOOTPRINT_KIB:-30720}"
export PERF_CPU_PLAY="${PERF_CPU_PLAY:-1.0}"
export PERF_CPU_IDLE="${PERF_CPU_IDLE:-0.5}"
# Seeks and track changes refill the ring in a burst; the 1% budget is for steady playback.
export PERF_CPU_BURST="${PERF_CPU_BURST:-2.0}"

if [ "$daemon" -eq 1 ]; then
  "$CARGO_BIN" build --release -p requiem-cli -p requiem-daemon
else
  "$CARGO_BIN" build --release -p requiem-cli
fi

work="$(mktemp -d)"
pid=""
trap 'if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; fi; rm -rf "$work"' EXIT
script="$work/perf.script"
log="$work/stdout.log"
samples="$work/samples.log"

cat >"$script" <<'SCRIPT'
mark idle_search
vol 5
search daft punk around the world
await results
sleep 10
mark play
play 1
add 2
await playing 30
sleep 60
mark seeks
seek +60
sleep 5
seek -30
sleep 5
seek 200
sleep 5
mark transition
next
await playing 30
sleep 15
mark idle_end
next
await stopped 10
sleep 10
mark end
SCRIPT
if [ "$daemon" -eq 1 ]; then
  echo shutdown-daemon >>"$script"
fi
echo quit >>"$script"

cli_pid=""
if [ "$daemon" -eq 1 ]; then
  mkdir -m 0700 "$work/sock"
  export REQUIEM_SOCKET_DIR="$work/sock"
  # The sampled pid is the daemon's; the CLI only supplies the MARK lines.
  REQUIEM_LOG="${PERF_DEBUG:+debug}" target/release/requiemd 2>"$work/stderr.log" &
  pid=$!
  # A client that connects before the daemon listens would spawn a second daemon.
  for _ in $(seq 100); do
    [ -S "$work/sock/daemon.sock" ] && break
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.1
  done
  target/release/requiem --connect --script "$script" >"$log" 2>"$work/cli-stderr.log" &
  cli_pid=$!
else
  REQUIEM_LOG="${PERF_DEBUG:+debug}" target/release/requiem --script "$script" >"$log" 2>"$work/stderr.log" &
  pid=$!
fi

# Sample until the process is gone. Memory gate: macOS physical footprint
# (proc_pid_rusage ri_phys_footprint, bytes), Linux Pss from smaps_rollup; `ps` RSS is
# informational because it counts ~10-12 MiB of shared system-framework pages. CPU time
# comes from /proc where available because `ps` reports whole seconds on Linux.
python3 -I - "$pid" "$samples" <<'PY' || true
import os, subprocess, sys, time

pid, path = int(sys.argv[1]), sys.argv[2]
tck = os.sysconf("SC_CLK_TCK") if os.path.exists(f"/proc/{pid}/stat") else None

if tck is None:
    import ctypes
    _libproc = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)

def footprint_kib():
    if tck:
        with open(f"/proc/{pid}/smaps_rollup") as f:
            for line in f:
                if line.startswith("Pss:"):
                    return int(line.split()[1])
        raise ValueError("no Pss")
    buf = ctypes.create_string_buffer(512)
    # RUSAGE_INFO_V0: uuid[16] then seven u64 counters (resident_size at 64), then ri_phys_footprint at offset 72.
    if _libproc.proc_pid_rusage(pid, 0, buf) != 0:
        raise OSError("proc_pid_rusage failed")
    return int.from_bytes(buf.raw[72:80], "little") // 1024

def ps_cpu(t):
    days = 0
    if "-" in t:
        d, t = t.split("-")
        days = int(d)
    secs = 0.0
    for part in t.split(":"):
        secs = secs * 60 + float(part)
    return days * 86400 + secs

def proc_cpu():
    with open(f"/proc/{pid}/stat") as f:
        rest = f.read().rsplit(")", 1)[1].split()
    return (int(rest[11]) + int(rest[12])) / tck

with open(path, "w") as out:
    while True:
        t0 = time.time()
        r = subprocess.run(["ps", "-o", "rss=,time=", "-p", str(pid)],
                           capture_output=True, text=True)
        fields = r.stdout.split()
        if r.returncode != 0 or len(fields) != 2:
            break
        try:
            cpu = proc_cpu() if tck else ps_cpu(fields[1])
            fp = footprint_kib()
        except (OSError, IndexError, ValueError):
            break
        out.write(f"{int(time.time() * 1000)} {fields[0]} {cpu} {fp}\n")
        out.flush()
        time.sleep(max(0.0, 0.5 - (time.time() - t0)))
PY

status=0
wait "$pid" || status=$?
pid=""
if [ -n "$cli_pid" ]; then
  cli_status=0
  wait "$cli_pid" || cli_status=$?
  if [ "$status" -eq 0 ] && [ "$cli_status" -ne 0 ]; then
    echo "requiem --connect exited with status $cli_status" >&2
    status=$cli_status
  fi
fi

summary=0
python3 -I - "$log" "$samples" "$status" <<'PY' || summary=$?
import os, sys

log, samples_path, status = sys.argv[1], sys.argv[2], int(sys.argv[3])
fp_cap = float(os.environ["PERF_FOOTPRINT_KIB"])
cpu_play = float(os.environ["PERF_CPU_PLAY"])
cpu_idle = float(os.environ["PERF_CPU_IDLE"])
cpu_burst = float(os.environ["PERF_CPU_BURST"])
phases = ["idle_search", "play", "seeks", "transition", "idle_end"]
limit = {"idle_search": cpu_idle, "idle_end": cpu_idle,
         "play": cpu_play, "seeks": cpu_burst, "transition": cpu_burst}

marks = {}
with open(log) as f:
    for line in f:
        p = line.split()
        if len(p) == 3 and p[0] == "MARK":
            marks[p[1]] = int(p[2])
samples = []
with open(samples_path) as f:
    for line in f:
        t, rss, cpu, fp = line.split()
        samples.append((int(t), int(rss), float(cpu), int(fp)))

failures = []
if status != 0:
    failures.append(f"process exited with status {status}")
names = phases + ["end"]
missing = [n for n in names if n not in marks]
if missing:
    failures.append(f"missing marks: {', '.join(missing)}")
if not samples:
    failures.append("no samples collected")

rows = []
if not missing and samples:
    def nearest(ms):
        return min(samples, key=lambda s: abs(s[0] - ms))
    for name, nxt in zip(phases, names[1:]):
        a, b = marks[name], marks[nxt]
        sa, sb = nearest(a), nearest(b)
        inside = [s for s in samples if sa[0] <= s[0] <= sb[0]]
        peak = max(s[1] for s in inside) if inside else 0
        peak_fp = max(s[3] for s in inside) if inside else 0
        wall = (sb[0] - sa[0]) / 1000.0
        cpu = (sb[2] - sa[2]) / wall * 100 if wall > 0 else float("nan")
        rows.append((name, wall, peak_fp, peak, cpu))
        if peak_fp > fp_cap:
            failures.append(f"{name}: peak footprint {peak_fp} KiB > {fp_cap:.0f} KiB")
        if not cpu < limit[name]:
            failures.append(f"{name}: CPU {cpu:.2f}% >= {limit[name]}%")

print(f"{'phase':<12} {'seconds':>8} {'peak footprint MiB':>19} {'peak RSS MiB (info)':>20} {'CPU %':>7}")
for name, wall, fp, peak, cpu in rows:
    print(f"{name:<12} {wall:>8.1f} {fp / 1024:>19.1f} {peak / 1024:>20.1f} {cpu:>7.2f}")
if failures:
    for f in failures:
        print(f"FAIL: {f}")
    sys.exit(1)
print("PASS")
PY

if [ "$summary" -ne 0 ]; then
  echo "--- stdout (tail) ---"
  tail -n 15 "$log"
  echo "--- stderr (tail) ---"
  tail -n 25 "$work/stderr.log"
  if [ -n "$cli_pid" ]; then
    echo "--- cli stderr (tail) ---"
    tail -n 25 "$work/cli-stderr.log"
  fi
fi
exit "$summary"

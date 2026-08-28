#!/usr/bin/env bash
# Plan 7 campaign: cyclictest (idle, load) then rt_bringup (idle, load + tcpdump).
# Run from ~/bench on the edge, TIA already at a 1 ms update time.
set -euo pipefail

DURATION="${1:-600}"
BENCH="${BENCH:-$HOME/bench}"
BIN="${BIN:-$BENCH/rt_bringup}"
PLC_IF="${PLC_IF:-eno2}"
DEV_IP="${DEV_IP:-172.16.2.10}"
RT_CPU="${RT_CPU:-3}"
HK_CPUS="${HK_CPUS:-0-2}"
RT_PRIO="${RT_PRIO:-80}"
STAMP="${STAMP:-$(date +%Y%m%d-%H%M%S)}"
OUT="$BENCH/logs/plan7-$STAMP"

# Background jobs (load.sh, tcpdump) currently running, if any; kept empty when
# none are outstanding so a signal or an early `set -e` exit never leaves
# stress-ng/tcpdump running unattended for up to DURATION+20s.
LOAD=""
DUMP=""
cleanup() {
  local p
  for p in "$LOAD" "$DUMP"; do
    if [ -n "$p" ]; then
      kill "$p" 2>/dev/null || true
    fi
  done
  wait 2>/dev/null || true
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

[ "$(cat /sys/kernel/realtime 2>/dev/null)" = "1" ] || { echo "not PREEMPT_RT" >&2; exit 2; }
grep -qw "$RT_CPU" /sys/devices/system/cpu/isolated || { echo "cpu $RT_CPU not isolated" >&2; exit 2; }
[ -x "$BIN" ] || { echo "$BIN missing" >&2; exit 2; }
mkdir -p "$OUT"
echo "campaign dir: $OUT"

{ uname -r; cat /proc/cmdline; cat /sys/devices/system/cpu/isolated; systemctl status profinet-rt-tune --no-pager 2>&1 | tail -n +1; } > "$OUT/env.txt" 2>&1 || true

step() { echo "== $(date +%T) $*"; }

step "1/4 cyclictest idle ($DURATION s)"
set +e
cyclictest -m -p"$RT_PRIO" -a"$RT_CPU" -i1000 -h400 -D"$DURATION" -q > "$OUT/cyclictest-idle.txt"
CT_IDLE_RC=$?
set -e

step "2/4 cyclictest under load"
"$BENCH/load.sh" "$((DURATION + 10))" > "$OUT/load-cyclictest.txt" 2>&1 &
LOAD=$!
sleep 5
set +e
cyclictest -m -p"$RT_PRIO" -a"$RT_CPU" -i1000 -h400 -D"$DURATION" -q > "$OUT/cyclictest-load.txt"
CT_LOAD_RC=$?
set -e
wait "$LOAD" || true
LOAD=""

RT_ARGS=(--iface "$PLC_IF" --ip "$DEV_IP" --rt-priority "$RT_PRIO" --cpu "$RT_CPU" --app-cpus "$HK_CPUS" --lock-memory --duration "$DURATION" --stats-every 5)

step "3/4 rt_bringup idle"
set +e
"$BIN" "${RT_ARGS[@]}" --csv "$OUT/rt-idle.csv" > "$OUT/rt-idle.log" 2>&1
IDLE_RC=$?
set -e
sleep 10   # let the CPU notice the device is gone and settle

step "4/4 rt_bringup under load + tcpdump"
"$BENCH/load.sh" "$((DURATION + 20))" > "$OUT/load-rt.txt" 2>&1 &
LOAD=$!
taskset -c "$HK_CPUS" tcpdump -i "$PLC_IF" -B 65536 -w "$OUT/rt-load.pcapng" > "$OUT/tcpdump.txt" 2>&1 &
DUMP=$!
sleep 5
set +e
"$BIN" "${RT_ARGS[@]}" --csv "$OUT/rt-load.csv" > "$OUT/rt-load.log" 2>&1
LOAD_RC=$?
set -e
kill -TERM "$DUMP" 2>/dev/null || true
wait "$LOAD" "$DUMP" 2>/dev/null || true
LOAD=""
DUMP=""

{
  echo "campaign $STAMP, duration $DURATION s"
  echo "cyclictest idle (rc=$CT_IDLE_RC): $(grep -E '^# Max Latencies' "$OUT/cyclictest-idle.txt" || tail -1 "$OUT/cyclictest-idle.txt")"
  echo "cyclictest load (rc=$CT_LOAD_RC): $(grep -E '^# Max Latencies' "$OUT/cyclictest-load.txt" || tail -1 "$OUT/cyclictest-load.txt")"
  echo "--- rt_bringup idle (rc=$IDLE_RC)"; sed -n '/rt_bringup summary/,$p' "$OUT/rt-idle.log"
  echo "--- rt_bringup load (rc=$LOAD_RC)"; sed -n '/rt_bringup summary/,$p' "$OUT/rt-load.log"
} | tee "$OUT/summary.txt"

[ "$IDLE_RC" -eq 0 ] && [ "$LOAD_RC" -eq 0 ]

#!/usr/bin/env bash
# Edge tuning for the 1 ms PROFINET RT campaign (spec §5.2). Idempotent; run as root.
# Prints ok/warn/FAIL per step and the resulting state at the end.
set -euo pipefail

PLC_IF="${PLC_IF:-eno2}"
RT_CPU="${RT_CPU:-3}"
HK_CPUS="${HK_CPUS:-0-2}"
IRQ_PRIO="${IRQ_PRIO:-90}"
RX_USECS="${RX_USECS:-0}"
TX_USECS="${TX_USECS:-0}"
EEE="${EEE:-off}"

ok()   { echo "ok    $*"; }
warn() { echo "warn  $*" >&2; }
fail() { echo "FAIL  $*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || fail "run as root"

# 1. preconditions
[ "$(cat /sys/kernel/realtime 2>/dev/null)" = "1" ] || fail "not a PREEMPT_RT kernel"
isolated="$(cat /sys/devices/system/cpu/isolated)"
# Word match on the kernel's isolated-CPU list/range string (e.g. "3" or "2-3"),
# same test as campaign.sh: a `,$isolated,` comma-list case match doesn't recognize
# a range like "2-3".
grep -qw "$RT_CPU" /sys/devices/system/cpu/isolated && ok "cpu $RT_CPU isolated" \
  || fail "cpu $RT_CPU not in isolated='$isolated' (GRUB cmdline?)"

# 2. governor
for g in /sys/devices/system/cpu/cpu[0-9]*/cpufreq/scaling_governor; do
  echo performance > "$g" 2>/dev/null && ok "$g = performance" || warn "$g not writable"
done

# 3. single queue. Must run before steps 4, 5 and the state block below: `ethtool -L`
# re-registers the NIC's MSI-X vectors, so any IRQ numbers read before this point are stale.
if ethtool -L "$PLC_IF" combined 1 >/dev/null 2>&1; then ok "$PLC_IF combined 1"; else warn "$PLC_IF: ethtool -L unsupported"; fi

# 4. IRQ affinity. After step 3, igb names the per-queue vectors "<ifc>-rx-0"/
# "<ifc>-tx-0" (no "TxRx") and keeps a bare "<ifc>" misc vector. Anchor the awk match
# on the exact field ("== ifc" or "^ifc-") so a sibling interface like "eno20" can't
# match.
found_queue_vector=0
while read -r irq name; do
  irq="${irq%:}"
  if [[ "$name" == "${PLC_IF}-"* ]]; then
    found_queue_vector=1
    echo "$RT_CPU" > "/proc/irq/$irq/smp_affinity_list" && ok "irq $irq ($name) -> cpu $RT_CPU" \
      || warn "irq $irq ($name): smp_affinity_list not writable"
  else
    echo "$HK_CPUS" > "/proc/irq/$irq/smp_affinity_list" && ok "irq $irq ($name) -> cpus $HK_CPUS" \
      || warn "irq $irq ($name): smp_affinity_list not writable"
  fi
done < <(awk -v ifc="$PLC_IF" '$NF == ifc || $NF ~ "^"ifc"-" {print $1, $NF}' /proc/interrupts)
[ "$found_queue_vector" -eq 1 ] || warn "no ${PLC_IF}-* vector found"

# 5. IRQ thread priority (threaded IRQs on PREEMPT_RT). Kernel threads have an empty
# cmdline, so `pgrep -f` never matches; match on comm instead. igb's queue threads are
# named "irq/<n>-<ifc>-rx-0"/"irq/<n>-<ifc>-tx-0" (trailing dash before the role); the
# misc thread "irq/<n>-<ifc>" (no trailing dash) is left alone.
found_irq_thread=0
for pid in $(pgrep "^irq/[0-9]+-${PLC_IF}-" || true); do
  found_irq_thread=1
  chrt -f -p "$IRQ_PRIO" "$pid" && ok "irq thread pid $pid -> SCHED_FIFO $IRQ_PRIO" \
    || warn "irq thread pid $pid: chrt failed"
done
[ "$found_irq_thread" -eq 1 ] || warn "no irq thread found for ${PLC_IF}"

# 6. NIC latency knobs (igb support is a hypothesis: warn, never fail)
ethtool --set-eee "$PLC_IF" eee "$EEE" >/dev/null 2>&1 && ok "$PLC_IF eee $EEE" || warn "$PLC_IF: eee not settable"
ethtool -C "$PLC_IF" rx-usecs "$RX_USECS" tx-usecs "$TX_USECS" >/dev/null 2>&1 && ok "$PLC_IF coalescing rx=$RX_USECS tx=$TX_USECS" || warn "$PLC_IF: coalescing not settable"
ethtool -K "$PLC_IF" gro off lro off >/dev/null 2>&1 && ok "$PLC_IF gro/lro off" || warn "$PLC_IF: gro/lro not settable"

# 7. sysctl
sysctl -q -w kernel.sched_rt_runtime_us=-1 && ok "sched_rt_runtime_us=-1" || warn "kernel.sched_rt_runtime_us not settable"
sysctl -q -w kernel.timer_migration=0 && ok "timer_migration=0" || warn "kernel.timer_migration not settable"
sysctl -q -w vm.stat_interval=120 && ok "vm.stat_interval=120" || warn "vm.stat_interval not settable"

# 8. state
echo "----- state -----"
echo "kernel:    $(uname -r)  realtime=$(cat /sys/kernel/realtime)"
echo "cmdline:   $(cat /proc/cmdline)"
echo "isolated:  $isolated   nohz_full: $(cat /sys/devices/system/cpu/nohz_full)"
echo "governor:  $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor)"
echo "cpuidle:   $(for s in /sys/devices/system/cpu/cpu$RT_CPU/cpuidle/state*; do printf '%s(disable=%s) ' "$(cat "$s/name")" "$(cat "$s/disable")"; done)"
awk -v ifc="$PLC_IF" '$NF == ifc || $NF ~ "^"ifc"-" {gsub(":","",$1); print "irq " $1 " " $NF}' /proc/interrupts | while read -r _ irq name; do
  echo "irq:       $irq $name affinity=$(cat /proc/irq/$irq/smp_affinity_list)"
done
for pid in $(pgrep "^irq/[0-9]+-${PLC_IF}-" || true); do echo "irqthread: pid $pid $(chrt -p "$pid" | tr '\n' ' ')"; done
ethtool -l "$PLC_IF" 2>/dev/null | sed -n '/Current/,$p' | tr '\n' ' ' || true; echo
ethtool -c "$PLC_IF" 2>/dev/null | grep -E '^(rx-usecs|tx-usecs):' | tr '\n' ' ' || true; echo
ethtool --show-eee "$PLC_IF" 2>/dev/null | grep -i 'EEE status' || true

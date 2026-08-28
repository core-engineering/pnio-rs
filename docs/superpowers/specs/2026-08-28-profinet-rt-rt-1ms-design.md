# Spec — Plan 7: 1 ms determinism (PREEMPT_RT edge, hardened RT path, jitter campaign)

Date: 2026-08-28. Status: implemented (feat/rt-1ms), HIL campaign 2026-08-28 — see
docs/bench-pnet-device.md §6e.
Parent: [`2026-06-25-profinet-rt-device-design.md`](2026-06-25-profinet-rt-device-design.md) §5.2 (thread model), §8.4 (determinism), §9 (NIC jitter risk).
Builds on Plan 4 ([`2026-08-28-profinet-rt-rt-cyclic-design.md`](2026-08-28-profinet-rt-rt-cyclic-design.md)): the RT thread, `timerfd` pacing, `RtStats`, `RtOptions` and `examples/rt_bringup.rs` exist and hold 32 ms on a stock kernel (bench §6d run 4: 10 min, 18 717 frames, 0 missed ticks, max lateness 395 µs).

## 1. Goal

Hold a **1 ms update time** (send clock 1 ms × reduction ratio 1) against the S7-1500 CPU 1515-2 PN on the edge `lab-server` (Intel Atom E3940, 4 cores, Debian 13, kernel `6.12.105+deb13-rt-amd64` PREEMPT_RT), **at idle and under load**, and prove it with numbers.

**Success criteria** (all four, over a 10-minute run, at idle *and* under load):
1. `missed_ticks == 0` and `watchdog_expirations == 0` (no `RtWatchdog` abort, device stays green in TIA).
2. Tick lateness (timer wake-up − scheduled expiry): **p99.99 < 100 µs, max < 300 µs** (the consumer watchdog is 3 cycles = 3 ms).
3. CPU→device inter-arrival interval (`0x8001` frames, measured by the RT thread): **max < 1.5 ms**.
4. Watch table unchanged from Plan 4: `%IB0 == %QB0`, `%ID2 == %QD2`, `%ID6 == %QD6`; TIA diagnostic buffer clean.

Load = `stress-ng --cpu 3 --vm 1 --vm-bytes 512M` pinned to CPUs 0-2 plus `tcpdump` capturing on `eno2`.

Deliverables: (a) reproducible edge tuning (script + systemd unit + GRUB cmdline doc), (b) hardened crate RT path (no allocation, no foreign wake-ups, locked memory), (c) instrumentation that proves the numbers (histograms + CSV + PASS/FAIL verdict), (d) campaign report in `docs/bench-pnet-device.md` §6e.

## 2. Scope

In:
- `bench/edge-rt-tune.sh` + `bench/profinet-rt-tune.service` + GRUB cmdline documentation.
- `eth`: `EthTransport::recv_into`, per-socket classic BPF filters (`eth::bpf`), `AfPacketTransport::attach_filter`.
- `rt::sched`: `set_fifo`, `set_affinity`, `lock_memory` (raw syscalls, musl-safe), public.
- `rt::hist`: fixed-bin latency histogram; three histograms in `RtStats`; new maxima in `StatsSnapshot`.
- `RtOptions.lock_memory`.
- `examples/rt_bringup.rs`: `--lock-memory`, `--app-cpus`, `--duration`, `--csv`, threshold flags, PASS/FAIL verdict and exit code, 1 ms application loop.
- `bench/load.sh`, `bench/campaign.sh`, `bench/README.md`.
- HIL campaign, `docs/bench-pnet-device.md` §6e, `README.md` status line, `FOLLOWUPS.md` Plan 7 section.
- Seqlock decision rule (§9) — decided by the campaign counters, recorded, not implemented here.

Out (FOLLOWUPS at close-out):
- `PACKET_MMAP` (TPACKET_V3 rings), `SO_BUSY_POLL` / spinning RT thread, XDP.
- Seqlock / lock-free I/O image (only if §9 says so → Plan 7bis).
- `PACKET_AUXDATA` (RX VLAN priority).
- Alarms, I&M, diagnosis (Plan 5); typed I/O API, GSDML (Plan 6).
- Any protocol change. The GSDML already declares `MinDeviceInterval=32` (1 ms); TIA only changes the update time.

## 3. Decisions (locked in brainstorm)

| Subject | Decision | Why |
|---|---|---|
| Approach | Kernel + isolation + minimal code (approach A) | Escalate to `PACKET_MMAP` / busy-poll only if the histograms fail the thresholds |
| Kernel | Debian `linux-image-rt-amd64` (6.12.105 PREEMPT_RT, signed) | Installed 2026-08-28; no custom build |
| Isolated core | CPU 3 = RT thread + `eno2` IRQ only; everything else on 0-2 | 4-core Atom; NAT gateway, ssh, acyclic thread need the rest |
| C-states | `intel_idle.max_cstate=1 processor.max_cstate=1` (C1 kept, 2 µs) | Bench shows C6…C10 with up to 5.9 ms exit latency |
| Priorities | RT thread `SCHED_FIFO 80`; `eno2` IRQ thread `SCHED_FIFO 90` | The CPU's frame is processed before our wake-up, never behind it |
| NIC queues | `ethtool -L eno2 combined 1`, one IRQ vector on CPU 3 | No RSS spreading across cores |
| Memory | `mlockall(MCL_CURRENT \| MCL_FUTURE)` + RT stack pre-fault, opt-in `RtOptions.lock_memory` | No page fault on the RT path |
| RX path | `recv_into` into a fixed buffer; cBPF filter per socket | Zero allocation per cycle; RT socket wakes only for RTC1, acyclic socket only for ≥ `0xFC00` |
| Metrics | Three 1 µs-bin histograms (`tick_lateness`, `cycle_work`, `rx_interval`) in `RtStats` | Percentiles, not just maxima; per cycle: 3 histograms × (2 fetch_add + 1 fetch_max) |
| Verdict | Computed by `rt_bringup` against thresholds, exit code 0/1 | The campaign script is mechanical; no reading tea leaves |
| Baseline | `cyclictest` on CPU 3, idle and loaded, before any `rt_bringup` run | Separates kernel floor from crate cost |
| Services | Docker/containerd, wpa_supplicant, bluetooth, periodic timers disabled on the edge | Done 2026-08-28; NAT script creates `DOCKER-USER` itself |
| Dependencies | none new | Keep the crate minimal |

## 4. Architecture

```
bench/
  edge-rt-tune.sh          idempotent sysfs/ethtool/chrt tuning, prints final state
  profinet-rt-tune.service runs the script after network-online
  load.sh                  stress-ng on CPUs 0-2
  campaign.sh              cyclictest idle/load → rt_bringup idle/load (+tcpdump) → summary
  README.md                usage, GRUB cmdline, tshark post-processing
crates/profinet-rt/src/
  eth/transport.rs   + recv_into (trait), recv default on top of it
  eth/afpacket.rs    + native recv_into, attach_filter
  eth/bpf.rs         NEW: SockFilter, rt_filter(), acyclic_filter()
  rt/sched.rs        NEW: set_fifo, set_affinity, lock_memory (moved out of runner.rs)
  rt/hist.rs         NEW: Histogram
  rt/engine.rs       RtStats + histograms and maxima; StatsSnapshot
  rt/runner.rs       fixed RX buffer, recv_into, filter at spawn, lock_memory, cycle_work timing
  device/mod.rs      RtOptions.lock_memory → RtConfig
examples/rt_bringup.rs   flags, CSV, verdict
```
Rules unchanged from Plan 4: no allocation, no blocking lock, no logging inside the RT loop. New rule: **no syscall in the RT loop other than `poll`, `read(timerfd)`, `recvfrom`, `sendto`, `write(eventfd)`, `clock_gettime` (vDSO)**.

## 5. Edge tuning (`bench/edge-rt-tune.sh`, GRUB)

### 5.1 GRUB cmdline (one-shot, user runs it with sudo)

`/etc/default/grub`:
```
GRUB_CMDLINE_LINUX_DEFAULT="quiet isolcpus=domain,managed_irq,3 nohz_full=3 rcu_nocbs=3 irqaffinity=0-2 intel_idle.max_cstate=1 processor.max_cstate=1 nosoftlockup"
```
then `update-grub`, reboot, verify `/sys/devices/system/cpu/isolated` = `3`, `nohz_full` = `3`, `/sys/kernel/realtime` = `1`, `ip -4 addr show eno2` = `172.16.2.10/24`, NAT gateway still answering (`ping 192.168.1.200` from Windows, TTL 254).

### 5.2 Script (idempotent, `set -euo pipefail`, every step logged as `ok`/`warn`/`FAIL`)

Variables at the top: `PLC_IF=eno2`, `RT_CPU=3`, `HK_CPUS=0-2`, `IRQ_PRIO=90`, `RX_USECS=0`, `TX_USECS=0`, `EEE=off`.

Steps:
1. Preconditions: `/sys/kernel/realtime == 1`, CPU `RT_CPU` listed in `/sys/devices/system/cpu/isolated`; else **FAIL**.
2. Governor `performance` on all CPUs (`/sys/devices/system/cpu/cpu*/cpufreq/scaling_governor`).
3. `ethtool -L $PLC_IF combined 1` (**warn** if unsupported).
4. IRQ affinity: every `$PLC_IF-TxRx-*` vector → `RT_CPU`; the misc `$PLC_IF` vector → `HK_CPUS` (`/proc/irq/N/smp_affinity_list`).
5. IRQ thread priority: `chrt -f -p $IRQ_PRIO <pid of irq/N-eno2-TxRx-0>`.
6. NIC: `ethtool --set-eee $PLC_IF eee $EEE`; `ethtool -C $PLC_IF rx-usecs $RX_USECS tx-usecs $TX_USECS`; `ethtool -K $PLC_IF gro off lro off` — each **warn** on error (igb support is a hypothesis to confirm on the bench).
7. sysctl: `kernel.sched_rt_runtime_us=-1`, `kernel.timer_migration=0`, `vm.stat_interval=120`.
8. Print final state: isolated CPUs, governor, IRQ→CPU table, IRQ thread policy/priority, cpuidle states enabled, `ethtool -c/-l/--show-eee` summary. This block is pasted into the report.

`bench/profinet-rt-tune.service`: `Type=oneshot`, `RemainAfterExit=yes`, `After=network-online.target`, `ExecStart=/home/maintenance/bench/edge-rt-tune.sh`. Installed by the user (`sudo cp`, `sudo systemctl enable --now`).

### 5.3 Baseline

`cyclictest -m -p80 -a3 -i1000 -h400 -D600 -q` idle, then under `bench/load.sh`. Output kept in the campaign directory; the histogram max at idle is the kernel floor. If the loaded baseline already exceeds 100 µs, the problem is the edge, not the crate — stop and tune before running `rt_bringup`.

## 6. `eth` — `recv_into` and BPF filters

### 6.1 `recv_into`

```rust
pub trait EthTransport: Send + Sync {
    fn send(&self, frame: &[u8]) -> Result<(), TransportError>;
    /// Receive the next frame into `buf`; returns its length. Same `Ok(None)` contract as `recv`.
    /// `buf.len() < 1522` is a programming error and returns `TransportError::BufferTooSmall`.
    /// A frame longer than `buf` is an error, never a silent truncation.
    fn recv_into(&self, buf: &mut [u8], timeout: Option<Duration>) -> Result<Option<usize>, TransportError>;
    /// Default: allocate 1522 bytes, call `recv_into`, shrink.
    fn recv(&self, timeout: Option<Duration>) -> Result<Option<Vec<u8>>, TransportError> { ... }
    fn raw_fd(&self) -> Option<RawFd> { None }
}
```
- `AfPacketTransport::recv_into`: `recvfrom(MSG_TRUNC)` into `buf`; `PACKET_OUTGOING` drop unchanged; returned length > `buf.len()` → `TransportError::FrameTooLong { len }`.
- `MockTransport::recv_into`: copies the queued frame; same error on a short buffer.
- `TransportError` gains `BufferTooSmall` and `FrameTooLong { len: usize }`.
- `rt::runner`: one `[u8; 1522]` on the RT thread's stack (pre-faulted by `lock_memory`), `recv_into` everywhere; the "one allocation left" note in the module doc is deleted.

### 6.2 `eth::bpf`

```rust
#[repr(C)] #[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SockFilter { pub code: u16, pub jt: u8, pub jf: u8, pub k: u32 }
/// Accept 0x8892 frames (untagged or 802.1Q-tagged) whose FrameID is in `lo..=hi`.
pub fn frame_id_filter(lo: u16, hi: u16) -> Vec<SockFilter>;
pub fn rt_filter() -> Vec<SockFilter>       // frame_id_filter(0x8000, 0xBFFF)  RTC1
pub fn acyclic_filter() -> Vec<SockFilter>  // frame_id_filter(0xFC00, 0xFFFF)  alarms, DCP
```
Program shape (classic BPF): `ldh [12]`; `jeq 0x8892 → A`; `jeq 0x8100 → B`; `ret 0`; `A: ldh [14]`; `jge lo`; `jgt hi → ret 0`; `ret 0xFFFF`; `B: ldh [16]`; `jeq 0x8892`; `ldh [18]`; same range check. The kernel strips the VLAN tag on RX before the filter runs when offload is on, so both branches are needed. Built from a tiny in-crate assembler (`ld_abs_h`, `jeq`, `jge`, `jgt`, `ret`), not from `libc` constants copied by hand.

`AfPacketTransport::attach_filter(&self, prog: &[SockFilter]) -> Result<(), TransportError>` → `setsockopt(SO_ATTACH_FILTER)` with a `sock_fprog`. Frames already queued before attachment are drained by the existing zero-timeout loop.

Attachment points: `RtRunner::spawn` attaches `rt_filter()` on the socket it opens (**fatal on error**: an unfiltered run is not comparable); `examples/rt_bringup.rs` attaches `acyclic_filter()` on the socket it hands to `Device::new` (`AfPacketTransport` is opened by the application, not by `Device`). `spawn_with_transport` attaches nothing (the mock has no fd).

## 7. `rt::sched` and memory locking

```rust
pub fn set_fifo(priority: u8) -> io::Result<()>;          // SYS_sched_setscheduler, current thread
pub fn set_affinity(cpus: &[usize]) -> io::Result<()>;   // SYS_sched_setaffinity, current thread
pub fn lock_memory() -> io::Result<()>;                  // mlockall(MCL_CURRENT | MCL_FUTURE)
pub fn prefault_stack(bytes: usize);                     // touches `bytes` of the current stack, volatile
```
All raw `libc::syscall` (musl stubs `sched_setscheduler` with ENOSYS — Plan 4 lesson). `runner.rs` calls these instead of its private `set_fifo_priority` / `pin_to_cpu`, which are deleted. Order in the RT thread: `set_affinity` → `set_fifo` → `lock_memory` → `prefault_stack(256 KiB)` → loop. Each failure → `RtEvent::SchedWarning(String)` (already exists), never fatal; `rt_bringup` reports "memory not locked" in its summary when the warning was seen.

`RtOptions` gains `pub lock_memory: bool` (default `false`); `RtConfig` gains the same field; `device` copies it.

## 8. `rt::hist` and `RtStats`

```rust
pub const HIST_BINS: usize = 2048;                 // 1 µs bins, 0..=2047 µs, last bin = overflow
pub struct Histogram { bins: [AtomicU64; HIST_BINS], count: AtomicU64, max_ns: AtomicU64 }
impl Histogram {
    pub const fn new() -> Self;
    pub fn record(&self, ns: u64);                 // RT side: 2 fetch_add + 1 fetch_max, relaxed
    pub fn count(&self) -> u64; pub fn max_ns(&self) -> u64;
    pub fn percentile(&self, p: f64) -> Option<u64>;   // µs, None if empty; p in 0..=100
    pub fn snapshot(&self) -> HistSnapshot;        // plain arrays for CSV
    pub fn reset(&self);
}
```
`RtStats` gains `pub tick_lateness: Histogram`, `pub cycle_work: Histogram`, `pub rx_interval: Histogram`; `StatsSnapshot` gains `max_cycle_work_ns` and `max_rx_interval_ns` (`max_tick_lateness_ns` stays). `RtStats` moves behind `Arc` already (unchanged); its size grows to ~48 KiB — fine, one per device.

Recording points in `run_loop`:
- `tick_lateness`: `now − expected` at every timerfd read (already computed for the max).
- `cycle_work`: `send returned − tick wake-up`, per tick.
- `rx_interval`: `now − last_accepted_rx` for every `RxVerdict::Accepted`, skipping the first frame after (re)start.

## 9. Seqlock decision rule

After the loaded 1 ms run: if `input_snapshot_reused + output_publish_deferred < 0.1 % × ticks`, the `Mutex` + `try_lock` image stays and the FOLLOWUP is closed with the numbers; otherwise a seqlock image is Plan 7bis. Recorded in `FOLLOWUPS.md` either way.

## 10. `examples/rt_bringup.rs`

New flags (existing: `--iface --name --ip --rt-priority --cpu --stats-every`):
- `--lock-memory` → `RtOptions.lock_memory = true`.
- `--app-cpus 0-2` → `rt::sched::set_affinity` on the main thread before `Device::new` (the acyclic loop runs on main; the RT thread sets its own affinity later).
- `--duration SECS` → clean stop after that time (same path as SIGTERM: the stop flag handed to `Device::run` is set, the runner is stopped, image `Stale`).
- `--csv PATH` → one line per `stats_every` (`t_s,tx,rx_accepted,rx_dropped,missed_ticks,watchdog_expirations,reused,deferred,lat_max_us,lat_p9999_us,work_max_us,rxint_max_us`); at exit `PATH.hist.csv` with `bin_us,tick_lateness,cycle_work,rx_interval` for all 2048 bins.
- Thresholds: `--max-lateness-us 300`, `--p9999-lateness-us 100`, `--max-rx-interval-us 1500` (defaults = §1).
- Summary at exit on stderr: p50 / p99 / p99.99 / max of the three histograms, counters, "memory locked: yes/no", then `VERDICT: PASS` or `VERDICT: FAIL (<reason list>)`; exit code 0/1. Duration < 60 s → verdict marked `(short run)` but still computed.
- Application loop: sleep 1 ms instead of 10 ms (mirror `QB0→IB0`, echo `QD2/QD6` unchanged).

## 11. Campaign (`bench/campaign.sh`, `bench/load.sh`, `bench/README.md`)

`bench/load.sh [SECS]`: `taskset -c 0-2 stress-ng --cpu 3 --vm 1 --vm-bytes 512M --timeout ${SECS:-600}s`.

`bench/campaign.sh [DURATION=600]` (run from `~/bench`, needs the capabilities already set on `rt_bringup`, `tcpdump`, `cyclictest`):
0. Preconditions: `/sys/kernel/realtime == 1`, CPU 3 isolated, `edge-rt-tune.sh` state printed into `env.txt`; refuse otherwise.
1. `cyclictest` idle → `cyclictest-idle.txt`.
2. `cyclictest` + `load.sh` → `cyclictest-load.txt`.
3. `rt_bringup --duration D --csv rt-idle.csv --lock-memory --rt-priority 80 --cpu 3 --app-cpus 0-2` → `rt-idle.log`.
4. Same + `load.sh` + `taskset -c 0-2 tcpdump -i eno2 -B 65536 -w rt-load.pcapng` → `rt-load.log`, `rt-load.csv`.
5. Summary table (`summary.txt`) from the four outputs; exit code = AND of the two `rt_bringup` verdicts.
Directory: `~/bench/logs/plan7-<YYYYmmdd-HHMMSS>/`. Steps 3-4 assume TIA is already at 1 ms; a 32 ms control run (`--duration 120`) is done by hand first as a non-regression check.

Post-processing (local, `tshark.exe` via WSL, documented in `bench/README.md`): `frame.time_delta_displayed` on `pn_rt.frame_id == 0x8000` and `== 0x8001`, p99.99/max, into the report.

TIA: update time 32 ms → **1 ms** (watchdog factor 3), download between the control run and step 3.

## 12. Report and docs

- `docs/bench-pnet-device.md` §6e "HIL — 1 ms on PREEMPT_RT": edge configuration (script output), cyclictest table, `rt_bringup` idle/load table (counters + percentiles), pcap inter-arrival percentiles, verdict per threshold, TIA diagnostic buffer, watch table, lessons.
- `README.md`: status line with the loaded-run numbers; `bench/` mentioned.
- `FOLLOWUPS.md`: Plan 7 section (PACKET_MMAP, busy-poll, seqlock per §9, PACKET_AUXDATA, any igb knob that turned out unsupported).
- `bench/README.md`: GRUB cmdline, install of the unit, capabilities (`cap_net_raw,cap_net_admin,cap_sys_nice+eip` on `rt_bringup`; `cap_net_raw,cap_net_admin+eip` on `tcpdump`; `cap_sys_nice` also for `cyclictest` or run it with sudo), campaign usage, tshark commands.

## 13. Errors and edge cases

- `mlockall` refused (`RLIMIT_MEMLOCK`, missing `CAP_IPC_LOCK`) → `SchedWarning`, run continues, summary says "memory locked: no". `bench/README.md` documents `ulimit -l unlimited` / `LimitMEMLOCK` and `cap_ipc_lock`.
- `SO_ATTACH_FILTER` refused → `RtError::Io` from `spawn` (fatal).
- `recv_into` with a frame longer than the buffer → `TransportError::FrameTooLong`, counted by the runner as `rx_invalid`, loop continues.
- `edge-rt-tune.sh`: ethtool steps warn-and-continue; everything else fails the script; the systemd unit therefore fails visibly (`systemctl status`).
- `campaign.sh` refuses to run without PREEMPT_RT + isolation; a `RtWatchdog` abort during a run is a FAIL (the counter is in the CSV).
- Histogram overflow bin (≥ 2047 µs) counts toward `max` via `max_ns`, so a single 5 ms hiccup is never hidden by binning.

## 14. Tests

- `eth::bpf`: generated programs compared to fixed `SockFilter` arrays; a ~40-line test interpreter (`ld_abs_h`, `jeq`, `jge`, `jgt`, `ret`) runs `rt_filter()` and `acyclic_filter()` over the Plan 4 goldens (`rtc_cpu_8001` accepted by RT / rejected by acyclic), a DCP Identify golden (the reverse), the same frames with an inserted 802.1Q tag, and an IPv4 frame (rejected by both).
- `eth::transport`: `MockTransport::recv_into` round-trip; short buffer → `BufferTooSmall`; `recv` default equals `recv_into` bytes.
- `rt::hist`: empty → `percentile == None`; dirac; uniform 0..1000 µs → p50 ≈ 500, p99.99 ≈ 999; overflow bin; `max_ns` beyond the last bin.
- `rt::sched`: `set_affinity` mask construction (unit, no syscall); `set_fifo` / `lock_memory` `#[ignore]` (need capabilities; run by hand on the edge).
- `rt::runner`: existing mock-transport tests updated for `recv_into`; `cycle_work.count() == tx` after N ticks.
- `tests/rt_replay.rs`: unchanged, must stay green.
- `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, musl build (`x86_64-unknown-linux-musl`) green.

## 15. Dependencies

None new. `libc` for `sock_filter`/`sock_fprog` constants (`SO_ATTACH_FILTER`, `MCL_CURRENT`, `MCL_FUTURE`, `SYS_*`), already a dependency. `bench/` scripts need `ethtool`, `chrt` (util-linux), `rt-tests`, `stress-ng`, `tcpdump` on the edge — all installed.

## 16. Roles

- Me: code, scripts, musl build, copy to `~/bench/`, campaign launch and analysis, docs.
- User (sudo / TIA): GRUB cmdline + `update-grub` + reboot, `setcap` after each binary copy, unit installation, TIA update time 1 ms + download, watch table and diagnostic buffer screenshots, CPU STOP/RUN when asked.

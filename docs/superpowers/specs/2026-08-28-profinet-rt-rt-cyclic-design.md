# Spec — Plan 4: `rt` — cyclic RTC1 exchange (PPM/CPM, I/O image, RT thread)

Date: 2026-08-28. Status: design validated in brainstorm, awaiting user review.
Parent: [`2026-06-25-profinet-rt-device-design.md`](2026-06-25-profinet-rt-device-design.md) §5.1 (`rt`), §5.2 (thread model), §7.
Builds on Plan 3 ([`2026-08-27-profinet-rt-cm-ar-design.md`](2026-08-27-profinet-rt-cm-ar-design.md)): the AR reaches DATA; `ArParams` carries both IOCRs (frame IDs, offsets, ratio, watchdog).

## 1. Goal

Once the AR is in DATA, exchange **cyclic RTC1 frames** with the S7-1500: produce our input frame
every cycle (PPM), consume the controller's output frame (CPM), maintain IOPS/IOCS, data status,
cycle counter and the consumer watchdog, and expose a **per-cycle-consistent I/O image** to the
application from a dedicated RT thread.

**Ground truth**: `captures/rt-cyclic-2026-08-27-164031.pcapng` (p-net ↔ CPU 1515-2 PN FW V2.9.4,
32 ms), `io-bits`/`q-bits`/`echo` captures, and `docs/bench-pnet-device.md` §6b (C-SDU layout).

**Success criteria**
1. Codecs and engine byte-exact against the capture (our produced frames equal p-net's except cycle
   counter and data status `0x36` → `0x35`).
2. HIL: `rt_bringup` on the edge, TIA project unchanged (32 ms): device **green in TIA for ≥ 60 s**,
   `%IB0 == %QB0`, `%IB1 == %QB1`, `%ID2/%ID6 == %QD2/%QD6` (true echo), no RPC Read `0xfbff` probe,
   `missed_ticks == 0`.
3. The structure is the final one: the 1 ms target (Plan 7) changes kernel/tuning only.

## 2. Scope

In:
- `rt::frame` — RTC1 frame codec (VLAN prio 6, FrameID, C-SDU, cycle counter, data status, transfer status).
- `rt::layout` — C-SDU serialization plan derived from `ArParams` + `DeviceModel`.
- `rt::engine` — pure PPM/CPM engine with IOPS/IOCS, cycle counter, consumer watchdog, stats.
- `rt::image` — shared `IoImage` (inputs app→CPU, outputs CPU→app, validity).
- `rt::runner` — dedicated RT thread (own AF_PACKET socket, timerfd, optional `SCHED_FIFO`/CPU pin), eventfd to the acyclic loop.
- `device` — start/stop the runner on `Data`/`Idle`, watchdog abort into `Ar`, `Device::image()`.
- `eth` — drop `PACKET_OUTGOING` frames in `AfPacketTransport::recv`.
- `examples/rt_bringup.rs` — HIL binary with a mirror/echo application loop.
- Docs: bench §6d (HIL), README, FOLLOWUPS.

Out (tracked in FOLLOWUPS at close-out):
- 1 ms / determinism measurement, `isolcpus`, IRQ affinity, `mlockall`, per-socket BPF filters, lock-free seqlock (all Plan 7 — decided by the `input_snapshot_reused` and `missed_ticks` counters).
- Alarm channel (ERR-RTA on stop, diagnosis, ProblemIndicator use) — Plan 5.
- Typed I/O API (`read_real(Slot(1), 0)`), GSDML/config — Plan 6.
- `PACKET_AUXDATA` (reading the RX VLAN priority) — not needed to operate; Plan 7 if ever.
- Multiple ARs, shared devices, IRT.

## 3. Decisions (locked in brainstorm)

| Subject | Decision | Why |
|---|---|---|
| Cycle target | Generic (send clock × ratio from the AR); HIL at 32 ms; 1 ms in Plan 7 | No bench change; determinism is a separate plan |
| Execution | Dedicated RT thread now (spec §5.2), timerfd-driven | Final structure; Plan 7 only tunes the kernel |
| Application API | Raw per-submodule byte image + `data` codecs | Typed accessors come with `config` (Plan 6) |
| Image sharing | `Mutex` + private RT snapshot, `try_lock` on the RT side | No `unsafe`, never blocks the RT thread; seqlock only if counters show contention |
| Sockets | Second AF_PACKET socket owned by the RT thread; both drop `PACKET_OUTGOING` | Isolates the RT path; BPF filters deferred |
| Missed ticks | One frame per wake-up, cycle counter advanced by `expirations × step` | No bursts; the CPU sees a jump, never duplicates |
| Watchdog policy | Freeze data + `Validity::Stale` + `Ar` abort (`RtWatchdog`) | Spec §7; the CPU re-establishes the AR |
| CPU in STOP (`0x25`) | Data still copied, `Validity::Stopped`, IOCS GOOD | AR stays alive (observed on the bench) |
| Data status we emit | `0x35` (Primary, Valid, Run, OK) | What the CPU emits; p-net's `0x36` documented only |
| Dependencies | none new (`libc` timerfd/eventfd/sched) | Keep the crate minimal |

## 4. Architecture

```
crates/profinet-rt/src/
  rt/mod.rs        RtError, re-exports
  rt/frame.rs      RtFrame parse/write (buffer-based), DataStatus, TCI_RT = 0xC000
  rt/layout.rs     Layout { input_cr, output_cr } from ArParams + DeviceModel
  rt/engine.rs     RtEngine: on_tick / on_frame / check_watchdog, RtStats
  rt/image.rs      IoImage (inputs/outputs, Validity), ImageError
  rt/runner.rs     RtRunner::spawn -> RtHandle (thread, timerfd, eventfd, stop/join)
  device/mod.rs    + RtOptions in DeviceSetup, runner lifecycle, eventfd in the poll set, image()
  eth/afpacket.rs  + drop PACKET_OUTGOING
examples/rt_bringup.rs
tests/rt_replay.rs
```
Rules: `frame`, `layout`, `engine`, `image` own no socket, no clock, no thread. The RT loop
allocates nothing after `spawn`. Logging never happens inside the RT loop (codes through the
eventfd; the acyclic side logs).

## 5. `rt::frame` — RTC1 codec

Wire layout (both directions): `dst(6) src(6) [81 00 TCI(2)] 88 92 | FrameID(2) | C-SDU(≥ 40) |
CycleCounter(2) | DataStatus(1) | TransferStatus(1)`.
- TX: always VLAN-tagged, `TCI = 0xC000` (priority 6, VID 0). RX: tagged or untagged accepted
  (the kernel strips the tag with RX offload).
- FrameID comes from the AR (`IocrParams.frame_id`; bench `0x8000` device→CPU, `0x8001` CPU→device).
- C-SDU length = `IocrParams.data_length`, zero-padded to ≥ 40.
- CycleCounter: u16, unit 31.25 µs, `+= send_clock_factor × reduction_ratio` per frame (1024 on the bench).
- DataStatus bits: 0 State (1 = Primary), 1 Redundancy, 2 DataValid, 4 ProviderState (1 = Run),
  5 StationProblemIndicator (1 = OK). Emitted `0x35`; received bits exposed (`0x25` = CPU STOP).
- TransferStatus ≠ 0 → frame dropped, counted.

API: `RtFrame { frame_id, cycle_counter, data_status: DataStatus, transfer_status, csdu: &[u8] }`;
`RtFrame::parse(&[u8]) -> Result<RtFrame, FrameError>` (checks ethertype, min length
FrameID + 40 + 4); `RtFrame::write(&self, out: &mut [u8], dst, src) -> Result<usize, FrameError>`
into a caller-provided buffer (no `Vec`); `DataStatus(u8)` with named accessors and
`DataStatus::RUN_PRIMARY_VALID_OK = 0x35`.

Goldens: `0x8000`/`0x8001` frames from `rt-cyclic`; byte-exact `write` against a CPU frame with
swapped MACs; parse of every frame of the capture.

## 6. `rt::layout` — C-SDU plan

Built once on entering `Data` from `ArParams` + `DeviceModel`:
```
CrLayout { frame_id, data_length, cycle_step, watchdog: Duration,
           objects: Vec<IoObject { slot, subslot, data_off, data_len, iops_off }>,
           iocs: Vec<CsObject { slot, subslot, iocs_off }> }
Layout { input_cr /* we produce */, output_cr /* we consume */ }
```
- `cycle_step = send_clock_factor × reduction_ratio`; `watchdog = data_hold_factor × cycle_step × 31.25 µs`.
- Input CR: each IODataObject → `data_off = frame_offset`, `data_len = model.input_len`,
  `iops_off = frame_offset + data_len`; each IOCS → one byte at `frame_offset` (our consumer status
  of the received outputs).
- Output CR: IODataObject → CPU output data + CPU IOPS at `frame_offset + output_len`; IOCS → the
  CPU's consumer status of our inputs.
- Zero-length submodules (DAP) carry only IOxS. IOxS values: `0x80` GOOD, `0x00` BAD.
- Flat ordered tables (no maps); `debug_assert!` on overlap/bounds (already validated by `cm`).
- Exposes the ordered submodule list `(slot, subslot, input_len, output_len)` = the `IoImage` cells.

Tests: from the golden `connect_req` + `pnet_sample`: offsets equal §6b's table (DAP IOPS 0/1/2;
DI data 3, IOPS 4; DO IOCS 5; DIO 6/7/8; Echo 9-16/17/18; mirrored on the output CR),
`cycle_step == 1024`, `watchdog == 96 ms`.

## 7. `rt::engine` — PPM/CPM + watchdog (pure)

`RtEngine::new(layout, our_mac, cpu_mac)` preallocates the TX frame and the RX C-SDU buffers.
- **PPM** `on_tick(&mut self, now, inputs: &[u8]) -> &[u8]`: writes the input data at `data_off`,
  IOPS GOOD per object, IOCS per output submodule = GOOD if its last received IOPS was GOOD else
  BAD, `cycle_counter += cycle_step × expirations`, data status `0x35` (or with ProblemIndicator
  cleared when armed — reserved for Plan 5), transfer status 0; returns the ready frame slice.
- **CPM** `on_frame(&mut self, frame: &[u8], now) -> RxVerdict`: `Ignored` if the FrameID is not
  `output_cr.frame_id` or the source MAC is not the CPU's; `Dropped(reason)` on transfer status ≠ 0
  or short C-SDU; `DataValid == 0` → accepted for the watchdog, data not copied; else copy the
  C-SDU, `last_rx = now`, cycle counter tracked (non-increasing modulo 2¹⁶ → `reordered++`,
  still accepted); returns `Accepted { provider_run, primary }`.
- **Watchdog** `check_watchdog(&mut self, now) -> WatchdogVerdict`: armed at the first accepted
  frame; `now − last_rx > output_cr.watchdog` → `Expired` once, then `Stopped`.
- Per-submodule view for the image: output objects `(data, cpu_iops_good)`, input objects
  `cpu_iocs_good`, global `provider_run`/`primary`.
- `RtStats` (relaxed atomics): `tx`, `rx_accepted`, `rx_ignored`, `rx_dropped`, `rx_invalid`,
  `reordered`, `watchdog_expirations`, `missed_ticks`, `input_snapshot_reused`,
  `output_publish_deferred`, `max_tick_lateness_ns`.

Tests: replay of every `0x8001` of `rt-cyclic` → `Accepted`, IOPS GOOD, data as in §6b; produced
frames equal p-net's `0x8000` except cycle counter and data status; watchdog (3 frames then
97 ms silence → `Expired`); `0x25` → `provider_run == false`; transfer status ≠ 0 → `Dropped`;
duplicate/late counter → `reordered`.

## 8. `rt::image` — shared I/O image

`IoImage` shared by `Arc` between the RT thread and the application:
- `inputs` (app → CPU) and `outputs` (CPU → app): one `Mutex<Buffer>` each, sized by the layout,
  plus per-cell metadata; `Validity { provider_run, primary, watchdog: Armed | Ok | Expired,
  last_rx_age }`, `cycle: u64`.
- RT side (never blocks): per tick `try_lock(inputs)` → copy into a private snapshot, else reuse
  the previous snapshot (`input_snapshot_reused++`); after an accepted CPM `try_lock(outputs)` →
  publish, else keep in the private RX buffer and retry next tick (`output_publish_deferred++`).
- Application side: `lock()` (waits at most one 40-byte copy). API: `write_inputs(slot, subslot,
  &[u8]) -> Result<(), ImageError>`, `read_outputs(slot, subslot, |bytes, validity| ..)`,
  `snapshot_outputs(&mut [u8]) -> Validity`, `validity()`, `cells() -> &[Cell]`.
- Consistency per cycle: a read sees one whole frame. No `unsafe`, no allocation after
  construction. The library never overwrites the last values on staleness.
- `Validity` states: `Fresh` (last CPM within the watchdog, IOPS GOOD, Run), `Stale` (watchdog
  expired, data frozen), `Stopped` (ProviderState = Stop), `NoData` (never received).
- Encoding/decoding is the application's job with `data::{decode_f32, get_bit, …}` (typed API = Plan 6).

Tests: app write → RT snapshot → produced C-SDU; accepted CPM → `read_outputs` returns it with
`Fresh`; simulated contention (lock held during a tick) → snapshot reused + counter; watchdog →
`Stale` with data preserved.

## 9. `rt::runner` — RT thread, and `device` integration

`RtRunner::spawn(cfg) -> Result<RtHandle, RtError>`, `cfg = { iface, our_mac, cpu_mac, layout,
image: Arc<IoImage>, stats: Arc<RtStats>, cpu_pin: Option<usize>, rt_priority: Option<u8> }`.

Thread body:
1. Opens its own `AfPacketTransport` on `iface`. `eth` change: `recv` drops `PACKET_OUTGOING`
   frames (our own transmissions are otherwise delivered to both sockets).
2. `sched_setscheduler(SCHED_FIFO, rt_priority)` when requested: `EPERM` → `RtError::Sched` reported
   through the handle as a warning, thread continues at normal priority (the 32 ms HIL passes
   without it). `cpu_pin` → `sched_setaffinity`, same policy. `mlockall`/`isolcpus` = Plan 7.
3. `timerfd` `CLOCK_MONOTONIC`, period = `cycle_step × 31.25 µs`.
4. Loop: `poll([timerfd, socket])` without timeout. Timer readable → `read` expirations
   (`> 1` → `missed_ticks`) → `check_watchdog` → `on_tick(snapshot)` → `send`. Socket readable →
   non-blocking `recv` until empty → `on_frame` → publish. `stop` flag → clean exit (no final
   frame; a device-initiated ERR-RTA is Plan 5).
5. No allocation and no logging inside the loop: buffers preallocated; events go through an
   `eventfd` in `RtHandle` (`WatchdogExpired`, `SocketError`, `SchedWarning`).

`RtHandle { stop(), join(timeout), event_fd(), take_event() -> Option<RtEvent>, stats() }`.

`device`:
- `DeviceSetup` gains `rt: RtOptions { iface, cpu_pin, rt_priority }`; `Device::image() -> Arc<IoImage>`
  (created at `new` with empty cells; cells rebuilt on each `Data` — the application keeps the same `Arc`).
- On `Notify { Data }`: build `Layout` from `cm.context().params` + `setup.model`, rebuild the image
  cells, `RtRunner::spawn`. On `Notify { Idle, .. }`: `stop()` + bounded `join`.
- `Device::step` adds the runner's eventfd to `wait_any_readable`; on `WatchdogExpired` →
  `cm.abort(AbortReason::RtWatchdog)` (new variant) → `Ar` goes `Idle`, runner stopped; the CPU
  re-establishes the AR and a fresh runner starts.
- Two sockets on one interface: both receive every inbound `0x8892` frame; the acyclic loop
  drops RTC1 (`FrameId::from_u16` → `None`), the RT loop drops DCP/alarms (`Ignored`, one compare).

## 10. Errors and edge cases

- `RtError { Frame(FrameError), Layout(LayoutError), Io(io::Error), Sched(io::Error), Stopped }`,
  `ImageError { UnknownSubmodule { slot, subslot }, LengthMismatch { expected, got }, Direction }`
  (`thiserror`). Every dropped frame increments a named counter; watchdog expirations are logged
  by the acyclic side at `warn!`.
- Missed ticks: one frame per wake-up, cycle counter advanced by `expirations × cycle_step`.
- Duplicate/late CPM: accepted, `reordered++`, latest by arrival time wins.
- CPU STOP (`0x25`): `Stopped`, data still copied, IOCS GOOD, AR alive.
- Watchdog expiry: freeze + `Stale` + AR abort; the next `Data` starts a fresh runner.
- Controller takeover while in `Data`: `Notify Idle` stops the runner, `Notify Data` starts a new
  one with the new layout.
- No application activity: frames still sent (zeros, IOPS GOOD).
- `data_length` larger than the objects: zero padding; smaller: already rejected at Connect.

## 11. Tests

1. Codecs (`frame`, `layout`) byte-exact vs `rt-cyclic` and the golden `connect_req`.
2. Engine: capture replay, byte comparison of produced frames (modulo counter/status), watchdog,
   transfer status, `0x25`, duplicates.
3. Image: consistency, contention, validity.
4. `tests/rt_replay.rs`: `Device` with mocks to `Data`, engine fed with the capture, image read by
   a fake application — no thread. Runner tested separately with a real timerfd on a
   `MockTransport` (5 ticks; timerfd needs no capability).
5. HIL (manual, documented): §1 criteria; `rt_bringup` application loop mirrors `QB0→IB0`,
   `QB1→IB1` and echoes `QB2..9→IB2..9` every 10 ms.

Exit criteria: suite green, `clippy -D warnings`, `fmt`; HIL passed and recorded in
`docs/bench-pnet-device.md` §6d; README (`rt` ✅ at 32 ms, < 2 ms = Plan 7); FOLLOWUPS updated
(per-socket BPF, seqlock, `PACKET_AUXDATA`/RX priority, `mlockall`, ERR-RTA on stop).

## 12. Dependencies

None new: timerfd, eventfd, `sched_setscheduler`, `sched_setaffinity` via `libc`.

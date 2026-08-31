# PROFINET-RT Plan 4 — `rt` Cyclic RTC1 Exchange Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Once the AR is in DATA, exchange cyclic RTC1 frames with the S7-1500 from a dedicated RT thread — produce our input frame every cycle, consume the controller's output frame, maintain IOPS/IOCS, cycle counter, data status and the consumer watchdog — and expose a per-cycle-consistent I/O image to the application; validated byte-exact against the bench capture and live on the edge at 32 ms (device green in TIA, data round trip).

**Architecture:** Four pure units (`rt::frame` codec, `rt::layout` from the AR, `rt::engine` PPM/CPM/watchdog, `rt::image` shared buffers) plus one thread unit (`rt::runner`: own AF_PACKET socket, timerfd clock, eventfd back-channel, optional `SCHED_FIFO`/CPU pin). `device` starts/stops the runner on `Data`/`Idle` and turns a watchdog expiry into an AR abort. No allocation and no logging inside the RT loop; no `unsafe` outside `libc` calls with a Safety comment.

**Tech Stack:** Rust stable (1.96), `libc` (timerfd, eventfd, sched, recvfrom), `nix` (poll), std `Mutex`/`Arc`/atomics. No new dependencies.

**Spec:** `docs/design/2026-08-28-profinet-rt-rt-cyclic-design.md` — read it first. Bench facts: `docs/bench-pnet-device.md` §6b (C-SDU layout, bit order, data status), `docs/cm-golden-frames.md`.

## Global Constraints

- No new dependencies. `unsafe` only around `libc` calls, each with a one-line `// Safety:` comment.
- RTC1 frames: TX always VLAN-tagged `81 00 c0 00` (priority 6, VID 0); RX tagged or untagged. FrameID and offsets come from the AR (`ArParams`), never hardcoded outside tests. C-SDU zero-padded to ≥ 40 bytes. Cycle counter unit 31.25 µs, step `send_clock_factor × reduction_ratio`. We emit data status `0x35`.
- The RT loop allocates nothing after `spawn`, never blocks on a lock (`try_lock` only), never logs (events go through the eventfd).
- Byte-exact goldens: `crates/profinet-rt/testdata/rt/*.hex` (Task 1) from `captures/rt-cyclic-2026-08-27-164031.pcapng` and `captures/echo-2026-08-27-165307.pcapng`.
- rustfmt `max_width = 100`; `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all` must pass. All cargo commands: `. "$HOME/.cargo/env" && cargo ...`.
- Existing 150 unit + 3 integration tests + doctest are sacred; `Device` tests with mocks must keep passing with `rt: None`.
- Errors typed (`thiserror`), never silent: every dropped frame increments a named `RtStats` counter.
- Branch: `feat/rt-cyclic` from `main` (HEAD `b7f97cd`), linked worktree `.worktrees/feat-rt-cyclic`; implementers commit, the controller pushes. Project language: English.

---

## File Structure

Create:
- `crates/profinet-rt/testdata/rt/{rtc_dev_8000,rtc_cpu_8001,echo_cpu_8001,echo_dev_8000}.hex` (Task 1)
- `crates/profinet-rt/src/rt/{mod.rs,frame.rs,layout.rs,engine.rs,image.rs,runner.rs}` (Tasks 2-6)
- `crates/profinet-rt/examples/rt_bringup.rs`, `crates/profinet-rt/tests/rt_replay.rs` (Task 8)

Modify:
- `crates/profinet-rt/src/testutil.rs`, `crates/profinet-rt/tests/common/mod.rs` — `golden_rt(name)` (Task 1)
- `crates/profinet-rt/src/lib.rs` — `pub mod rt;` (Task 2)
- `crates/profinet-rt/src/eth/afpacket.rs` — drop `PACKET_OUTGOING` (Task 6)
- `crates/profinet-rt/src/cm/ar.rs` — `AbortReason::RtWatchdog` (Task 7)
- `crates/profinet-rt/src/device/mod.rs` — `RtOptions`, runner lifecycle, eventfd in the poll set, `image()`, `rt_stats()` (Task 7)
- `README.md`, `FOLLOWUPS.md`, `docs/bench-pnet-device.md` §6d (Task 9)

Golden inventory (full Ethernet frames, all VLAN-tagged: FrameID at byte 18, C-SDU at 20..60, cycle counter at 60..62, data status 62, transfer status 63):

| File | Source | Content |
|---|---|---|
| `rtc_dev_8000.hex` | rt-cyclic frame 1 | p-net → CPU, data status `0x35`, cycle counter `0x1c00`, DI `0x2c`, DIO `0x2d` |
| `rtc_cpu_8001.hex` | rt-cyclic frame 2 | CPU → device, `0x35`, cc `0xb800`, all outputs zero |
| `echo_cpu_8001.hex` | echo frame 2 | CPU → device: `QB0 = 0x01`, Echo `12 34 56 78 3f c0 00 00`, cc `0xe400` |
| `echo_dev_8000.hex` | echo frame 1 | p-net → CPU with data status `0x36` (documented p-net quirk) |

C-SDU byte map (both directions, from §6b): `[0..3]` DAP IOxS ×3 · slot 1 DI: data `[3]`, IOPS `[4]` (input CR) / IOCS `[3]` (output CR) · slot 2 DO: IOCS `[5]` (input CR) / data `[4]`, IOPS `[5]` (output CR) · slot 3 DIO: input CR data `[6]` IOPS `[7]` IOCS `[8]`, output CR IOCS `[6]` data `[7]` IOPS `[8]` · slot 4 Echo: input CR data `[9..17]` IOPS `[17]` IOCS `[18]`, output CR IOCS `[9]` data `[10..18]` IOPS `[18]` · padding to 40.

---

### Task 1: Pin the RTC1 golden frames + `golden_rt` loader

**Files:**
- Create: `crates/profinet-rt/testdata/rt/*.hex` (4 files)
- Modify: `crates/profinet-rt/src/testutil.rs`, `crates/profinet-rt/tests/common/mod.rs`

**Interfaces:**
- Produces: `crate::testutil::golden_rt(name: &str) -> Vec<u8>` (reads `testdata/rt/<name>.hex`) and the same in `tests/common/mod.rs`; constants `RT_FRAMEID_OFF = 18`, `RT_CSDU_OFF = 20`, `RT_APDU_OFF = 60` (cycle counter), all in both loaders.

- [ ] **Step 1: Create the branch and worktree** (controller does this; implementer skips)

- [ ] **Step 2: Write the hex files**

`rtc_dev_8000.hex`:
```
# rt-cyclic-2026-08-27-164031.pcapng frame 1: p-net -> CPU, FrameID 0x8000, ds 0x35, cc 0x1c00 (64 bytes)
ec 1c 5d 61 e7 3f 8c f3 19 cd 19 f8 81 00 c0 00
88 92 80 00 80 80 80 2c 80 80 2d 80 80 00 00 00
00 00 00 00 00 80 80 00 00 00 00 00 00 00 00 00
00 00 00 00 00 00 00 00 00 00 00 00 1c 00 35 00
```
`rtc_cpu_8001.hex`:
```
# rt-cyclic-2026-08-27-164031.pcapng frame 2: CPU -> device, FrameID 0x8001, ds 0x35, cc 0xb800 (64 bytes)
8c f3 19 cd 19 f8 ec 1c 5d 61 e7 3f 81 00 c0 00
88 92 80 01 80 80 80 80 00 80 80 00 80 80 00 00
00 00 00 00 00 00 80 00 00 00 00 00 00 00 00 00
00 00 00 00 00 00 00 00 00 00 00 00 b8 00 35 00
```
`echo_cpu_8001.hex`:
```
# echo-2026-08-27-165307.pcapng frame 2: CPU -> device, QB0=01, Echo 12345678 / 1.5f, cc 0xe400 (64 bytes)
8c f3 19 cd 19 f8 ec 1c 5d 61 e7 3f 81 00 c0 00
88 92 80 01 80 80 80 80 01 80 80 00 80 80 12 34
56 78 3f c0 00 00 80 00 00 00 00 00 00 00 00 00
00 00 00 00 00 00 00 00 00 00 00 00 e4 00 35 00
```
`echo_dev_8000.hex`:
```
# echo-2026-08-27-165307.pcapng frame 1: p-net -> CPU, ds 0x36 (Backup+Redundancy quirk), cc 0xec00 (64 bytes)
ec 1c 5d 61 e7 3f 8c f3 19 cd 19 f8 81 00 c0 00
88 92 80 00 80 80 80 a4 80 80 a5 80 80 12 b4 56
78 7f 80 00 00 80 80 00 00 00 00 00 00 00 00 00
00 00 00 00 00 00 00 00 00 00 00 00 ec 00 36 00
```

- [ ] **Step 3: Loaders**

In `crates/profinet-rt/src/testutil.rs` add:
```rust
/// Offsets inside a VLAN-tagged RTC1 golden frame.
pub const RT_FRAMEID_OFF: usize = 18;
pub const RT_CSDU_OFF: usize = 20;
pub const RT_APDU_OFF: usize = 60;

/// Load `testdata/rt/<name>.hex` relative to the crate root.
pub fn golden_rt(name: &str) -> Vec<u8> {
    let path = format!("{}/testdata/rt/{name}.hex", env!("CARGO_MANIFEST_DIR"));
    parse_hex(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")))
}
```
and a test:
```rust
    #[test]
    fn rt_goldens_are_64_byte_tagged_rtc1_frames() {
        for name in ["rtc_dev_8000", "rtc_cpu_8001", "echo_cpu_8001", "echo_dev_8000"] {
            let f = golden_rt(name);
            assert_eq!(f.len(), 64, "{name}");
            assert_eq!(&f[12..16], &[0x81, 0x00, 0xc0, 0x00], "{name} VLAN tag");
            assert_eq!(&f[16..18], &[0x88, 0x92], "{name} ethertype");
        }
    }
```
Mirror `golden_rt` and the three constants in `crates/profinet-rt/tests/common/mod.rs`.

- [ ] **Step 4: Run** `. "$HOME/.cargo/env" && cargo test -p profinet-rt testutil` → 3 passed.
- [ ] **Step 5: Commit** `git add crates/profinet-rt/testdata/rt crates/profinet-rt/src/testutil.rs crates/profinet-rt/tests/common/mod.rs && git commit -m "test(rt): pin RTC1 golden frames from the 2026-08-27 bench + loader"`

---

### Task 2: `rt::frame` — RTC1 codec

**Files:**
- Create: `crates/profinet-rt/src/rt/mod.rs`, `crates/profinet-rt/src/rt/frame.rs`
- Modify: `crates/profinet-rt/src/lib.rs` (`pub mod rt;` after `pub mod rpc;`)

**Interfaces:**
- Produces `rt::RtError { Frame(#[from] FrameError), Layout(#[from] LayoutError), Io(#[from] std::io::Error), Sched(std::io::Error), Stopped }` (`thiserror`, `Debug`); `rt::frame::FrameError { TooShort { need, have }, NotProfinet, BufferTooSmall { need, have } }` (`thiserror`, `Debug, PartialEq, Eq`). `LayoutError` is defined in Task 3 — declare `pub mod layout;` there; in this task define `RtError` without the `Layout` variant and add it in Task 3.
- Produces `pub const TCI_RT: u16 = 0xC000;`, `pub const CSDU_MIN: usize = 40;`, `pub const APDU_LEN: usize = 4;`, `pub const CYCLE_UNIT: Duration = Duration::from_nanos(31_250);`.
- Produces `DataStatus(pub u8)` (`Copy, PartialEq, Eq, Debug`) with `const RUN_PRIMARY_VALID_OK: DataStatus = DataStatus(0x35)`, `primary()` (bit 0), `redundancy()` (bit 1), `data_valid()` (bit 2), `provider_run()` (bit 4), `station_ok()` (bit 5).
- Produces `RtFrame<'a> { frame_id: u16, csdu: &'a [u8], cycle_counter: u16, data_status: DataStatus, transfer_status: u8 }` with `RtFrame::parse(frame: &'a [u8]) -> Result<(EthHeader, RtFrame<'a>), FrameError>` (uses `crate::eth::EthHeader::parse`; `NotProfinet` if ethertype ≠ 0x8892; `TooShort` if fewer than `2 + CSDU_MIN + APDU_LEN` bytes after the header; `csdu` = everything between FrameID and the last 4 bytes) and `RtFrame::write(&self, out: &mut [u8], dst: MacAddr, src: MacAddr) -> Result<usize, FrameError>` (writes `EthHeader { dst, src, vlan: Some(TCI_RT), ethertype: 0x8892 }`, FrameID, C-SDU zero-padded to `max(csdu.len(), CSDU_MIN)`, cycle counter BE, data status, transfer status; returns the length; `BufferTooSmall` if `out` is shorter).
- Produces `pub fn frame_len(csdu_len: usize) -> usize` = `18 + 2 + max(csdu_len, 40) + 4`.

- [ ] **Step 1: Failing tests** (`frame.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::eth::MacAddr;
    use crate::testutil::{golden_rt, RT_CSDU_OFF};

    const CPU: MacAddr = MacAddr([0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f]);
    const DEV: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);

    #[test]
    fn parse_cpu_frame() {
        let f = golden_rt("rtc_cpu_8001");
        let (eth, rt) = RtFrame::parse(&f).unwrap();
        assert_eq!(eth.src, CPU);
        assert_eq!(eth.vlan, Some(TCI_RT));
        assert_eq!(rt.frame_id, 0x8001);
        assert_eq!(rt.csdu.len(), 40);
        assert_eq!(rt.csdu, &f[RT_CSDU_OFF..RT_CSDU_OFF + 40]);
        assert_eq!(rt.cycle_counter, 0xb800);
        assert_eq!(rt.data_status, DataStatus::RUN_PRIMARY_VALID_OK);
        assert!(rt.data_status.provider_run() && rt.data_status.primary() && rt.data_status.data_valid());
        assert_eq!(rt.transfer_status, 0);
    }

    #[test]
    fn parse_untagged_frame_too() {
        let f = golden_rt("rtc_cpu_8001");
        let mut untagged = f[..12].to_vec();
        untagged.extend_from_slice(&f[16..]);
        let (eth, rt) = RtFrame::parse(&untagged).unwrap();
        assert_eq!(eth.vlan, None);
        assert_eq!(rt.frame_id, 0x8001);
        assert_eq!(rt.cycle_counter, 0xb800);
    }

    #[test]
    fn write_is_byte_exact_against_cpu_golden() {
        let f = golden_rt("rtc_cpu_8001");
        let (_, rt) = RtFrame::parse(&f).unwrap();
        let mut out = [0u8; 128];
        let n = rt.write(&mut out, DEV, CPU).unwrap();
        assert_eq!(n, 64);
        assert_eq!(&out[..n], &f[..]);
    }

    #[test]
    fn write_pads_short_csdu_to_40() {
        let rt = RtFrame { frame_id: 0x8000, csdu: &[1, 2, 3], cycle_counter: 1024, data_status: DataStatus::RUN_PRIMARY_VALID_OK, transfer_status: 0 };
        let mut out = [0u8; 128];
        let n = rt.write(&mut out, CPU, DEV).unwrap();
        assert_eq!(n, frame_len(3));
        assert_eq!(n, 64);
        assert_eq!(&out[20..23], &[1, 2, 3]);
        assert!(out[23..60].iter().all(|b| *b == 0));
        assert_eq!(&out[60..64], &[0x04, 0x00, 0x35, 0x00]);
    }

    #[test]
    fn data_status_bits() {
        let stop = DataStatus(0x25);
        assert!(!stop.provider_run() && stop.primary() && stop.data_valid() && stop.station_ok());
        let backup = DataStatus(0x36);
        assert!(!backup.primary() && backup.redundancy());
    }

    #[test]
    fn errors() {
        assert_eq!(RtFrame::parse(&golden_rt("rtc_cpu_8001")[..30]).unwrap_err(), FrameError::TooShort { need: 46, have: 30 });
        let mut ip = golden_rt("rtc_cpu_8001");
        ip[16] = 0x08; ip[17] = 0x00;
        assert_eq!(RtFrame::parse(&ip).unwrap_err(), FrameError::NotProfinet);
        let rt = RtFrame { frame_id: 1, csdu: &[0; 40], cycle_counter: 0, data_status: DataStatus(0), transfer_status: 0 };
        assert_eq!(rt.write(&mut [0u8; 10], CPU, DEV).unwrap_err(), FrameError::BufferTooSmall { need: 64, have: 10 });
    }
}
```
(`TooShort.need` counts the whole frame: 18 header + 2 + 40 + 4 = 64 for a tagged frame; with a 30-byte input the header parse yields offset 18, so `need = 18 + 46 = 64`? — define `need` as the minimum frame length given the parsed header offset: `off + 2 + CSDU_MIN + APDU_LEN`; for the tagged golden `off = 18` → `need = 64`. Fix the test constant accordingly: `FrameError::TooShort { need: 64, have: 30 }`.)

- [ ] **Step 2: Run, expect compile failure.** `cargo test -p profinet-rt rt::frame`
- [ ] **Step 3: Implement** `rt/mod.rs` (`pub mod frame; pub use frame::{DataStatus, FrameError, RtFrame, TCI_RT, CSDU_MIN, CYCLE_UNIT};` + `RtError`) and `rt/frame.rs` per the interface. `write` uses `EthHeader::write` into a `Vec`? No — `EthHeader::write(&self, out: &mut Vec<u8>)` allocates; write the 18 header bytes manually into the slice (dst, src, `81 00`, TCI BE, `88 92`) to keep the RT path allocation-free.
- [ ] **Step 4: Run tests + clippy + fmt** → 6 pass.
- [ ] **Step 5: Commit** `git commit -m "feat(rt): RTC1 frame codec (VLAN prio 6, C-SDU, APDU status) byte-exact"`

---

### Task 3: `rt::layout` — C-SDU plan from the AR

**Files:**
- Create: `crates/profinet-rt/src/rt/layout.rs`
- Modify: `crates/profinet-rt/src/rt/mod.rs` (`pub mod layout; pub use layout::{Layout, CrLayout, IoObject, CsObject, Cell, LayoutError};` + `RtError::Layout`)

**Interfaces:**
- Consumes `crate::cm::{ArParams, IocrParams, IocrObject, DeviceModel}` (`IocrObject { slot, subslot, frame_offset }`, `IocrParams { frame_id, data_length, send_clock_factor, reduction_ratio, data_hold_factor, io_data: Vec<IocrObject>, iocs: Vec<IocrObject>, .. }`, `DeviceModel::find(slot, subslot) -> Option<&SubmoduleModel { input_len, output_len, .. }>`).
- Produces `IoObject { slot: u16, subslot: u16, data_off: usize, data_len: usize, iops_off: usize }`, `CsObject { slot, subslot, iocs_off: usize }`, `CrLayout { frame_id: u16, data_length: usize, cycle_step: u16, watchdog: Duration, objects: Vec<IoObject>, iocs: Vec<CsObject> }`, `Layout { input_cr: CrLayout, output_cr: CrLayout, cells: Vec<Cell> }`, `Cell { slot, subslot, input_len: usize, output_len: usize, input_off: Option<usize> /* data_off in the input CR */, output_off: Option<usize> /* data_off in the output CR */ }`, `LayoutError { UnknownSubmodule { slot, subslot }, OutOfBounds { slot, subslot, end, data_length }, Overlap { slot, subslot } }`.
- Produces `Layout::from_ar(params: &ArParams, model: &DeviceModel) -> Result<Layout, LayoutError>`; `CrLayout::period(&self) -> Duration` = `cycle_step × CYCLE_UNIT`.
- Rules: input CR objects use `model.input_len`, output CR objects use `model.output_len`; `iops_off = frame_offset + len`; IOCS at `frame_offset`; `cycle_step = send_clock_factor × reduction_ratio` (u16, `checked_mul` else `OutOfBounds`-style error — use `LayoutError::OutOfBounds` with `end = u16::MAX`), `watchdog = data_hold_factor × cycle_step × CYCLE_UNIT`; bounds: every `iops_off + 1` and `iocs_off + 1` ≤ `data_length`; overlap check by marking bytes in a `Vec<bool>` per CR. `cells` = one per model submodule (model order), offsets found from the CR object lists.

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cm::{validate, ConnectReq, DeviceModel};
    use crate::eth::MacAddr;
    use crate::testutil::golden;
    use std::time::Duration;

    const MAC: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);

    fn layout() -> Layout {
        let model = DeviceModel::pnet_sample(MAC);
        let req = ConnectReq::parse(&golden("connect_req")[142..]).unwrap();
        let params = validate(&req, &model).unwrap();
        Layout::from_ar(&params, &model).unwrap()
    }

    #[test]
    fn input_cr_matches_bench_table() {
        let l = layout();
        let cr = &l.input_cr;
        assert_eq!((cr.frame_id, cr.data_length, cr.cycle_step), (0x8000, 40, 1024));
        assert_eq!(cr.watchdog, Duration::from_millis(96));
        assert_eq!(cr.period(), Duration::from_millis(32));
        let o = |slot, subslot| cr.objects.iter().find(|o| o.slot == slot && o.subslot == subslot).unwrap();
        assert_eq!((o(0, 1).data_off, o(0, 1).data_len, o(0, 1).iops_off), (0, 0, 0));
        assert_eq!((o(0, 0x8000).iops_off, o(0, 0x8001).iops_off), (1, 2));
        assert_eq!((o(1, 1).data_off, o(1, 1).data_len, o(1, 1).iops_off), (3, 1, 4));
        assert_eq!((o(3, 1).data_off, o(3, 1).iops_off), (6, 7));
        assert_eq!((o(4, 1).data_off, o(4, 1).data_len, o(4, 1).iops_off), (9, 8, 17));
        let c = |slot| cr.iocs.iter().find(|c| c.slot == slot).unwrap().iocs_off;
        assert_eq!((c(2), c(3), c(4)), (5, 8, 18));
    }

    #[test]
    fn output_cr_matches_bench_table() {
        let l = layout();
        let cr = &l.output_cr;
        assert_eq!(cr.frame_id, 0x8001);
        let o = |slot| cr.objects.iter().find(|o| o.slot == slot).unwrap();
        assert_eq!((o(2).data_off, o(2).data_len, o(2).iops_off), (4, 1, 5));
        assert_eq!((o(3).data_off, o(3).iops_off), (7, 8));
        assert_eq!((o(4).data_off, o(4).data_len, o(4).iops_off), (10, 8, 18));
        let c = |slot, subslot| cr.iocs.iter().find(|c| c.slot == slot && c.subslot == subslot).unwrap().iocs_off;
        assert_eq!((c(0, 1), c(0, 0x8000), c(0, 0x8001), c(1, 1), c(3, 1), c(4, 1)), (0, 1, 2, 3, 6, 9));
    }

    #[test]
    fn cells_follow_the_model() {
        let l = layout();
        assert_eq!(l.cells.len(), 7);
        let echo = l.cells.iter().find(|c| c.slot == 4).unwrap();
        assert_eq!((echo.input_len, echo.output_len, echo.input_off, echo.output_off), (8, 8, Some(9), Some(10)));
        let di = l.cells.iter().find(|c| c.slot == 1).unwrap();
        assert_eq!((di.input_off, di.output_off), (Some(3), None));
    }

    #[test]
    fn out_of_bounds_and_unknown_are_errors() {
        let model = DeviceModel::pnet_sample(MAC);
        let req = ConnectReq::parse(&golden("connect_req")[142..]).unwrap();
        let mut params = validate(&req, &model).unwrap();
        params.input_cr.data_length = 10;
        assert!(matches!(Layout::from_ar(&params, &model), Err(LayoutError::OutOfBounds { .. })));
        let mut model2 = model.clone();
        model2.slots.pop();
        let params = validate(&req, &model).unwrap();
        assert!(matches!(Layout::from_ar(&params, &model2), Err(LayoutError::UnknownSubmodule { slot: 4, .. })));
    }
}
```

- [ ] **Step 2: Run, expect compile failure.**
- [ ] **Step 3: Implement** per the interface (`Layout::from_ar` builds both CRs with one private `fn build_cr(cr: &IocrParams, model, direction: Dir) -> Result<CrLayout, LayoutError>`).
- [ ] **Step 4: Run tests + clippy + fmt** → 4 pass.
- [ ] **Step 5: Commit** `git commit -m "feat(rt): C-SDU layout derived from the AR (IOCR offsets, cycle step, watchdog)"`

---
### Task 4: `rt::engine` — PPM/CPM engine, watchdog, stats (pure)

**Files:**
- Create: `crates/profinet-rt/src/rt/engine.rs`
- Modify: `crates/profinet-rt/src/rt/mod.rs` (`pub mod engine; pub use engine::{RtEngine, RtStats, RxVerdict, DropReason, WatchdogVerdict, IOXS_GOOD, IOXS_BAD};`)

**Interfaces:**
- Consumes `Layout`, `RtFrame`, `DataStatus`, `MacAddr`.
- Produces `pub const IOXS_GOOD: u8 = 0x80; pub const IOXS_BAD: u8 = 0x00;`
- Produces `RtStats` (all `AtomicU64`, `Relaxed`): `tx, rx_accepted, rx_ignored, rx_dropped, rx_invalid, reordered, watchdog_expirations, missed_ticks, input_snapshot_reused, output_publish_deferred, max_tick_lateness_ns`; `RtStats::snapshot(&self) -> StatsSnapshot` (plain `u64` struct with the same fields, `Debug, Clone, Copy, PartialEq, Eq`).
- Produces `RxVerdict { Accepted { provider_run: bool, primary: bool, data_valid: bool }, Ignored, Dropped(DropReason) }`, `DropReason { TransferStatus(u8), ShortCsdu { have, need }, Malformed }`, `WatchdogVerdict { NotArmed, Ok, Expired, Stopped }` (all `Debug, Clone, Copy, PartialEq, Eq`).
- Produces `RtEngine::new(layout: Layout, our_mac: MacAddr, cpu_mac: MacAddr, stats: Arc<RtStats>) -> RtEngine` (preallocates `tx: Vec<u8>` of `frame_len(input_cr.data_length)`, `rx_csdu: Vec<u8>` of `output_cr.data_length`, `rx_iops_good: Vec<bool>` per output object (all `false`), `rx_iocs_good: Vec<bool>` per input object (all `false`)).
- `RtEngine::on_tick(&mut self, expirations: u32, inputs: &[u8]) -> &[u8]`: `inputs` is the **full input-CR C-SDU image** (`input_cr.data_length` bytes, the application's data already at each object's `data_off`); the engine copies `inputs[data_off..data_off+data_len]` per object into its TX C-SDU, writes `IOXS_GOOD` at every `iops_off`, writes at every `iocs_off` `IOXS_GOOD` if the matching output object's last IOPS was good else `IOXS_BAD`, advances `cycle_counter = cycle_counter.wrapping_add(cycle_step × expirations as u16)` **before** writing it, data status `0x35`, transfer status 0; `stats.tx += 1`, `stats.missed_ticks += expirations − 1`; returns the frame slice (length `frame_len(data_length)`).
- `RtEngine::on_frame(&mut self, frame: &[u8], now: Instant) -> RxVerdict`: parse errors → `Dropped(Malformed)`; `eth.src ≠ cpu_mac` or `frame_id ≠ output_cr.frame_id` → `Ignored` (`rx_ignored += 1`); `transfer_status ≠ 0` → `Dropped(TransferStatus)`; `csdu.len() < output_cr.data_length` → `Dropped(ShortCsdu)`; otherwise: record `last_rx = now`; cycle counter check — if `Some(prev)` and `cc.wrapping_sub(prev) == 0 || cc.wrapping_sub(prev) > 0x8000` → `reordered += 1` (still accepted); if `data_status.data_valid()` copy the C-SDU into `rx_csdu` and refresh `rx_iops_good[i] = csdu[iops_off] & 0x80 != 0` and `rx_iocs_good[j] = csdu[iocs_off] & 0x80 != 0`, `rx_accepted += 1`; else `rx_invalid += 1` (no copy); `provider_run`/`primary` from the data status; return `Accepted { .. }`.
- `RtEngine::check_watchdog(&mut self, now: Instant) -> WatchdogVerdict`: `NotArmed` before the first accepted frame; `Ok` while `now − last_rx ≤ output_cr.watchdog`; the first time it is exceeded → `Expired` (`watchdog_expirations += 1`), afterwards `Stopped` until a new frame is accepted (which re-arms).
- Accessors: `rx_csdu(&self) -> &[u8]`, `rx_iops_good(&self) -> &[bool]`, `rx_iocs_good(&self) -> &[bool]`, `provider_run(&self) -> bool`, `primary(&self) -> bool`, `last_rx(&self) -> Option<Instant>`, `cycle_counter(&self) -> u16`, `layout(&self) -> &Layout`.

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cm::{validate, ConnectReq, DeviceModel};
    use crate::eth::MacAddr;
    use crate::rt::frame::RtFrame;
    use crate::rt::layout::Layout;
    use crate::testutil::{golden, golden_rt, RT_CSDU_OFF};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const CPU: MacAddr = MacAddr([0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f]);
    const DEV: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);

    fn engine() -> RtEngine {
        let model = DeviceModel::pnet_sample(DEV);
        let req = ConnectReq::parse(&golden("connect_req")[142..]).unwrap();
        let params = validate(&req, &model).unwrap();
        RtEngine::new(Layout::from_ar(&params, &model).unwrap(), DEV, CPU, Arc::new(RtStats::default()))
    }

    #[test]
    fn produced_frame_matches_pnet_except_counter_and_status() {
        let mut e = engine();
        // inputs image: DI = 0x2c at [3], DIO = 0x2d at [6], echo zeros — as p-net sent in rtc_dev_8000
        let mut inputs = vec![0u8; 40];
        inputs[3] = 0x2c;
        inputs[6] = 0x2d;
        // p-net had received the CPU's IOPS GOOD for every output object -> feed one CPU frame first
        e.on_frame(&golden_rt("rtc_cpu_8001"), Instant::now());
        let out = e.on_tick(1, &inputs).to_vec();
        let g = golden_rt("rtc_dev_8000");
        assert_eq!(&out[..60], &g[..60]);            // header + C-SDU identical (IOPS/IOCS all GOOD)
        assert_eq!(&out[60..62], &1024u16.to_be_bytes()); // our first counter = one step
        assert_eq!(out[62], 0x35);                    // we emit Run|Primary|Valid|Ok, p-net 0x36
        assert_eq!(out[63], 0);
        assert_eq!(e.stats_snapshot().tx, 1);
    }

    #[test]
    fn iocs_reflects_received_iops() {
        let mut e = engine();
        let inputs = vec![0u8; 40];
        let out = e.on_tick(1, &inputs).to_vec();
        // no CPU frame yet -> IOCS BAD for the three output objects at [5], [8], [18]
        assert_eq!((out[RT_CSDU_OFF + 5], out[RT_CSDU_OFF + 8], out[RT_CSDU_OFF + 18]), (IOXS_BAD, IOXS_BAD, IOXS_BAD));
        // IOPS of our own objects always GOOD: [0],[1],[2],[4],[7],[17]
        for off in [0, 1, 2, 4, 7, 17] { assert_eq!(out[RT_CSDU_OFF + off], IOXS_GOOD, "iops at {off}"); }
        e.on_frame(&golden_rt("rtc_cpu_8001"), Instant::now());
        let out = e.on_tick(1, &inputs).to_vec();
        assert_eq!((out[RT_CSDU_OFF + 5], out[RT_CSDU_OFF + 8], out[RT_CSDU_OFF + 18]), (IOXS_GOOD, IOXS_GOOD, IOXS_GOOD));
    }

    #[test]
    fn cycle_counter_steps_and_missed_ticks() {
        let mut e = engine();
        let inputs = vec![0u8; 40];
        e.on_tick(1, &inputs);
        assert_eq!(e.cycle_counter(), 1024);
        e.on_tick(3, &inputs);
        assert_eq!(e.cycle_counter(), 4096);
        assert_eq!(e.stats_snapshot().missed_ticks, 2);
        for _ in 0..70 { e.on_tick(1, &inputs); }
        assert_eq!(e.cycle_counter(), (4096u32 + 70 * 1024) as u16); // wraps
    }

    #[test]
    fn consumes_cpu_frame_with_echo_data() {
        let mut e = engine();
        let v = e.on_frame(&golden_rt("echo_cpu_8001"), Instant::now());
        assert_eq!(v, RxVerdict::Accepted { provider_run: true, primary: true, data_valid: true });
        let c = e.rx_csdu();
        assert_eq!(c[4], 0x01);                                  // QB0
        assert_eq!(&c[10..18], &[0x12, 0x34, 0x56, 0x78, 0x3f, 0xc0, 0x00, 0x00]);
        assert!(e.rx_iops_good().iter().all(|g| *g));
        assert!(e.rx_iocs_good().iter().all(|g| *g));
        assert_eq!(e.stats_snapshot().rx_accepted, 1);
    }

    #[test]
    fn ignores_foreign_and_own_frames() {
        let mut e = engine();
        assert_eq!(e.on_frame(&golden_rt("rtc_dev_8000"), Instant::now()), RxVerdict::Ignored); // our own frame id / src
        let mut other = golden_rt("rtc_cpu_8001");
        other[6] = 0x02; // src MAC changed
        assert_eq!(e.on_frame(&other, Instant::now()), RxVerdict::Ignored);
        assert_eq!(e.stats_snapshot().rx_ignored, 2);
    }

    #[test]
    fn drops_bad_transfer_status_and_short_csdu() {
        let mut e = engine();
        let mut f = golden_rt("rtc_cpu_8001");
        f[63] = 0x01;
        assert_eq!(e.on_frame(&f, Instant::now()), RxVerdict::Dropped(DropReason::TransferStatus(1)));
        assert_eq!(e.on_frame(&golden_rt("rtc_cpu_8001")[..50], Instant::now()), RxVerdict::Dropped(DropReason::Malformed));
        assert_eq!(e.stats_snapshot().rx_dropped, 2);
    }

    #[test]
    fn cpu_stop_and_invalid_data() {
        let mut e = engine();
        let mut stop = golden_rt("echo_cpu_8001");
        stop[62] = 0x25; // ProviderState = Stop, still DataValid
        let v = e.on_frame(&stop, Instant::now());
        assert_eq!(v, RxVerdict::Accepted { provider_run: false, primary: true, data_valid: true });
        assert_eq!(e.rx_csdu()[4], 0x01); // data still copied
        let mut invalid = golden_rt("rtc_cpu_8001");
        invalid[62] = 0x31; // DataValid cleared
        let v = e.on_frame(&invalid, Instant::now());
        assert_eq!(v, RxVerdict::Accepted { provider_run: true, primary: true, data_valid: false });
        assert_eq!(e.rx_csdu()[4], 0x01); // not overwritten
        assert_eq!(e.stats_snapshot().rx_invalid, 1);
    }

    #[test]
    fn reordered_frames_are_counted_but_accepted() {
        let mut e = engine();
        let t = Instant::now();
        e.on_frame(&golden_rt("echo_cpu_8001"), t);   // cc 0xe400
        let v = e.on_frame(&golden_rt("rtc_cpu_8001"), t); // cc 0xb800 (older)
        assert!(matches!(v, RxVerdict::Accepted { .. }));
        assert_eq!(e.stats_snapshot().reordered, 1);
    }

    #[test]
    fn watchdog_arms_on_first_frame_and_expires_once() {
        let mut e = engine();
        let t = Instant::now();
        assert_eq!(e.check_watchdog(t), WatchdogVerdict::NotArmed);
        e.on_frame(&golden_rt("rtc_cpu_8001"), t);
        assert_eq!(e.check_watchdog(t + Duration::from_millis(96)), WatchdogVerdict::Ok);
        assert_eq!(e.check_watchdog(t + Duration::from_millis(97)), WatchdogVerdict::Expired);
        assert_eq!(e.check_watchdog(t + Duration::from_millis(200)), WatchdogVerdict::Stopped);
        assert_eq!(e.stats_snapshot().watchdog_expirations, 1);
        e.on_frame(&golden_rt("rtc_cpu_8001"), t + Duration::from_millis(300));
        assert_eq!(e.check_watchdog(t + Duration::from_millis(310)), WatchdogVerdict::Ok);
    }

    #[test]
    fn replay_whole_capture_cpu_frames() {
        // every 0x8001 golden we have parses and is accepted
        let mut e = engine();
        for name in ["rtc_cpu_8001", "echo_cpu_8001"] {
            let (_, rt) = RtFrame::parse(&golden_rt(name)).unwrap();
            assert_eq!(rt.frame_id, 0x8001);
            assert!(matches!(e.on_frame(&golden_rt(name), Instant::now()), RxVerdict::Accepted { .. }));
        }
    }
}
```
(`stats_snapshot(&self)` is a convenience on `RtEngine` returning `self.stats.snapshot()`.)

- [ ] **Step 2: Run, expect compile failure.**
- [ ] **Step 3: Implement** per the interface; keep `on_tick`/`on_frame` free of allocation (`tx` and `rx_csdu` are reused; `RtFrame::write` into `self.tx`).
- [ ] **Step 4: Run tests + clippy + fmt** → 10 pass.
- [ ] **Step 5: Commit** `git commit -m "feat(rt): pure PPM/CPM engine with IOxS, cycle counter, consumer watchdog and stats"`

---

### Task 5: `rt::image` — shared I/O image

**Files:**
- Create: `crates/profinet-rt/src/rt/image.rs`
- Modify: `crates/profinet-rt/src/rt/mod.rs` (`pub mod image; pub use image::{IoImage, ImageError, Validity, WatchdogState};`)

**Interfaces:**
- Produces `Validity { provider_run: bool, primary: bool, watchdog: WatchdogState, last_rx_age: Option<Duration>, cycle: u64 }`, `WatchdogState { NotArmed, Ok, Expired }` (`Copy, Debug, PartialEq, Eq`), `Validity::freshness(&self) -> Freshness` with `Freshness { NoData, Fresh, Stopped, Stale }` (`NoData` if `watchdog == NotArmed`; `Stale` if `Expired`; `Stopped` if `!provider_run`; else `Fresh`).
- Produces `ImageError { UnknownSubmodule { slot, subslot }, LengthMismatch { expected, got }, NoInput { slot, subslot }, NoOutput { slot, subslot } }`.
- Produces `IoImage::new(layout: &Layout) -> IoImage` (cells from `layout.cells`, `inputs: Mutex<Vec<u8>>` of `input_cr.data_length` zeros, `outputs: Mutex<Outputs { csdu: Vec<u8>, validity: Validity }>`), `IoImage::empty() -> IoImage` (no cells; every accessor returns `UnknownSubmodule`), `IoImage::rebuild(&self, layout: &Layout)` (replaces cells and buffers under both locks — used by `device` on each `Data`).
- Application side: `write_inputs(&self, slot, subslot, bytes: &[u8]) -> Result<(), ImageError>` (exact length), `read_outputs<T>(&self, slot, subslot, f: impl FnOnce(&[u8], &Validity) -> T) -> Result<T, ImageError>`, `snapshot_outputs(&self, dst: &mut [u8]) -> Validity` (copies `min(len)`), `validity(&self) -> Validity`, `cells(&self) -> Vec<Cell>` (clone).
- RT side (non-blocking): `rt_snapshot_inputs(&self, dst: &mut [u8]) -> bool` (`try_lock`; `false` = not copied, caller reuses its previous snapshot), `rt_publish(&self, csdu: &[u8], validity: Validity) -> bool` (`try_lock`; copies and stores validity; `false` = deferred), `rt_set_validity(&self, validity: Validity) -> bool` (`try_lock`; validity only — used after a watchdog verdict without new data).

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cm::{validate, ConnectReq, DeviceModel};
    use crate::eth::MacAddr;
    use crate::rt::layout::Layout;
    use crate::testutil::golden;
    use std::time::Duration;

    fn layout() -> Layout {
        let model = DeviceModel::pnet_sample(MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]));
        let req = ConnectReq::parse(&golden("connect_req")[142..]).unwrap();
        Layout::from_ar(&validate(&req, &model).unwrap(), &model).unwrap()
    }
    fn fresh() -> Validity {
        Validity { provider_run: true, primary: true, watchdog: WatchdogState::Ok, last_rx_age: Some(Duration::from_millis(1)), cycle: 7 }
    }

    #[test]
    fn app_writes_land_in_the_rt_snapshot_at_layout_offsets() {
        let img = IoImage::new(&layout());
        img.write_inputs(1, 1, &[0xa5]).unwrap();
        img.write_inputs(4, 1, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        let mut snap = vec![0u8; 40];
        assert!(img.rt_snapshot_inputs(&mut snap));
        assert_eq!(snap[3], 0xa5);
        assert_eq!(&snap[9..17], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(img.write_inputs(1, 1, &[1, 2]).unwrap_err(), ImageError::LengthMismatch { expected: 1, got: 2 });
        assert_eq!(img.write_inputs(2, 1, &[1]).unwrap_err(), ImageError::NoInput { slot: 2, subslot: 1 });
        assert_eq!(img.write_inputs(9, 1, &[1]).unwrap_err(), ImageError::UnknownSubmodule { slot: 9, subslot: 1 });
    }

    #[test]
    fn published_outputs_are_readable_per_cell_with_validity() {
        let img = IoImage::new(&layout());
        let mut csdu = vec![0u8; 40];
        csdu[4] = 0x01;
        csdu[10..18].copy_from_slice(&[0x12, 0x34, 0x56, 0x78, 0x3f, 0xc0, 0x00, 0x00]);
        assert!(img.rt_publish(&csdu, fresh()));
        let (qb0, v) = img.read_outputs(2, 1, |b, v| (b.to_vec(), *v)).unwrap();
        assert_eq!(qb0, vec![0x01]);
        assert_eq!(v.freshness(), Freshness::Fresh);
        let echo = img.read_outputs(4, 1, |b, _| crate::data::decode_f32(&b[4..8]).unwrap()).unwrap();
        assert_eq!(echo, 1.5);
        assert_eq!(img.read_outputs(1, 1, |_, _| ()).unwrap_err(), ImageError::NoOutput { slot: 1, subslot: 1 });
    }

    #[test]
    fn freshness_states() {
        let img = IoImage::new(&layout());
        assert_eq!(img.validity().freshness(), Freshness::NoData);
        let mut v = fresh();
        v.provider_run = false;
        assert!(img.rt_set_validity(v));
        assert_eq!(img.validity().freshness(), Freshness::Stopped);
        v.provider_run = true;
        v.watchdog = WatchdogState::Expired;
        assert!(img.rt_set_validity(v));
        assert_eq!(img.validity().freshness(), Freshness::Stale);
    }

    #[test]
    fn rt_side_never_blocks_under_contention() {
        let img = IoImage::new(&layout());
        let guard = img.inputs.lock().unwrap(); // application holds the lock
        let mut snap = vec![0u8; 40];
        assert!(!img.rt_snapshot_inputs(&mut snap));
        drop(guard);
        assert!(img.rt_snapshot_inputs(&mut snap));
        let guard = img.outputs.lock().unwrap();
        assert!(!img.rt_publish(&[0u8; 40], fresh()));
        drop(guard);
        assert!(img.rt_publish(&[0u8; 40], fresh()));
    }

    #[test]
    fn empty_then_rebuild() {
        let img = IoImage::empty();
        assert_eq!(img.write_inputs(1, 1, &[1]).unwrap_err(), ImageError::UnknownSubmodule { slot: 1, subslot: 1 });
        img.rebuild(&layout());
        img.write_inputs(1, 1, &[1]).unwrap();
        assert_eq!(img.cells().len(), 7);
    }
}
```
(`inputs`/`outputs` are `pub(crate)` fields so the contention test can hold the locks.)

- [ ] **Step 2: Run, expect compile failure.**
- [ ] **Step 3: Implement.** `std::sync::Mutex`; poisoning: `lock().unwrap_or_else(|e| e.into_inner())` on the application side (a panicking application thread must not brick the image); `try_lock()` errors (`WouldBlock` or poisoned) → `false` on the RT side.
- [ ] **Step 4: Run tests + clippy + fmt** → 5 pass.
- [ ] **Step 5: Commit** `git commit -m "feat(rt): shared I/O image (per-cell inputs/outputs, validity, non-blocking RT side)"`

---
### Task 6: `eth` PACKET_OUTGOING drop + `rt::runner` — the RT thread

**Files:**
- Modify: `crates/profinet-rt/src/eth/afpacket.rs`
- Create: `crates/profinet-rt/src/rt/runner.rs`
- Modify: `crates/profinet-rt/src/rt/mod.rs` (`pub mod runner; pub use runner::{RtConfig, RtEvent, RtHandle, RtRunner};`)

**Interfaces:**
- `AfPacketTransport::recv`: replace `recv(2)` by `libc::recvfrom` with a `sockaddr_ll`; if `sll_pkttype == libc::PACKET_OUTGOING` → `Ok(None)` (our own transmissions are looped back to every AF_PACKET socket on the interface). Keep `is_profinet_frame`. Unit test: none possible without a capability (documented); the ignored `open_loopback_succeeds` stays.
- Produces `RtConfig { iface: String, our_mac: MacAddr, cpu_mac: MacAddr, layout: Layout, image: Arc<IoImage>, stats: Arc<RtStats>, cpu_pin: Option<usize>, rt_priority: Option<u8> }`.
- Produces `RtEvent { WatchdogExpired, SocketError(String), SchedWarning(String), Exited }` (`Debug, Clone, PartialEq, Eq`).
- Produces `RtHandle` with `stop(&self)` (sets the flag and writes the wake eventfd so a `poll` without timeout returns), `join(self, timeout: Duration) -> Result<(), RtError>` (`RtError::Stopped` if the thread does not exit in time; the thread is then detached), `event_fd(&self) -> RawFd` (readable when an event is pending), `take_event(&self) -> Option<RtEvent>` (drains one event from a `Mutex<VecDeque<RtEvent>>`; also reads/clears the eventfd counter), `stats(&self) -> Arc<RtStats>`, `is_running(&self) -> bool`.
- Produces `RtRunner::spawn(cfg: RtConfig) -> Result<RtHandle, RtError>` (opens `AfPacketTransport::open(&cfg.iface)` **in the spawning thread** so an `open` error is returned synchronously) and `RtRunner::spawn_with_transport<T: EthTransport + 'static>(cfg: RtConfig, transport: T) -> Result<RtHandle, RtError>` (used by tests with `MockTransport`; a transport without `raw_fd` is serviced by a zero-timeout `recv` drain after each tick instead of being polled).
- Thread body (`fn run_loop`): 
  1. `sched`: if `rt_priority` is `Some(p)` → `libc::sched_setscheduler(0, SCHED_FIFO, &sched_param { sched_priority: p as c_int })`; on failure push `SchedWarning(errno text)` and continue. If `cpu_pin` is `Some(c)` → `cpu_set_t` zeroed, `CPU_SET(c)`, `sched_setaffinity(0, size_of::<cpu_set_t>(), &set)`; on failure `SchedWarning`.
  2. timerfd: `libc::timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK | TFD_CLOEXEC)`, `timerfd_settime(fd, 0, &itimerspec { it_interval: period, it_value: period }, null)` with `period = layout.input_cr.period()`.
  3. Preallocate: `snapshot: Vec<u8>` (`input_cr.data_length`), the engine (`RtEngine::new`), `rx: Vec<u8>` (1522).
  4. Loop while `!stop`: `poll` on `[timerfd, wake_fd, socket_fd?]` (no timeout; use `crate::eth::poll::wait_any_readable` extended with a variant returning **which** fds are readable: add `pub(crate) fn poll_readable(fds: &[RawFd], timeout: Option<Duration>) -> io::Result<Vec<bool>>`? — no allocation allowed in the loop: add instead `pub(crate) fn poll_readable_into(fds: &[RawFd], ready: &mut [bool], timeout: Option<Duration>) -> io::Result<usize>` in `eth/poll.rs`, with a unit test on loopback UDP).
     - timer readable → `read(timerfd, &mut [u8; 8])` → `expirations: u64`; `wd = engine.check_watchdog(now)`; if `wd == Expired` → push `WatchdogExpired`, write the event eventfd, and publish `Validity` with `WatchdogState::Expired`; if `!image.rt_snapshot_inputs(&mut snapshot)` → `stats.input_snapshot_reused += 1`; `let frame = engine.on_tick(expirations as u32, &snapshot)`; `transport.send(frame)` — on `Err` push `SocketError`, write the eventfd, exit the loop; `max_tick_lateness_ns` = max of (`now − expected_tick_instant`) computed from the first tick's `Instant` + `n × period`.
     - socket readable → loop `transport.recv(Some(Duration::ZERO))` until `Ok(None)`: `match engine.on_frame(&buf, now)` → on `Accepted { .. }` build `Validity { provider_run, primary, watchdog: Ok, last_rx_age: Some(0), cycle }` and `if !image.rt_publish(engine.rx_csdu(), validity) { stats.output_publish_deferred += 1; pending_publish = true }`; on the next tick, if `pending_publish` retry once.
     - wake fd readable → read it (stop requested).
  5. On exit: push `Exited`, write the eventfd. `RtHandle::join` joins the `JoinHandle` with a timeout loop (`is_finished()` + sleep 1 ms).
- **Never** log inside the loop; the acyclic side logs the events.

- [ ] **Step 1: Failing tests** (`runner.rs`; timerfd/eventfd need no capability; `MockTransport` has no fd → drain path)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cm::{validate, ConnectReq, DeviceModel};
    use crate::eth::{MacAddr, MockTransport};
    use crate::rt::image::{Freshness, IoImage};
    use crate::rt::layout::Layout;
    use crate::testutil::{golden, golden_rt};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const CPU: MacAddr = MacAddr([0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f]);
    const DEV: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);

    fn layout() -> Layout {
        let model = DeviceModel::pnet_sample(DEV);
        let req = ConnectReq::parse(&golden("connect_req")[142..]).unwrap();
        Layout::from_ar(&validate(&req, &model).unwrap(), &model).unwrap()
    }

    /// Shrink the period for the test: 5 ms instead of 32 ms (cycle_step stays 1024 for the counter).
    fn cfg(image: Arc<IoImage>, stats: Arc<RtStats>) -> RtConfig {
        let mut layout = layout();
        layout.input_cr.cycle_step = 160; // 160 × 31.25 µs = 5 ms
        layout.output_cr.cycle_step = 160;
        layout.output_cr.watchdog = Duration::from_millis(15);
        RtConfig { iface: String::new(), our_mac: DEV, cpu_mac: CPU, layout, image, stats, cpu_pin: None, rt_priority: None }
    }

    #[test]
    fn runner_ticks_sends_and_consumes_with_a_mock_transport() {
        let image = Arc::new(IoImage::new(&layout()));
        let stats = Arc::new(RtStats::default());
        let mock = MockTransport::new();
        mock.push_rx(golden_rt("echo_cpu_8001"));
        let mock = Arc::new(mock);
        image.write_inputs(1, 1, &[0x5a]).unwrap();
        let h = RtRunner::spawn_with_transport(cfg(image.clone(), stats.clone()), SharedMock(mock.clone())).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        h.stop();
        h.join(Duration::from_secs(1)).unwrap();
        let sent = mock.sent();
        assert!(sent.len() >= 8 && sent.len() <= 14, "sent {}", sent.len());
        assert_eq!(&sent[0][12..18], &[0x81, 0x00, 0xc0, 0x00, 0x88, 0x92]);
        assert_eq!(sent[0][20 + 3], 0x5a); // our DI byte from the image
        // the CPU frame was consumed and published
        let qb0 = image.read_outputs(2, 1, |b, _| b[0]).unwrap();
        assert_eq!(qb0, 0x01);
        assert_eq!(image.validity().freshness(), Freshness::Stale); // watchdog 15 ms expired after the single frame
        assert!(stats.snapshot().tx >= 8);
        assert_eq!(stats.snapshot().rx_accepted, 1);
        assert_eq!(stats.snapshot().watchdog_expirations, 1);
        assert_eq!(h.take_event(), Some(RtEvent::WatchdogExpired));
        assert_eq!(h.take_event(), Some(RtEvent::Exited));
    }

    #[test]
    fn stop_is_prompt_and_join_times_out_cleanly() {
        let image = Arc::new(IoImage::new(&layout()));
        let h = RtRunner::spawn_with_transport(cfg(image, Arc::new(RtStats::default())), MockTransport::new()).unwrap();
        let t = Instant::now();
        h.stop();
        h.join(Duration::from_secs(1)).unwrap();
        assert!(t.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn sched_warning_is_reported_not_fatal() {
        let image = Arc::new(IoImage::new(&layout()));
        let mut c = cfg(image, Arc::new(RtStats::default()));
        c.rt_priority = Some(80); // no CAP_SYS_NICE in the test environment
        let h = RtRunner::spawn_with_transport(c, MockTransport::new()).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        h.stop();
        h.join(Duration::from_secs(1)).unwrap();
        let first = h.take_event();
        assert!(matches!(first, Some(RtEvent::SchedWarning(_))), "{first:?}");
    }

    /// `MockTransport` is not `Clone`; share it through an `Arc` for the test.
    struct SharedMock(Arc<MockTransport>);
    impl crate::eth::EthTransport for SharedMock {
        fn send(&self, f: &[u8]) -> Result<(), crate::eth::TransportError> { self.0.send(f) }
        fn recv(&self, t: Option<Duration>) -> Result<Option<Vec<u8>>, crate::eth::TransportError> { self.0.recv(t) }
    }
}
```
(If the environment does run as root and `SCHED_FIFO` succeeds, `sched_warning_is_reported_not_fatal` must be skipped: guard it with `if unsafe { libc::geteuid() } == 0 { return; }`.)

- [ ] **Step 2: Run, expect compile failure.**
- [ ] **Step 3: Implement** `afpacket.rs` change, `poll_readable_into` in `eth/poll.rs` (+ loopback test), `runner.rs`. Every `libc` call in an `unsafe` block with a `// Safety:` line; timerfd/eventfd wrapped in `OwnedFd`.
- [ ] **Step 4: Run tests + clippy + fmt** → 3 runner tests + poll test pass; full suite green.
- [ ] **Step 5: Commit** `git commit -m "feat(rt,eth): RT runner thread (timerfd, eventfd, optional SCHED_FIFO); drop looped-back PACKET_OUTGOING frames"`

---

### Task 7: `device` integration — runner lifecycle, watchdog abort, `image()`

**Files:**
- Modify: `crates/profinet-rt/src/cm/ar.rs` (`AbortReason::RtWatchdog`), `crates/profinet-rt/src/cm/mod.rs` (`Cm::abort(&mut self, reason: AbortReason, now: Instant) -> CmOutput` if not present — it feeds `Event::Abort(reason)` to `Ar` and maps the actions like `handle_datagram`), `crates/profinet-rt/src/device/mod.rs`

**Interfaces:**
- Produces `RtOptions { iface: String, cpu_pin: Option<usize>, rt_priority: Option<u8> }` (`Debug, Clone`); `DeviceSetup` gains `pub rt: Option<RtOptions>` (`None` = no cyclic thread: the mock-based tests and the AR-only example keep working unchanged — update the existing `setup()` helpers in `device/mod.rs` tests, `tests/ar_replay.rs`, `examples/ar_bringup.rs` with `rt: None`).
- Produces `Device::image(&self) -> Arc<IoImage>` (created in `new` as `IoImage::empty()`), `Device::rt_stats(&self) -> Arc<RtStats>`, `Device::rt_running(&self) -> bool`.
- Behaviour in `dispatch`, after invoking the state-change callback for each notify:
  - `(ArState::Data, None)` and `setup.rt.is_some()` → `Layout::from_ar(&cm.context().unwrap().params, &setup.model)` (on `Err` → `log::error!` and no runner: the AR stays up without cyclic data, as in Plan 3); `image.rebuild(&layout)`; `RtRunner::spawn(RtConfig { iface, our_mac: setup.dcp.mac, cpu_mac: params.initiator_mac, layout, image, stats, cpu_pin, rt_priority })` → `self.runner = Some(handle)`; spawn error → `log::error!`, no runner.
  - `(ArState::Idle, Some(_))` → if a runner exists: `stop()`, `join(500 ms)` (`Err` → `log::warn!`), `self.runner = None`.
- `Device::step`: if a runner exists, its `event_fd()` joins the `wait_any_readable` list; after the drains, `while let Some(ev) = runner.take_event()`: `WatchdogExpired` → `log::warn!` + `dispatch(cm.abort(AbortReason::RtWatchdog, now))` (which stops the runner through the `Idle` notify above); `SocketError(s)` → `log::error!` + same abort; `SchedWarning(s)` → `log::warn!`; `Exited` → `log::info!`.
- `Device::run` unchanged; `Drop for Device` stops a running runner.

- [ ] **Step 1: Failing tests** (`device/mod.rs`; a `MockTransport`-backed runner is only reachable through `spawn_with_transport`, so the device tests exercise the lifecycle through a test-only hook: add `#[cfg(test)] fn spawn_runner_with(&mut self, transport: impl EthTransport + 'static)` — or simpler: make the runner factory a field `runner_factory: Box<dyn Fn(RtConfig) -> Result<RtHandle, RtError> + Send>` defaulting to `RtRunner::spawn`, overridable with `Device::with_runner_factory(..)` (public, documented as test/embedding hook). Choose the factory.)

```rust
    #[test]
    fn data_starts_the_runner_and_idle_stops_it() {
        let eth = MockTransport::new();
        let rpc = MockRpcTransport::new();
        let cpu = "172.16.2.100:54766".parse().unwrap();
        let cpu_cm = "172.16.2.100:34964".parse().unwrap();
        rpc.push_rx(golden("connect_req")[RPC_OFF..].to_vec(), cpu);
        rpc.push_rx(golden("prmend_req")[RPC_OFF..].to_vec(), cpu);
        rpc.push_rx(golden("appready_res")[RPC_OFF..].to_vec(), cpu_cm);
        let mut s = setup();
        s.rt = Some(RtOptions { iface: "mock".into(), cpu_pin: None, rt_priority: None });
        let mut dev = Device::new(s, eth, rpc);
        dev.with_runner_factory(|cfg| RtRunner::spawn_with_transport(cfg, MockTransport::new()));
        dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
        assert_eq!(dev.state(), ArState::Data);
        assert!(dev.rt_running());
        assert_eq!(dev.image().cells().len(), 7);
        // controller Release -> Idle -> runner stopped
        let mut rel = golden("prmend_req")[RPC_OFF..].to_vec();
        rel[68] = 1; // opnum Release (LE low byte)
        rel[64] = 9; // new seq_num
        rel[100] = 0x01; rel[101] = 0x14; // block type ReleaseBlockReq
        rel[126] = 0x00; rel[127] = 0x04; // command Release
        dev.rpc().push_rx(rel, cpu);
        dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
        assert_eq!(dev.state(), ArState::Idle);
        assert!(!dev.rt_running());
    }

    #[test]
    fn watchdog_event_aborts_the_ar() {
        // same bring-up as above, then a runner whose watchdog expires quickly (no CPU frames on the mock)
        // ... (build the device with a factory that shrinks layout.output_cr.watchdog to 10 ms and
        //      feeds one CPU frame so the watchdog arms) ...
        // after ~50 ms: step() must have consumed RtEvent::WatchdogExpired -> state Idle with
        // notify (Idle, Some(AbortReason::RtWatchdog)) and no runner.
    }
```
Write the second test in full: the factory closure clones the config, sets `cfg.layout.output_cr.watchdog = Duration::from_millis(10)` and `cycle_step = 160` on both CRs, pushes `golden_rt("rtc_cpu_8001")` on a fresh `MockTransport` and calls `spawn_with_transport`; the test then sleeps 60 ms, calls `dev.step(..)` and asserts `dev.state() == ArState::Idle`, the recorded notifies end with `(ArState::Idle, Some(AbortReason::RtWatchdog))`, and `!dev.rt_running()`.

- [ ] **Step 2: Run, expect compile failure.**
- [ ] **Step 3: Implement.** Keep `device/mod.rs` readable: the runner lifecycle in a private `impl` block (`start_runner`, `stop_runner`, `drain_rt_events`).
- [ ] **Step 4: Run the whole suite + clippy + fmt** → green (existing device tests unchanged except `rt: None`).
- [ ] **Step 5: Commit** `git commit -m "feat(device): start/stop the RT runner on Data/Idle, watchdog expiry aborts the AR, expose the I/O image"`

---
### Task 8: `tests/rt_replay.rs` + `examples/rt_bringup.rs` + edge build (offline part)

**Files:**
- Create: `crates/profinet-rt/tests/rt_replay.rs`, `crates/profinet-rt/examples/rt_bringup.rs`

- [ ] **Step 1: `tests/rt_replay.rs`** — no thread: bring the `Device` (mocks, `rt: None`) to `Data`, then drive an `RtEngine` + `IoImage` by hand with the goldens, exactly like the runner would:

```rust
//! Cyclic replay: AR to Data through Device (mocks), then the engine consumes the bench CPU frames
//! and produces ours; the application reads/writes through IoImage.
mod common;

use common::{golden, golden_rt, RPC_OFF, RT_CSDU_OFF};
use profinet_rt::cm::{ArState, DeviceModel};
use profinet_rt::dcp::{DeviceConfig, DeviceProperties};
use profinet_rt::device::{Device, DeviceSetup};
use profinet_rt::eth::{MacAddr, MockTransport};
use profinet_rt::rpc::{MockRpcTransport, Uuid};
use profinet_rt::rt::{Freshness, IoImage, Layout, RtEngine, RtStats, RxVerdict, Validity, WatchdogState, IOXS_GOOD};
use std::sync::Arc;
use std::time::{Duration, Instant};

const CPU: MacAddr = MacAddr([0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f]);
const DEV: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);

fn setup() -> DeviceSetup {
    DeviceSetup {
        dcp: DeviceConfig {
            mac: DEV,
            properties: DeviceProperties {
                name_of_station: "rt-labs-dev".into(), type_of_station: "P-Net Sample Application".into(),
                vendor_id: 0x0493, device_id: 0x0002, device_role: 0x0100, device_instance: 1,
                device_options: vec![1, 2, 2, 2, 2, 3],
                ip: [172, 16, 2, 10], subnet: [255, 255, 255, 0], gateway: [172, 16, 2, 10], ip_block_info: 1,
            },
        },
        model: DeviceModel::pnet_sample(DEV),
        activity_seed: Uuid::parse_str("14af198a-1234-1056-8079-8cf319cd19f8").unwrap(),
        rt: None,
    }
}

#[test]
fn cyclic_round_trip_over_the_bench_frames() {
    let rpc = MockRpcTransport::new();
    let cpu = "172.16.2.100:54766".parse().unwrap();
    let cpu_cm = "172.16.2.100:34964".parse().unwrap();
    for n in ["connect_req", "write_req", "prmend_req"] { rpc.push_rx(golden(n)[RPC_OFF..].to_vec(), cpu); }
    rpc.push_rx(golden("appready_res")[RPC_OFF..].to_vec(), cpu_cm);
    let mut dev = Device::new(setup(), MockTransport::new(), rpc);
    dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    assert_eq!(dev.state(), ArState::Data);

    // What the runner would do at Data:
    let params = dev.ar_params().expect("params in Data"); // add `Device::ar_params(&self) -> Option<ArParams>` (clone) in Task 7 if missing
    let layout = Layout::from_ar(&params, &DeviceModel::pnet_sample(DEV)).unwrap();
    let image = Arc::new(IoImage::new(&layout));
    let stats = Arc::new(RtStats::default());
    let mut engine = RtEngine::new(layout, DEV, CPU, stats.clone());

    // Application mirrors QB0 -> IB0 and echoes the Echo module, like rt_bringup does.
    let t0 = Instant::now();
    let v = engine.on_frame(&golden_rt("echo_cpu_8001"), t0);
    assert!(matches!(v, RxVerdict::Accepted { data_valid: true, .. }));
    assert!(image.rt_publish(engine.rx_csdu(), Validity { provider_run: true, primary: true, watchdog: WatchdogState::Ok, last_rx_age: Some(Duration::ZERO), cycle: 1 }));
    let qb0 = image.read_outputs(2, 1, |b, v| { assert_eq!(v.freshness(), Freshness::Fresh); b[0] }).unwrap();
    let echo = image.read_outputs(4, 1, |b, _| b.to_vec()).unwrap();
    image.write_inputs(1, 1, &[qb0]).unwrap();
    image.write_inputs(4, 1, &echo).unwrap();

    let mut snap = vec![0u8; 40];
    assert!(image.rt_snapshot_inputs(&mut snap));
    let frame = engine.on_tick(1, &snap).to_vec();
    assert_eq!(&frame[..12], &[0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f, 0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);
    assert_eq!(&frame[12..20], &[0x81, 0x00, 0xc0, 0x00, 0x88, 0x92, 0x80, 0x00]);
    let c = &frame[RT_CSDU_OFF..RT_CSDU_OFF + 40];
    assert_eq!(c[3], 0x01);                                             // IB0 mirrors QB0
    assert_eq!(&c[9..17], &[0x12, 0x34, 0x56, 0x78, 0x3f, 0xc0, 0x00, 0x00]); // true echo
    for off in [0, 1, 2, 4, 5, 7, 8, 17, 18] { assert_eq!(c[off], IOXS_GOOD, "ioxs at {off}"); }
    assert_eq!(&frame[60..64], &[0x04, 0x00, 0x35, 0x00]);
    assert_eq!(stats.snapshot().tx, 1);
}
```
Run: `cargo test -p profinet-rt --test rt_replay` → 1 passed. (`Device::ar_params()` — a small public accessor returning `Option<ArParams>` — belongs to Task 7; add it there if it is missing when you get here, and say so in the report.)

- [ ] **Step 2: `examples/rt_bringup.rs`** — copy `examples/ar_bringup.rs` and extend:
  - CLI: `--iface`, `--name` (default `rt-labs-dev`), `--ip`, `--rt-priority <u8>` (optional), `--cpu <usize>` (optional), `--stats-every <secs>` (default 5).
  - `DeviceSetup { .., rt: Some(RtOptions { iface: a.iface.clone(), cpu_pin: a.cpu, rt_priority: a.rt_priority }) }`.
  - The application thread: `let image = dev.image();` before `run`; spawn a std thread that every 10 ms does: `let qb0 = image.read_outputs(2, 1, |b, _| b[0]); let qb1 = image.read_outputs(3, 1, |b, _| b[0]); let echo = image.read_outputs(4, 1, |b, _| b.to_vec());` — when the cells are unknown (no AR yet) the calls return `Err(UnknownSubmodule)`: ignore and retry — then `write_inputs(1, 1, &[qb0])`, `write_inputs(3, 1, &[qb1])`, `write_inputs(4, 1, &echo)`; every `stats_every` seconds log `dev.rt_stats().snapshot()` and `image.validity().freshness()` (use an `Arc<RtStats>` and `Arc<IoImage>` cloned into the thread; the device itself stays on the main thread in `run`).
  - State-change logging as in `ar_bringup`; `AbortReason::RtWatchdog` shows up through the same callback.
  - Build: `cargo build --example rt_bringup && cargo clippy --all-targets -- -D warnings && cargo fmt --all --check`.

- [ ] **Step 3: Edge build + copy (no run)**: `cargo build --release --example rt_bringup --target x86_64-unknown-linux-musl`, `scp target/x86_64-unknown-linux-musl/release/examples/rt_bringup maintenance@192.168.1.21:bench/rt_bringup`, verify `ssh -o BatchMode=yes maintenance@192.168.1.21 '~/bench/rt_bringup --help'`. **Do not run it against the interface, do not touch `pn_dev`, no sudo.**

- [ ] **Step 4: Commit** `git add crates/profinet-rt/tests/rt_replay.rs crates/profinet-rt/examples/rt_bringup.rs && git commit -m "feat: rt_bringup example (mirror/echo application) + cyclic replay test"`

---

### Task 9: HIL on the edge + docs + follow-ups

- [ ] **Step 1: HIL** (controller with the user — needs `sudo setcap cap_net_raw,cap_net_admin,cap_sys_nice+eip /home/maintenance/bench/rt_bringup`; `cap_sys_nice` lets `--rt-priority` work). On the edge: stop `pn_dev`, start a capture (`~/bench/capture.sh hil-rt`), run `RUST_LOG=info ~/bench/rt_bringup --iface eno2 --name rt-labs-dev --ip 172.16.2.10 --rt-priority 50 --stats-every 5` for ≥ 90 s, stop the capture. Expected: `AR state: Data` once and **no further state change**; stats every 5 s with `tx ≈ rx_accepted` growing at 31/s, `missed_ticks == 0`, `watchdog_expirations == 0`, freshness `Fresh` (CPU in RUN) or `Stopped` (CPU in STOP — still no abort); no `Read 0xfbff` in the capture; in TIA: device green, `%IB0 == %QB0`, `%IB1 == %QB1`, `%ID2 == %QD2` and `%ID6 == %QD6` in a watch table after modifying the Q values. Then run once with `--rt-priority` omitted to confirm the fallback, and once with the CPU switched to STOP and back to RUN (freshness `Stopped` → `Fresh`, no abort). Decode the capture: our `0x8000` frames every 32 ms, cycle counter step 1024, data status `0x35`, IOCS/IOPS `0x80`.

- [ ] **Step 2: Docs**: `docs/bench-pnet-device.md` §6d "HIL — cyclic exchange (2026-08-xx)" (binary, command, stats excerpt, capture name, TIA observations, what a 1 ms run needs: Plan 7); `README.md` status table (`rt` ✅ at 32 ms, "1 ms + determinism = Plan 7"; `Alarms + I&M` still ⏳; HIL row → "AR + cyclic data on a real S7-1500"); `FOLLOWUPS.md` new section "From Plan 4 (`rt`)": per-socket BPF filters (both AF_PACKET sockets see every `0x8892` frame), lock-free seqlock if `input_snapshot_reused`/`output_publish_deferred` are non-zero at 1 ms, `PACKET_AUXDATA` (RX VLAN priority) if ever needed, `mlockall`/`isolcpus`/IRQ affinity (Plan 7), ERR-RTA on stop (Plan 5), ProblemIndicator/diagnosis (Plan 5), `RtHandle::join` detaches on timeout, half-fd `Device` configuration still spins (pre-existing).

- [ ] **Step 3: Final verification + commit** `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test --all` → green; `git commit -m "docs: Plan 4 close-out — cyclic HIL results, README status, follow-ups"`. Then `superpowers:finishing-a-development-branch`.

---

## Self-review notes (done while writing)

- Spec coverage: §5 → Task 2; §6 → Task 3; §7 → Task 4; §8 → Task 5; §9 → Tasks 6-7; §10 → Tasks 4-7 (missed ticks, reordered, STOP, watchdog, takeover via Idle/Data notifies, no-application zeros); §11 → every task's tests + Tasks 8-9; §12 → none new.
- Deviations from the spec, deliberate: `poll_readable_into` (which-fd-is-ready variant of the poll helper) added in Task 6 because the runner must not allocate; `Device::with_runner_factory` and `Device::ar_params` added as embedding/test hooks (Task 7/8); the runner reports `Exited` as an event so `device` can log a clean stop.
- Type consistency: `Layout { input_cr, output_cr, cells }`, `CrLayout::period()`, `IoObject/CsObject/Cell`, `RtEngine::{new, on_tick, on_frame, check_watchdog, rx_csdu, rx_iops_good, rx_iocs_good, cycle_counter, stats_snapshot}`, `RxVerdict/DropReason/WatchdogVerdict`, `RtStats::{snapshot}`, `IoImage::{new, empty, rebuild, write_inputs, read_outputs, snapshot_outputs, validity, cells, rt_snapshot_inputs, rt_publish, rt_set_validity}`, `Validity/WatchdogState/Freshness`, `RtConfig/RtEvent/RtHandle::{stop, join, event_fd, take_event, stats, is_running}`, `RtRunner::{spawn, spawn_with_transport}`, `RtOptions`, `Device::{image, rt_stats, rt_running, with_runner_factory, ar_params}` — used consistently across Tasks 2-8. `rt::mod.rs` must re-export `Freshness` alongside `Validity`.

# Plan 5 — alarm channel, application diagnosis, I&M records — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The device sends channel diagnosis alarms (and its own abort) to the S7-1500 over the PROFINET alarm channel, sets `ProblemIndicator` in the cyclic data status, and serves/persists I&M0-3 records — all driven from `IoDevice` and declared in the GSDML.

**Architecture:** A pure `alarm` module (RTA-PDU codec + one-alarm-in-flight sender/receiver state machine) and a pure `diag` store live on the acyclic thread next to `cm`, driven by the existing `Device` loop; `cm` gains a `records` handler for I&M reads/writes backed by a small `im` store; the RT thread reads one `AtomicBool` for the data-status bit. No new thread, no new dependency.

**Tech Stack:** Rust 2021, `libc`/`nix` (existing), classic BPF filter (existing), goldens from `captures/plan5-20260830/plan5-alarm.pcapng` already extracted to `crates/pnio/testdata/alarm/*.hex`.

**Spec:** `docs/design/2026-08-30-pnio-alarm-diag-im-design.md`

## Global Constraints

- No new runtime dependency (spec §10): the I&M file is raw record bytes via `std::fs`.
- Nothing allocating or blocking is added to `rt::runner`'s loop; the RT change is one `AtomicBool::load(Ordering::Relaxed)` per tick (spec §5.7).
- Byte-exact goldens: every frame the device emits that has a p-net counterpart in `testdata/alarm/` must round-trip `parse → build == golden` (spec §5.1, §7).
- Wire constants (spec §4): High = FrameID `0xFC01` / TCI `0xC000`, Low = `0xFE01` / `0xA000`; RTA header 12 bytes; `SendSeqNum` starts `0xFFFF` then `0,1,…` wrapping after `0x7FFF`; `AckSeqNum`/"none" = `0xFFFE`; ERR-RTA always Low; PNIOStatus for RTA aborts `CF 81 FD xx`.
- Standard channel diagnosis only: USI `0x8000`, `ChannelErrorType 0x0001..=0x0009`; `MayIssueProcessAlarm` stays `false`; `PNIO_Version` stays `"V2.3"`.
- `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all` green at every commit; existing goldens/replays untouched.
- Commit messages follow the repo style (`feat(alarm): …`, `test(...)`, `docs(...)`), one commit per task minimum.
- Public repo: no customer/site names in code, tests or docs (aliases only).

---

## File structure

| Path | Responsibility |
|---|---|
| `crates/pnio/testdata/alarm/*.hex` | 20 goldens (already committed with this plan) |
| `crates/pnio/src/testutil.rs`, `tests/common/mod.rs` | + `golden_alarm(name)` |
| `docs/alarm-golden-frames.md` | catalogue + key wire facts |
| `crates/pnio/src/alarm/mod.rs` | module root, re-exports |
| `crates/pnio/src/alarm/rta.rs` | RTA-PDU codec (header, DATA blocks, ACK, ERR, frame build/parse) |
| `crates/pnio/src/alarm/channel.rs` | sender/receiver state machine (pure) |
| `crates/pnio/src/diag.rs` | `ChannelError`, `Severity`, `Diagnosis`, `DiagStore` |
| `crates/pnio/src/im.rs` | `Im0`, `SwRevision`, `ImStore`, `encode_im0` |
| `crates/pnio/src/cm/records.rs` | `ReadReq`, `build_read_res`, I&M read/write handling |
| `crates/pnio/src/cm/status.rs`, `cm/ar.rs`, `cm/connect.rs`, `cm/mod.rs` | RTA status codes, new `AbortReason`s, AlarmCR params in `ArParams`, Read/Write dispatch |
| `crates/pnio/src/rt/engine.rs`, `rt/runner.rs` | problem-indicator bit |
| `crates/pnio/src/device/mod.rs` | alarm channel lifecycle, frame routing, diag queue, ERR-RTA, replay |
| `crates/pnio/src/config.rs`, `gsdml.rs`, `api.rs` | `Im0` in the builder, GSDML attributes, diag API, `im_store` |
| `crates/pnio/examples/typed_bringup.rs` | `--diag`, `--im-store` |
| `crates/pnio/tests/alarm_replay.rs` | end-to-end replay |
| `docs/gsdml.md`, `README.md`, `FOLLOWUPS.md`, `docs/bench-pnet-device.md` | docs |

---

### Task 1: Golden helpers and the golden-frame catalogue

**Files:**
- Modify: `crates/pnio/src/testutil.rs` (add after `golden`)
- Modify: `crates/pnio/tests/common/mod.rs` (add after `golden_rt`)
- Create: `docs/alarm-golden-frames.md`
- Test: `crates/pnio/src/testutil.rs` (inline test)

**Interfaces:**
- Produces: `pub fn golden_alarm(name: &str) -> Vec<u8>` (both helpers), loading `testdata/alarm/<name>.hex`.

- [ ] **Step 1: Write the failing test** in `crates/pnio/src/testutil.rs` (append):

```rust
#[cfg(test)]
mod tests {
    use super::golden_alarm;

    #[test]
    fn every_alarm_golden_is_a_tagged_profinet_frame_or_rpc() {
        for name in [
            "alarm_process_notif", "alarm_ack_rta_high_cpu", "alarm_ack_high_cpu", "alarm_ack_rta_high_dev",
            "alarm_diag_notif", "alarm_ack_rta_low_cpu", "alarm_diag_ack_cpu", "alarm_ack_rta_low_dev",
            "alarm_diag_update_appears", "alarm_diag_update_others_remain", "alarm_diag_usi_disappears",
            "alarm_diag_std_remove", "alarm_err_rta_dev", "alarm_err_rta_cpu", "alarm_err_rta_cpu_removed",
            "alarm_err_rta_dev_removed_reply",
        ] {
            let f = golden_alarm(name);
            assert_eq!(&f[12..14], &[0x81, 0x00], "{name}: VLAN tag");
            assert_eq!(&f[16..18], &[0x88, 0x92], "{name}: EtherType");
            assert!(matches!(f[18..20], [0xfc, 0x01] | [0xfe, 0x01]), "{name}: FrameID");
        }
        for name in ["im0_read_req", "im0_read_res", "im0_read_req_if", "im0_read_res_if"] {
            let f = golden_alarm(name);
            assert_eq!(&f[12..14], &[0x08, 0x00], "{name}: IPv4");
        }
    }
}
```

- [ ] **Step 2: Run** `cargo test -p pnio golden_alarm` → FAIL (`golden_alarm` not found).

- [ ] **Step 3: Implement** — add to `src/testutil.rs` after `golden`:

```rust
/// Load `testdata/alarm/<name>.hex` (2026-08-30 p-net alarm/I&M capture) relative to the crate root.
pub fn golden_alarm(name: &str) -> Vec<u8> {
    let path = format!("{}/testdata/alarm/{name}.hex", env!("CARGO_MANIFEST_DIR"));
    parse_hex(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")))
}
```
and the identical function to `tests/common/mod.rs` (with the same doc comment, the file duplicates `testutil` on purpose).

- [ ] **Step 4: Write `docs/alarm-golden-frames.md`** with: provenance (capture file, date, p-net v0.2.0 `pn_dev`, CPU 1515-2 PN FW V2.9.4, TIA V21, X1 at 32 ms update time, DGS-1008P in line); a table `| file | frame # | direction | bytes | what |` for the 20 files (the `#` comment line of each `.hex` carries the description — copy it); a "Key facts" list copied from spec §4 (header layout, sequence rules with the captured values, VLAN priorities, block types/lengths, ChannelProperties bit fields with the captured `0x2801`/`0x3801`/`0x2001`, PNIOStatus codes seen `00/0B/11`, the negotiated AlarmCR `TFactor 1 / Retries 3 / Ref 0 / Len 256 / Tag 0xC000-0xA000`, I&M0 layout and the fact TIA reads `0xAFF0` on DAP `0/1`, interface `0/0x8000`+`0/0x8001` and every module, and never wrote I&M1 because the p-net GSDML lacks `Writeable_IM_Records`).

- [ ] **Step 5: Run** `cargo test -p pnio golden_alarm` → PASS. `cargo fmt --all --check`.

- [ ] **Step 6: Commit**: `git add crates/pnio/src/testutil.rs crates/pnio/tests/common/mod.rs docs/alarm-golden-frames.md && git commit -m "test(alarm): golden loader for the 2026-08-30 alarm/I&M capture + catalogue"`

---

### Task 2: `alarm::rta` — RTA-PDU codec

**Files:**
- Create: `crates/pnio/src/alarm/mod.rs`, `crates/pnio/src/alarm/rta.rs`
- Modify: `crates/pnio/src/lib.rs` (add `pub mod alarm;` and `pub mod diag; pub mod im;` are added in their own tasks)

**Interfaces:**
- Consumes: `crate::eth::{EthHeader, MacAddr, ETHERTYPE_PROFINET}`, `crate::cm::block::{BlockHeader, Cursor, BlockError}`, `crate::cm::PnioStatus`.
- Produces (all `pub` in `pnio::alarm`): constants `FRAME_ID_HIGH = 0xFC01`, `FRAME_ID_LOW = 0xFE01`, `TCI_HIGH = 0xC000`, `TCI_LOW = 0xA000`, `SEQ_INIT = 0xFFFF`, `SEQ_NONE = 0xFFFE`, `USI_CHANNEL_DIAG = 0x8000`, `USI_EXT_CHANNEL_DIAG = 0x8002`; `enum Priority { High, Low }` with `frame_id()`/`tci()`; `enum PduType { Data = 1, Nack = 2, Ack = 3, Err = 4 }`; `struct RtaHeader { dst_ref: u16, src_ref: u16, pdu_type: PduType, tack: bool, send_seq: u16, ack_seq: u16 }` (`LEN = 12`, `parse(&[u8]) -> Result<(RtaHeader, u16 /*var_part_len*/), RtaError>`, `write(&self, out, var_part_len: u16)`); `enum AlarmType { Diagnosis = 0x0001, Process = 0x0002, Pull = 0x0003, Plug = 0x0004, Status = 0x0005, Update = 0x0006, Redundancy = 0x0007, ControlledBySupervisor = 0x0008, Released = 0x0009, PlugWrongSubmodule = 0x000A, ReturnOfSubmodule = 0x000B, DiagnosisDisappears = 0x000C, Other(u16) }` with `from_u16`/`to_u16`; `struct AlarmSpecifier { sequence: u16, channel_diag: bool, manufacturer_diag: bool, submodule_diag: bool, ar_diag: bool }` (`from_u16`/`to_u16`); `enum Maintenance { Fault = 0, Required = 1, Demanded = 2, Qualified = 3 }`; `enum Specifier { AllDisappear = 0, Appears = 1, Disappears = 2, DisappearsOthersRemain = 3 }`; `struct ChannelProperties { type_: u8, accumulative: bool, maintenance: Maintenance, specifier: Specifier, direction: u8 }` (`from_u16`/`to_u16`); `struct ChannelDiagnosis { channel: u16, properties: ChannelProperties, error_type: u16 }` (6 bytes), `struct ExtChannelDiagnosis { channel: u16, properties: ChannelProperties, error_type: u16, ext_error_type: u16, ext_add_value: u32 }` (12 bytes); `enum UserData { Channel(ChannelDiagnosis), ExtChannel(ExtChannelDiagnosis), Raw(Vec<u8>) }`; `struct AlarmNotification { alarm_type, api: u32, slot: u16, subslot: u16, module_ident: u32, submodule_ident: u32, specifier: AlarmSpecifier, usi: u16, data: UserData }`; `struct AlarmAck { alarm_type, api, slot, subslot, specifier, status: PnioStatus }`; `enum RtaData { Notification(AlarmNotification), Ack(AlarmAck), Unknown { block_type: u16, body: Vec<u8> } }`; `struct RtaPdu { priority: Priority, header: RtaHeader, body: RtaBody }`, `enum RtaBody { Data(RtaData), Ack, Nack, Err(PnioStatus) }`; `fn parse_frame(frame: &[u8]) -> Result<RtaPdu, RtaError>`; `fn build_frame(dst: MacAddr, src: MacAddr, pdu: &RtaPdu) -> Vec<u8>`; `fn is_alarm_frame(frame: &[u8]) -> bool` (tagged or untagged `0x8892` with FrameID `0xFC01`/`0xFE01`); `enum RtaError { Eth(EthError), NotAlarm, TooShort, BadPduType(u8), BadVersion(u8), Block(BlockError), BadVarPartLen { declared: u16, available: usize } }` (thiserror).

Layout recap for the implementer: frame = `EthHeader` (dst, src, VLAN `Some(tci)`, `0x8892`) + FrameID u16 + RTA header (`dst_ref u16, src_ref u16, (version<<4 | type) u8, (tack<<4 | window 1) u8, send_seq u16, ack_seq u16, var_part_len u16`) + var part. DATA var part = one PNIO block: `BlockHeader` (type `0x0001`/`0x0002` notification High/Low, `0x8001`/`0x8002` ack High/Low; version 1.0) + body. Notification body: `alarm_type u16, api u32, slot u16, subslot u16, module_ident u32, submodule_ident u32, specifier u16, usi u16, data…`. Ack body: `alarm_type u16, api u32, slot u16, subslot u16, specifier u16, status u32`. `ChannelDiagnosis` = `channel u16, properties u16, error_type u16`; `ExtChannelDiagnosis` adds `ext_error_type u16, ext_add_value u32`. `UserData::Channel` is produced for USI `0x8000` with a 6-byte payload, `ExtChannel` for `0x8002` with 12 bytes, `Raw` otherwise. `build_frame` does **not** pad to 60 bytes (the goldens are unpadded sender-side captures: `alarm_ack_rta_high_dev` is 32 bytes; the NIC pads).

- [ ] **Step 1: Write the failing tests** — `src/alarm/rta.rs` `#[cfg(test)] mod tests`:

```rust
use super::*;
use crate::testutil::golden_alarm;

const DEV: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);
const CPU: MacAddr = MacAddr([0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3c]);

#[test]
fn process_notification_parses_and_rebuilds_byte_exact() {
    let g = golden_alarm("alarm_process_notif");
    let pdu = parse_frame(&g).unwrap();
    assert_eq!(pdu.priority, Priority::High);
    assert_eq!(pdu.header, RtaHeader { dst_ref: 0, src_ref: 0, pdu_type: PduType::Data, tack: true, send_seq: 0xFFFF, ack_seq: 0xFFFE });
    let RtaBody::Data(RtaData::Notification(n)) = &pdu.body else { panic!("not a notification") };
    assert_eq!(n.alarm_type, AlarmType::Process);
    assert_eq!((n.api, n.slot, n.subslot), (0, 1, 1));
    assert_eq!((n.module_ident, n.submodule_ident), (0x30, 0x130));
    assert_eq!(n.specifier, AlarmSpecifier { sequence: 0, channel_diag: false, manufacturer_diag: false, submodule_diag: false, ar_diag: false });
    assert_eq!(n.usi, 0x0010);
    assert_eq!(n.data, UserData::Raw(vec![0x01]));
    assert_eq!(build_frame(CPU, DEV, &pdu), g);
}

#[test]
fn diagnosis_notification_parses_ext_channel_and_rebuilds() {
    let g = golden_alarm("alarm_diag_notif");
    let pdu = parse_frame(&g).unwrap();
    assert_eq!(pdu.priority, Priority::Low);
    let RtaBody::Data(RtaData::Notification(n)) = &pdu.body else { panic!() };
    assert_eq!(n.alarm_type, AlarmType::Diagnosis);
    assert_eq!(n.specifier, AlarmSpecifier { sequence: 0, channel_diag: true, manufacturer_diag: false, submodule_diag: true, ar_diag: true });
    assert_eq!(n.usi, USI_EXT_CHANNEL_DIAG);
    let UserData::ExtChannel(d) = &n.data else { panic!() };
    assert_eq!(d.channel, 4);
    assert_eq!(d.properties, ChannelProperties { type_: 1, accumulative: false, maintenance: Maintenance::Fault, specifier: Specifier::Appears, direction: 1 });
    assert_eq!(d.properties.to_u16(), 0x2801);
    assert_eq!(d.error_type, 0x0001);
    assert_eq!((d.ext_error_type, d.ext_add_value), (0, 0));
    assert_eq!(build_frame(CPU, DEV, &pdu), g);
}

#[test]
fn channel_properties_bit_fields_round_trip() {
    for raw in [0x2801u16, 0x3801, 0x2001, 0x0000, 0x6A00] {
        assert_eq!(ChannelProperties::from_u16(raw).to_u16(), raw);
    }
    assert_eq!(ChannelProperties::from_u16(0x3801).specifier, Specifier::DisappearsOthersRemain);
    assert_eq!(ChannelProperties::from_u16(0x2001).specifier, Specifier::AllDisappear);
}

#[test]
fn cpu_ack_rta_and_alarm_ack_parse() {
    let ack = parse_frame(&golden_alarm("alarm_ack_rta_high_cpu")).unwrap();
    assert_eq!(ack.body, RtaBody::Ack);
    assert_eq!((ack.header.send_seq, ack.header.ack_seq, ack.header.tack), (0xFFFE, 0xFFFF, false));
    let aa = parse_frame(&golden_alarm("alarm_ack_high_cpu")).unwrap();
    let RtaBody::Data(RtaData::Ack(a)) = &aa.body else { panic!() };
    assert_eq!(a.alarm_type, AlarmType::Process);
    assert_eq!((a.slot, a.subslot), (1, 1));
    assert_eq!(a.status, PnioStatus::OK);
    assert_eq!((aa.header.send_seq, aa.header.ack_seq), (0xFFFF, 0xFFFF));
}

#[test]
fn our_ack_rta_rebuilds_byte_exact() {
    for name in ["alarm_ack_rta_high_dev", "alarm_ack_rta_low_dev"] {
        let g = golden_alarm(name);
        let pdu = parse_frame(&g).unwrap();
        assert_eq!(pdu.body, RtaBody::Ack);
        assert_eq!(build_frame(CPU, DEV, &pdu), g, "{name}");
    }
}

#[test]
fn err_rta_both_ways() {
    let dev = parse_frame(&golden_alarm("alarm_err_rta_dev")).unwrap();
    assert_eq!(dev.body, RtaBody::Err(PnioStatus::new(0xCF, 0x81, 0xFD, 0x00)));
    assert_eq!((dev.header.send_seq, dev.header.ack_seq), (6, 5));
    assert_eq!(build_frame(CPU, DEV, &dev), golden_alarm("alarm_err_rta_dev"));
    let cpu = parse_frame(&golden_alarm("alarm_err_rta_cpu")).unwrap();
    assert_eq!(cpu.body, RtaBody::Err(PnioStatus::new(0xCF, 0x81, 0xFD, 0x0B)));
    let removed = parse_frame(&golden_alarm("alarm_err_rta_cpu_removed")).unwrap();
    assert_eq!(removed.body, RtaBody::Err(PnioStatus::new(0xCF, 0x81, 0xFD, 0x11)));
    assert_eq!((removed.header.send_seq, removed.header.ack_seq), (0xFFFE, 0xFFFE));
}

#[test]
fn disappears_and_std_remove_goldens_rebuild() {
    for name in ["alarm_diag_usi_disappears", "alarm_diag_std_remove", "alarm_diag_update_appears", "alarm_diag_update_others_remain"] {
        let g = golden_alarm(name);
        let pdu = parse_frame(&g).unwrap();
        assert_eq!(build_frame(CPU, DEV, &pdu), g, "{name}");
    }
    let RtaBody::Data(RtaData::Notification(n)) = parse_frame(&golden_alarm("alarm_diag_usi_disappears")).unwrap().body else { panic!() };
    assert_eq!(n.alarm_type, AlarmType::DiagnosisDisappears);
    assert_eq!(n.usi, 0x1234);
    assert_eq!(n.specifier.sequence, 5);
}

#[test]
fn is_alarm_frame_discriminates() {
    assert!(is_alarm_frame(&golden_alarm("alarm_process_notif")));
    assert!(!is_alarm_frame(&golden_alarm("im0_read_req")));
    assert!(!is_alarm_frame(&crate::testutil::golden("dcp_set_req")));
}

#[test]
fn malformed_frames_are_errors_not_panics() {
    let g = golden_alarm("alarm_process_notif");
    assert!(matches!(parse_frame(&g[..25]), Err(RtaError::TooShort)));
    let mut bad = g.clone();
    bad[24] = 0x15; // pdu type 5
    assert!(matches!(parse_frame(&bad), Err(RtaError::BadPduType(5))));
    let mut short = g.clone();
    short[30] = 0x00; short[31] = 0x40; // var_part_len 64 > available
    assert!(matches!(parse_frame(&short), Err(RtaError::BadVarPartLen { .. })));
}
```

- [ ] **Step 2: Run** `cargo test -p pnio alarm::rta` → FAIL to compile (module missing).

- [ ] **Step 3: Implement** `src/alarm/mod.rs`:

```rust
//! PROFINET alarm channel (RTA over `0x8892`, FrameIDs `0xFC01` High / `0xFE01` Low):
//! the codec (`rta`) and the one-alarm-in-flight sender/receiver state machine
//! (`channel`). Pure: no sockets, no clock — the device loop drives both.
pub mod channel;
pub mod rta;

pub use channel::{AlarmAction, AlarmChannel, AlarmChannelConfig, AlarmError, AlarmReq, AlarmStats};
pub use rta::*;
```
(`channel` is created in Task 3; for this task leave `pub mod channel;` out and add it in Task 3.)

`src/alarm/rta.rs` — write the types listed in **Interfaces** and:

```rust
impl RtaHeader {
    pub const LEN: usize = 12;
    pub fn parse(buf: &[u8]) -> Result<(RtaHeader, u16), RtaError> {
        if buf.len() < Self::LEN { return Err(RtaError::TooShort); }
        let u16at = |o: usize| u16::from_be_bytes([buf[o], buf[o + 1]]);
        let (version, ty) = (buf[4] >> 4, buf[4] & 0x0F);
        if version != 1 { return Err(RtaError::BadVersion(version)); }
        let pdu_type = match ty { 1 => PduType::Data, 2 => PduType::Nack, 3 => PduType::Ack, 4 => PduType::Err, t => return Err(RtaError::BadPduType(t)) };
        Ok((RtaHeader { dst_ref: u16at(0), src_ref: u16at(2), pdu_type, tack: buf[5] & 0x10 != 0, send_seq: u16at(6), ack_seq: u16at(8) }, u16at(10)))
    }
    pub fn write(&self, out: &mut Vec<u8>, var_part_len: u16) {
        out.extend_from_slice(&self.dst_ref.to_be_bytes());
        out.extend_from_slice(&self.src_ref.to_be_bytes());
        out.push(0x10 | self.pdu_type as u8);
        out.push(0x01 | if self.tack { 0x10 } else { 0 }); // window size 1
        out.extend_from_slice(&self.send_seq.to_be_bytes());
        out.extend_from_slice(&self.ack_seq.to_be_bytes());
        out.extend_from_slice(&var_part_len.to_be_bytes());
    }
}
```

`parse_frame`: `EthHeader::parse` → require `ethertype == ETHERTYPE_PROFINET`; FrameID at the payload offset → `Priority` or `NotAlarm`; `RtaHeader::parse` on the rest; `var_part_len` must be `<= remaining` else `BadVarPartLen`; body by `pdu_type`: `Ack`/`Nack` → unit; `Err` → `PnioStatus(u32 BE)` (needs 4 bytes); `Data` → `BlockHeader::parse` then by block type: `0x0001|0x0002` → notification (use `Cursor`; `data` = remaining bytes; wrap into `UserData` by USI/length), `0x8001|0x8002` → ack, other → `Unknown`. `build_frame`: `EthHeader { dst, src, vlan: Some(tci), ethertype: 0x8892 }.write` + FrameID + body bytes computed first (so `var_part_len` is known) + header. Block lengths: notification body length = `2 (version) + 2+4+2+2+4+4+2+2 + data.len()`; ack = `2 + 2+4+2+2+2+4 = 18`. `AlarmSpecifier::to_u16`: `(sequence & 0x07FF) | channel<<11 | manufacturer<<12 | submodule<<13 | ar<<15`. `ChannelProperties::to_u16`: `type_ as u16 | accumulative<<8 | maintenance<<9 | specifier<<11 | (direction as u16)<<13`. Add `pub mod alarm;` to `src/lib.rs` (alphabetical, before `api`).

- [ ] **Step 4: Run** `cargo test -p pnio alarm::rta` → PASS; `cargo clippy --all-targets -- -D warnings`; `cargo fmt --all`.

- [ ] **Step 5: Commit**: `git add crates/pnio/src/alarm crates/pnio/src/lib.rs && git commit -m "feat(alarm): RTA-PDU codec — notification/ack/err frames, byte-exact against the p-net goldens"`

---

### Task 3: `alarm::channel` — sender/receiver state machine

**Files:**
- Create: `crates/pnio/src/alarm/channel.rs`
- Modify: `crates/pnio/src/alarm/mod.rs` (add `pub mod channel;` + re-exports)
- Modify: `crates/pnio/src/cm/ar.rs` (new `AbortReason` variants — needed by the actions)
- Modify: `crates/pnio/src/cm/status.rs` (RTA status constructors)

**Interfaces:**
- Consumes: Task 2 types; `crate::cm::{AbortReason, PnioStatus}`.
- Produces:

```rust
// cm/status.rs
impl PnioStatus {
    /// `CF 81 FD xx`: RTA error, PNIO, RTA_ERR_CLS_PROTOCOL, `code2` per spec §4.3.
    pub fn rta_abort(code2: u8) -> PnioStatus { PnioStatus::new(0xCF, 0x81, 0xFD, code2) }
    pub const RTA_ABORT_DHT_EXPIRED: u8 = 1;
    pub const RTA_ABORT_ALARM_SEND_FAILED: u8 = 3;
    pub const RTA_ABORT_DHT_WDT_EXPIRED: u8 = 5;
    pub const RTA_ABORT_ALARM_IND_ERR: u8 = 11;
    pub const RTA_ABORT_AR_REMOVED: u8 = 17;
}
// cm/ar.rs — AbortReason gains:
    /// The controller sent an ERR-RTA on the alarm channel.
    ControllerErrRta(PnioStatus),
    /// An alarm we sent was never acknowledged within `rta_retries` resends.
    AlarmSendFailed,
    /// The RT thread's socket failed (was reported as `RtWatchdog` before Plan 5).
    RtSocket,
    /// The application stopped the device (`IoDevice::stop`).
    Shutdown,
// alarm/channel.rs
pub struct AlarmChannelConfig { pub local_ref: u16, pub remote_ref: u16, pub rta_timeout: Duration, pub rta_retries: u16, pub max_alarm_data_length: u16, pub peer_mac: MacAddr, pub our_mac: MacAddr }
pub struct AlarmReq { pub id: u32, pub priority: Priority, pub notification: AlarmNotification }
#[derive(Debug, Clone, PartialEq)]
pub enum AlarmAction { Send(Vec<u8>), Acked { id: u32, status: PnioStatus }, Abort(AbortReason), UnexpectedRx }
#[derive(Debug, Error, PartialEq)] pub enum AlarmError { #[error("alarm data {len} bytes exceeds the negotiated {max}")] TooLong { len: usize, max: u16 } }
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AlarmStats { pub sent: u64, pub acked: u64, pub retries: u64, pub unexpected_rx: u64, pub send_failures: u64, pub rx_err_rta: u64 }
pub struct AlarmChannel { /* private */ }
impl AlarmChannel {
    pub fn new(cfg: AlarmChannelConfig) -> Self;
    pub fn enqueue(&mut self, req: AlarmReq, now: Instant) -> Result<Vec<AlarmAction>, AlarmError>;
    pub fn on_frame(&mut self, frame: &[u8], now: Instant) -> Vec<AlarmAction>;
    pub fn on_tick(&mut self, now: Instant) -> Vec<AlarmAction>;
    pub fn next_deadline(&self) -> Option<Instant>;
    pub fn err_rta(&mut self, status: PnioStatus) -> Vec<u8>;
    pub fn in_flight(&self) -> Option<u32>;
    pub fn queued(&self) -> usize;
    pub fn stats(&self) -> AlarmStats;
    pub fn next_specifier_sequence(&mut self) -> u16;  // 0,1,2,… wrapping at 0x7FF — the device fills AlarmSpecifier.sequence with it before enqueue
}
```

Semantics (spec §5.2): `next_send_seq` starts `SEQ_INIT`; after a DATA is sent, `last_sent_seq = that seq`, `next_send_seq = if seq == 0xFFFF { 0 } else { (seq + 1) & 0x7FFF }`. `last_rx_seq` starts `SEQ_NONE`. Our DATA: `send_seq = next`, `ack_seq = last_rx_seq`, `tack = true`. Our ACK-RTA: `send_seq = last_sent_seq` (init `SEQ_NONE`), `ack_seq = the received DATA's send_seq`, `tack = false`. ERR-RTA: `send_seq = last_sent_seq`, `ack_seq = last_rx_seq`, Low priority. States: `Idle`, `SentData { req, seq, attempt, deadline }`, `AwaitAlarmAck { req, deadline }`. `enqueue`: `TooLong` if notification body length (block body: 24 + data len) `> max_alarm_data_length`; push to queue; if `Idle`, pop and send (`Send`). `on_frame`: parse (parse error → `UnexpectedRx`, count); source MAC ≠ `peer_mac` → `UnexpectedRx`; `Ack` with `ack_seq == seq` in `SentData` → `AwaitAlarmAck { deadline: now + rta_timeout }`; `Ack` otherwise → ignored (count); `Data`: if `send_seq == last_rx_seq` → re-send our ACK only; else `last_rx_seq = send_seq`, emit ACK-RTA, then if `RtaData::Ack(a)` and state is `AwaitAlarmAck` with matching `alarm_type/slot/subslot` → `Acked { id, status }`, state → `Idle`, pop next queued (another `Send`); any other DATA → `UnexpectedRx`; `Err(status)` → `stats.rx_err_rta += 1`, `Abort(ControllerErrRta(status))`, state → `Idle`, queue cleared; `Nack` → `UnexpectedRx`. `on_tick`: past `deadline` in `SentData`: `attempt < rta_retries` → resend same frame (`retries += 1`), `attempt += 1`, new deadline; else `send_failures += 1`, `Abort(AlarmSendFailed)`, `Idle`, queue cleared. Past deadline in `AwaitAlarmAck`: same policy (treat as a resend of the DATA — the CPU re-acks). `err_rta` builds the frame and returns it (no state change besides counters).

- [ ] **Step 1: Write the failing tests** (`src/alarm/channel.rs` tests):

```rust
use super::*;
use crate::alarm::rta::*;
use crate::cm::PnioStatus;
use crate::testutil::golden_alarm;
use std::time::{Duration, Instant};

const DEV: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);
const CPU: MacAddr = MacAddr([0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3c]);

fn cfg() -> AlarmChannelConfig {
    AlarmChannelConfig { local_ref: 0, remote_ref: 0, rta_timeout: Duration::from_millis(100), rta_retries: 3, max_alarm_data_length: 256, peer_mac: CPU, our_mac: DEV }
}
fn process_req(id: u32) -> AlarmReq {
    AlarmReq { id, priority: Priority::High, notification: AlarmNotification { alarm_type: AlarmType::Process, api: 0, slot: 1, subslot: 1, module_ident: 0x30, submodule_ident: 0x130, specifier: AlarmSpecifier::default(), usi: 0x0010, data: UserData::Raw(vec![1]) } }
}
fn sends(actions: &[AlarmAction]) -> Vec<Vec<u8>> {
    actions.iter().filter_map(|a| if let AlarmAction::Send(f) = a { Some(f.clone()) } else { None }).collect()
}

#[test]
fn full_handshake_reproduces_the_process_alarm_goldens() {
    let t0 = Instant::now();
    let mut ch = AlarmChannel::new(cfg());
    let out = ch.enqueue(process_req(7), t0).unwrap();
    assert_eq!(sends(&out), vec![golden_alarm("alarm_process_notif")]);
    assert_eq!(ch.in_flight(), Some(7));
    let out = ch.on_frame(&golden_alarm("alarm_ack_rta_high_cpu"), t0);
    assert!(out.is_empty(), "transport ack produces nothing to send");
    let out = ch.on_frame(&golden_alarm("alarm_ack_high_cpu"), t0);
    assert_eq!(sends(&out), vec![golden_alarm("alarm_ack_rta_high_dev")]);
    assert!(out.contains(&AlarmAction::Acked { id: 7, status: PnioStatus::OK }));
    assert_eq!(ch.in_flight(), None);
    assert_eq!(ch.stats().sent, 1);
    assert_eq!(ch.stats().acked, 1);
}

#[test]
fn queue_is_fifo_and_one_in_flight() {
    let t0 = Instant::now();
    let mut ch = AlarmChannel::new(cfg());
    let a = ch.enqueue(process_req(1), t0).unwrap();
    let b = ch.enqueue(process_req(2), t0).unwrap();
    assert_eq!(sends(&a).len(), 1);
    assert!(sends(&b).is_empty());
    assert_eq!(ch.queued(), 1);
    ch.on_frame(&golden_alarm("alarm_ack_rta_high_cpu"), t0);
    let out = ch.on_frame(&golden_alarm("alarm_ack_high_cpu"), t0);
    // our ACK-RTA for the CPU's ack, then the second notification (send_seq 0)
    let s = sends(&out);
    assert_eq!(s.len(), 2);
    let second = parse_frame(&s[1]).unwrap();
    assert_eq!((second.header.send_seq, second.header.ack_seq), (0x0000, 0xFFFF));
    assert_eq!(ch.in_flight(), Some(2));
}

#[test]
fn retries_then_aborts_when_never_acked() {
    let t0 = Instant::now();
    let mut ch = AlarmChannel::new(cfg());
    ch.enqueue(process_req(1), t0).unwrap();
    let mut resends = 0;
    let mut t = t0;
    let mut aborted = None;
    for _ in 0..5 {
        t += Duration::from_millis(101);
        let out = ch.on_tick(t);
        resends += sends(&out).len();
        if let Some(AlarmAction::Abort(r)) = out.iter().find(|a| matches!(a, AlarmAction::Abort(_))) { aborted = Some(r.clone()); break; }
    }
    assert_eq!(resends, 3);
    assert_eq!(aborted, Some(crate::cm::AbortReason::AlarmSendFailed));
    assert_eq!(ch.stats().retries, 3);
    assert_eq!(ch.stats().send_failures, 1);
    assert_eq!(ch.in_flight(), None);
}

#[test]
fn duplicate_data_is_re_acked_but_not_reprocessed() {
    let t0 = Instant::now();
    let mut ch = AlarmChannel::new(cfg());
    ch.enqueue(process_req(1), t0).unwrap();
    ch.on_frame(&golden_alarm("alarm_ack_rta_high_cpu"), t0);
    let first = ch.on_frame(&golden_alarm("alarm_ack_high_cpu"), t0);
    let again = ch.on_frame(&golden_alarm("alarm_ack_high_cpu"), t0);
    assert_eq!(sends(&again), vec![golden_alarm("alarm_ack_rta_high_dev")]);
    assert!(!again.iter().any(|a| matches!(a, AlarmAction::Acked { .. })));
    assert!(first.iter().any(|a| matches!(a, AlarmAction::Acked { .. })));
}

#[test]
fn controller_err_rta_aborts() {
    let t0 = Instant::now();
    let mut ch = AlarmChannel::new(cfg());
    ch.enqueue(process_req(1), t0).unwrap();
    let out = ch.on_frame(&golden_alarm("alarm_err_rta_cpu_removed"), t0);
    assert_eq!(out, vec![AlarmAction::Abort(crate::cm::AbortReason::ControllerErrRta(PnioStatus::new(0xCF, 0x81, 0xFD, 0x11)))]);
    assert_eq!(ch.in_flight(), None);
    assert_eq!(ch.queued(), 0);
}

#[test]
fn err_rta_out_uses_current_counters_and_low_priority() {
    let mut ch = AlarmChannel::new(cfg());
    let f = ch.err_rta(PnioStatus::rta_abort(PnioStatus::RTA_ABORT_AR_REMOVED));
    let pdu = parse_frame(&f).unwrap();
    assert_eq!(pdu.priority, Priority::Low);
    assert_eq!((pdu.header.send_seq, pdu.header.ack_seq), (SEQ_NONE, SEQ_NONE));
    assert_eq!(pdu.body, RtaBody::Err(PnioStatus::new(0xCF, 0x81, 0xFD, 17)));
}

#[test]
fn too_long_is_refused_before_the_wire() {
    let mut c = cfg(); c.max_alarm_data_length = 30;
    let mut ch = AlarmChannel::new(c);
    let mut r = process_req(1); r.notification.data = UserData::Raw(vec![0; 40]);
    assert!(matches!(ch.enqueue(r, Instant::now()), Err(AlarmError::TooLong { .. })));
}

#[test]
fn frames_from_another_mac_and_garbage_are_unexpected() {
    let t0 = Instant::now();
    let mut ch = AlarmChannel::new(cfg());
    let mut f = golden_alarm("alarm_ack_high_cpu"); f[6] ^= 0xFF;
    assert_eq!(ch.on_frame(&f, t0), vec![AlarmAction::UnexpectedRx]);
    assert_eq!(ch.on_frame(&[0u8; 10], t0), vec![AlarmAction::UnexpectedRx]);
    assert_eq!(ch.stats().unexpected_rx, 2);
}
```
(`AlarmSpecifier` needs `#[derive(Default)]` — add it in `rta.rs`.)

- [ ] **Step 2: Run** `cargo test -p pnio alarm::channel` → FAIL (compile).
- [ ] **Step 3: Implement** per the semantics above (`PnioStatus::rta_abort` + consts, `AbortReason` variants — update every exhaustive `match` on `AbortReason` the compiler flags, e.g. in `api.rs`/examples, keeping behaviour), then `channel.rs`. The first notification's frame must equal `alarm_process_notif` byte-for-byte: `dst = peer_mac`, `src = our_mac`, `dst_ref = remote_ref`, `src_ref = local_ref`.
- [ ] **Step 4: Run** `cargo test -p pnio` (whole crate: `AbortReason` matches) → PASS; clippy; fmt.
- [ ] **Step 5: Commit**: `git commit -am "feat(alarm): one-in-flight RTA sender/receiver with retries, dedup and ERR-RTA both ways"` (use `git add` for the new file).

---

### Task 4: `diag` — diagnosis store

**Files:**
- Create: `crates/pnio/src/diag.rs`; Modify: `src/lib.rs` (`pub mod diag;`)

**Interfaces:**
- Consumes: `crate::alarm::{AlarmNotification, AlarmSpecifier, AlarmType, ChannelDiagnosis, ChannelProperties, Maintenance, Specifier, UserData, USI_CHANNEL_DIAG}`, `crate::config::{Slot, Direction}`.
- Produces:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u16)]
pub enum ChannelError { ShortCircuit = 0x0001, Undervoltage = 0x0002, Overvoltage = 0x0003, Overload = 0x0004, Overtemperature = 0x0005, LineBreak = 0x0006, UpperLimitExceeded = 0x0007, LowerLimitExceeded = 0x0008, Error = 0x0009 }
impl ChannelError { pub fn code(self) -> u16; pub fn from_code(c: u16) -> Option<ChannelError>; pub fn from_name(s: &str) -> Option<ChannelError> /* "short-circuit", "undervoltage", … "line-break", "upper-limit", "lower-limit", "error" */ }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Severity { Fault, MaintenanceRequired, MaintenanceDemanded }
pub const WHOLE_SUBMODULE: u16 = 0x8000;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnosis { pub slot: Slot, pub channel: u16, pub error: ChannelError, pub severity: Severity, pub direction: Direction }
/// What the store needs to know about one submodule to build notifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmoduleInfo { pub slot: Slot, pub subslot: u16, pub module_ident: u32, pub submodule_ident: u32, pub direction: Direction }
pub struct DiagStore { /* submodules: Vec<SubmoduleInfo>, active: BTreeMap<(u16 /*slot*/, u16 /*channel*/, u16 /*error code*/), Diagnosis> */ }
impl DiagStore {
    pub fn new(submodules: Vec<SubmoduleInfo>) -> Self;
    pub fn from_model(model: &crate::cm::DeviceModel) -> Self;  // slots > 0, subslot 1: direction Input if output_len == 0 && input_len > 0, Output if the reverse, InputOutput otherwise
    pub fn knows(&self, slot: Slot) -> bool;
    pub fn raise(&mut self, d: Diagnosis) -> Option<AlarmNotification>;
    pub fn clear(&mut self, slot: Slot, channel: u16, error: ChannelError) -> Option<AlarmNotification>;
    pub fn problem_indicator(&self) -> bool;
    pub fn active(&self) -> Vec<Diagnosis>;
    pub fn replay(&self) -> Vec<AlarmNotification>;
}
```

Notification building: `alarm_type` = `Diagnosis` for raise/update/replay, `DiagnosisDisappears` for clear; `api 0`, `slot`, `subslot 1`, idents from `SubmoduleInfo`; `specifier` = `AlarmSpecifier { sequence: 0 /* the device fills it */, channel_diag: any diagnosis remains on that submodule after the change, manufacturer_diag: false, submodule_diag: same as channel_diag, ar_diag: any diagnosis remains anywhere }`; `usi = USI_CHANNEL_DIAG`; `data = UserData::Channel(ChannelDiagnosis { channel, properties: ChannelProperties { type_: 0, accumulative: false, maintenance: Fault→Fault / MaintenanceRequired→Required / MaintenanceDemanded→Demanded, specifier: Appears (raise/update/replay) | Disappears (clear, nothing else left on that channel) | DisappearsOthersRemain (clear, other errors remain on the same channel), direction: Input→1, Output→2, InputOutput→3 }, error_type: error.code() })`. `raise` of an identical entry → `None`; different severity on the same key → replace, `Some(appears)`. `clear` of an absent key → `None`. `problem_indicator` = any `Severity::Fault`.

- [ ] **Step 1: Write the failing tests** (`src/diag.rs` tests):

```rust
use super::*;
use crate::alarm::*;
use crate::config::{Direction, Slot};

fn store() -> DiagStore {
    DiagStore::new(vec![
        SubmoduleInfo { slot: Slot(1), subslot: 1, module_ident: 0x101, submodule_ident: 1, direction: Direction::Input },
        SubmoduleInfo { slot: Slot(3), subslot: 1, module_ident: 0x103, submodule_ident: 1, direction: Direction::Output },
    ])
}
fn d(slot: u16, ch: u16, e: ChannelError, s: Severity) -> Diagnosis {
    Diagnosis { slot: Slot(slot), channel: ch, error: e, severity: s, direction: Direction::Input }
}
fn chan(n: &AlarmNotification) -> ChannelDiagnosis { match &n.data { UserData::Channel(c) => c.clone(), _ => panic!() } }

#[test]
fn raise_builds_a_channel_diagnosis_appears() {
    let mut s = store();
    let n = s.raise(d(1, 0, ChannelError::LineBreak, Severity::Fault)).unwrap();
    assert_eq!(n.alarm_type, AlarmType::Diagnosis);
    assert_eq!((n.slot, n.subslot, n.module_ident, n.submodule_ident), (1, 1, 0x101, 1));
    assert_eq!(n.usi, USI_CHANNEL_DIAG);
    let c = chan(&n);
    assert_eq!(c.error_type, 0x0006);
    assert_eq!(c.properties, ChannelProperties { type_: 0, accumulative: false, maintenance: Maintenance::Fault, specifier: Specifier::Appears, direction: 1 });
    assert!(n.specifier.channel_diag && n.specifier.submodule_diag && n.specifier.ar_diag);
    assert!(s.problem_indicator());
}

#[test]
fn identical_raise_is_noop_and_severity_change_is_update() {
    let mut s = store();
    s.raise(d(1, 0, ChannelError::LineBreak, Severity::Fault));
    assert!(s.raise(d(1, 0, ChannelError::LineBreak, Severity::Fault)).is_none());
    let n = s.raise(d(1, 0, ChannelError::LineBreak, Severity::MaintenanceRequired)).unwrap();
    assert_eq!(chan(&n).properties.maintenance, Maintenance::Required);
    assert!(!s.problem_indicator(), "maintenance-required is not a fault");
    assert_eq!(s.active().len(), 1);
}

#[test]
fn clear_builds_disappears_and_clears_flags_when_last() {
    let mut s = store();
    s.raise(d(1, 0, ChannelError::LineBreak, Severity::Fault));
    s.raise(d(1, 0, ChannelError::Overload, Severity::Fault));
    let n = s.clear(Slot(1), 0, ChannelError::LineBreak).unwrap();
    assert_eq!(n.alarm_type, AlarmType::DiagnosisDisappears);
    assert_eq!(chan(&n).properties.specifier, Specifier::DisappearsOthersRemain);
    assert!(n.specifier.channel_diag);
    let n = s.clear(Slot(1), 0, ChannelError::Overload).unwrap();
    assert_eq!(chan(&n).properties.specifier, Specifier::Disappears);
    assert!(!n.specifier.channel_diag && !n.specifier.ar_diag);
    assert!(!s.problem_indicator());
    assert!(s.clear(Slot(1), 0, ChannelError::Overload).is_none());
}

#[test]
fn output_submodule_direction_and_replay() {
    let mut s = store();
    s.raise(d(3, WHOLE_SUBMODULE, ChannelError::Error, Severity::Fault));
    s.raise(d(1, 2, ChannelError::ShortCircuit, Severity::MaintenanceDemanded));
    let r = s.replay();
    assert_eq!(r.len(), 2);
    let out = r.iter().find(|n| n.slot == 3).unwrap();
    assert_eq!(chan(out).properties.direction, 2);
    assert_eq!(chan(out).channel, WHOLE_SUBMODULE);
    assert!(r.iter().all(|n| chan(n).properties.specifier == Specifier::Appears && n.specifier.ar_diag));
}

#[test]
fn from_model_derives_directions() {
    use crate::cm::{DeviceModel, SlotModel, SubmoduleModel};
    let sm = |i, o| SubmoduleModel { subslot: 1, submodule_ident: 1, input_len: i, output_len: o };
    let m = DeviceModel { vendor_id: 0xFFFF, device_id: 1, instance: 1, station_name: "x".into(), mac: crate::eth::MacAddr([0; 6]), max_alarm_data_length: 200,
        slots: vec![SlotModel { slot: 0, module_ident: 1, submodules: vec![] }, SlotModel { slot: 1, module_ident: 0x101, submodules: vec![sm(4, 0)] }, SlotModel { slot: 2, module_ident: 0x102, submodules: vec![sm(0, 4)] }, SlotModel { slot: 3, module_ident: 0x103, submodules: vec![sm(2, 2)] }] };
    let s = DiagStore::from_model(&m);
    assert!(s.knows(Slot(1)) && s.knows(Slot(3)) && !s.knows(Slot(0)) && !s.knows(Slot(9)));
    let mut s = s;
    let n1 = s.raise(d(1, 0, ChannelError::Error, Severity::Fault)).unwrap();
    let n2 = s.raise(d(2, 0, ChannelError::Error, Severity::Fault)).unwrap();
    let n3 = s.raise(d(3, 0, ChannelError::Error, Severity::Fault)).unwrap();
    assert_eq!((chan(&n1).properties.direction, chan(&n2).properties.direction, chan(&n3).properties.direction), (1, 2, 3));
}

#[test]
fn channel_error_names_and_codes() {
    assert_eq!(ChannelError::from_name("line-break"), Some(ChannelError::LineBreak));
    assert_eq!(ChannelError::from_code(0x0009), Some(ChannelError::Error));
    assert_eq!(ChannelError::from_code(0x0100), None);
}
```
(`Diagnosis.direction` is overwritten by the store from `SubmoduleInfo`; the API layer passes whatever.)

- [ ] **Step 2: Run** `cargo test -p pnio diag::` → FAIL. **Step 3: Implement** `src/diag.rs` per the rules; `pub mod diag;` in `lib.rs`. **Step 4:** tests PASS, clippy, fmt. **Step 5: Commit** `feat(diag): channel-diagnosis store producing appears/disappears notifications and the problem indicator`.

---

### Task 5: `im` — I&M0 encoding and the I&M1-3 store

**Files:**
- Create: `crates/pnio/src/im.rs`; Modify: `src/lib.rs` (`pub mod im;`)

**Interfaces:**
- Consumes: `crate::cm::block::BlockHeader`.
- Produces:

```rust
#[derive(Debug, Clone, PartialEq, Eq)] pub struct SwRevision { pub prefix: char /* 'V' */, pub functional: u8, pub bug_fix: u8, pub internal: u8 }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Im0 { pub order_id: String, pub serial_number: String, pub hardware_revision: u16, pub software_revision: SwRevision, pub revision_counter: u16, pub profile_id: u16, pub profile_specific_type: u16 }
impl Im0 { pub fn validate(&self) -> Result<(), ImError>; /* ASCII only; order_id ≤ 20, serial ≤ 16, prefix in "VRPUT" */ }
impl Default for Im0 { /* order_id "pnio device", serial_number "", hw 1, sw V0.1.0, counter 0, profile 0/0 */ }
#[derive(Debug, Error, PartialEq, Eq)] pub enum ImError { #[error("{field} is not ASCII")] NotAscii { field: &'static str }, #[error("{field} longer than {max} bytes")] TooLong { field: &'static str, max: usize }, #[error("bad software revision prefix {0:?}")] BadPrefix(char), #[error("record {index:#06x} has a bad shape: {why}")] BadRecord { index: u16, why: &'static str } }
pub const INDEX_IM0: u16 = 0xAFF0; pub const INDEX_IM1: u16 = 0xAFF1; pub const INDEX_IM2: u16 = 0xAFF2; pub const INDEX_IM3: u16 = 0xAFF3;
pub const IM_SUPPORTED_DAP: u16 = 0x000E; pub const IM_SUPPORTED_NONE: u16 = 0x0000;
pub const IM1_LEN: usize = 54; pub const IM2_LEN: usize = 16; pub const IM3_LEN: usize = 54;
pub fn encode_im0(vendor_id: u16, im0: &Im0, supported: u16) -> Vec<u8>;  // 60 bytes: header 0x0020/56/1.0 + fields, strings space-padded
#[derive(Debug, Clone, PartialEq, Eq)] pub struct ImStore { im1: [u8; 54], im2: [u8; 16], im3: [u8; 54], path: Option<std::path::PathBuf> }
impl ImStore {
    pub fn new() -> Self;                                    // all spaces
    pub fn load(path: Option<std::path::PathBuf>) -> Self;   // 124-byte file → fields; missing/short → new() + log::warn!
    pub fn read(&self, index: u16) -> Option<Vec<u8>>;       // full record with BlockHeader (0x0021/56, 0x0022/18, 0x0023/56), None for other indices
    pub fn write(&mut self, index: u16, record: &[u8]) -> Result<(), ImError>; // validates header type/length/version, stores body, persists (temp + rename), fs error → log::error!, Ok
    pub fn tag_function(&self) -> String; pub fn tag_location(&self) -> String; pub fn date(&self) -> String; pub fn descriptor(&self) -> String; // trimmed
}
```

- [ ] **Step 1: Write the failing tests**:

```rust
use super::*;
use crate::testutil::golden_alarm;

fn pnet_im0() -> Im0 {
    Im0 { order_id: "12345 Abcdefghijk".into(), serial_number: "007".into(), hardware_revision: 3, software_revision: SwRevision { prefix: 'V', functional: 0, bug_fix: 2, internal: 0 }, revision_counter: 0, profile_id: 0x1234, profile_specific_type: 0x5678 }
}

#[test]
fn im0_encoding_matches_the_pnet_read_response_record() {
    let res = golden_alarm("im0_read_res");
    // RPC header 80 + NDR response 20 + IODReadResHeader 64 = 164 bytes after the 42-byte Ethernet/IP/UDP prefix
    let record = &res[42 + 80 + 20 + 64..];
    assert_eq!(record.len(), 60);
    assert_eq!(encode_im0(0x0493, &pnet_im0(), IM_SUPPORTED_DAP), record);
}

#[test]
fn im0_validation() {
    let mut i = pnet_im0();
    i.order_id = "x".repeat(21);
    assert_eq!(i.validate(), Err(ImError::TooLong { field: "order_id", max: 20 }));
    i = pnet_im0(); i.serial_number = "é".into();
    assert_eq!(i.validate(), Err(ImError::NotAscii { field: "serial_number" }));
    i = pnet_im0(); i.software_revision.prefix = 'X';
    assert_eq!(i.validate(), Err(ImError::BadPrefix('X')));
    assert_eq!(Im0::default().validate(), Ok(()));
}

#[test]
fn store_round_trips_records_and_persists() {
    let dir = std::env::temp_dir().join(format!("pnio-im-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("im.bin");
    let mut s = ImStore::load(Some(path.clone()));
    assert_eq!(s.tag_function(), "");
    let mut rec = Vec::new();
    crate::cm::block::BlockHeader::write(&mut rec, 0x0021, 56);
    rec.extend_from_slice(format!("{:<32}{:<22}", "TEST-FUNC", "TEST-LOC").as_bytes());
    s.write(INDEX_IM1, &rec).unwrap();
    assert_eq!(s.read(INDEX_IM1).unwrap(), rec);
    assert_eq!((s.tag_function(), s.tag_location()), ("TEST-FUNC".into(), "TEST-LOC".into()));
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 124);
    let again = ImStore::load(Some(path.clone()));
    assert_eq!(again.tag_function(), "TEST-FUNC");
    assert_eq!(s.read(0xAFF0), None);
    let bad = &rec[..30];
    assert!(matches!(s.write(INDEX_IM1, bad), Err(ImError::BadRecord { index: 0xAFF1, .. })));
    let mut wrong_type = rec.clone(); wrong_type[1] = 0x22;
    assert!(matches!(s.write(INDEX_IM1, &wrong_type), Err(ImError::BadRecord { .. })));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn short_or_missing_file_is_empty_store() {
    let s = ImStore::load(Some(std::path::PathBuf::from("/nonexistent/pnio-im.bin")));
    assert_eq!(s, ImStore { path: Some("/nonexistent/pnio-im.bin".into()), ..ImStore::new() });
    assert_eq!(s.read(INDEX_IM2).unwrap().len(), 22);
}
```
(`ImStore` needs `pub path` for the struct-update test, or expose `path()` and compare fields — pick `pub path`.)

- [ ] **Step 2: Run** → FAIL. **Step 3: Implement** (`Im0` string fields padded with spaces on encode: `order_id` 20, `serial` 16; `SwRevision` written as `prefix as u8, functional, bug_fix, internal`; `IM_Version` bytes `1, 1`). **Step 4:** PASS, clippy, fmt. **Step 5: Commit** `feat(im): I&M0 record encoding (golden-exact) and a raw-bytes I&M1-3 store with file persistence`.

---

### Task 6: `cm` — Read/ReadImplicit/Write for I&M, AlarmCR params, abort plumbing

**Files:**
- Create: `crates/pnio/src/cm/records.rs`
- Modify: `crates/pnio/src/cm/block.rs` (`ty::IOD_READ_REQ_HEADER = 0x0009`, `IOD_READ_RES_HEADER = 0x8009`), `cm/connect.rs` (`ArParams` fields), `cm/mod.rs` (dispatch + `set_im`), `cm/status.rs` (`read_wrong_ar`, `write_invalid_parameter`)

**Interfaces:**
- Consumes: Task 5 (`im`), Task 3 (`AbortReason` variants).
- Produces:

```rust
// cm/connect.rs — ArParams gains (filled from req.alarm_cr in the same fn that sets alarm_ref_remote):
    pub rta_timeout_factor: u16,   // × 100 ms
    pub rta_retries: u16,
    pub alarm_ref_local: u16,      // 0 — what we answer in AlarmCRBlockRes
    pub alarm_tag_high: u16,       // 0xC000 from the request
    pub alarm_tag_low: u16,        // 0xA000
// cm/records.rs
pub struct ReadReq { pub seq: u16, pub ar_uuid: Uuid, pub api: u32, pub slot: u16, pub subslot: u16, pub index: u16, pub record_data_length: u32, pub target_ar_uuid: Uuid }
impl ReadReq { pub fn parse(blocks: &[u8]) -> Result<ReadReq, CmError>; }
pub fn build_read_res(req: &ReadReq, data: &[u8]) -> Vec<u8>;   // IODReadResHeader (0x8009, 58) + data
/// Everything the record handler needs that is not in the request.
pub struct RecordCtx<'a> { pub model: &'a DeviceModel, pub im0: &'a Im0, pub im: &'a ImStore }
/// `Some(record bytes)` for an index we serve on that (slot, subslot), `None` → "invalid index".
pub fn read_record(req: &ReadReq, ctx: &RecordCtx) -> Option<Vec<u8>>;
/// Called for every Write record with index 0xAFF1..=0xAFF3 once the AR accepted the Write; returns the per-record status.
pub fn write_im_record(r: &Record, model: &DeviceModel, im: &mut ImStore) -> PnioStatus;
// cm/mod.rs
impl Cm { pub fn set_im(&mut self, im0: Im0, store: ImStore); pub fn im_store(&self) -> &ImStore; }
```

Dispatch rules in `Cm::handle_datagram`: `Some(Opnum::Read)` → `ReadReq::parse`; if no AR or `req.ar_uuid != ctx.params.ar_uuid` → `respond(PnioStatus::read_wrong_ar())`; else `read_record` → `Some(d)` ⇒ `respond_ok(build_read_res(&req, &d))`, `None` ⇒ `respond(read_index_unsupported())`. `Some(Opnum::ReadImplicit)` → same without the AR check. `read_record`: index `0xAFF0` on any `(slot, subslot)` the model knows (`model.find`) → `encode_im0(model.vendor_id, im0, if slot == 0 && subslot == 1 { IM_SUPPORTED_DAP } else { IM_SUPPORTED_NONE })`; `0xAFF1..=0xAFF3` only on `(0, 1)` → `im.read(index)`; else `None`. Write: after `self.ar.on(Event::WriteReq(req.clone()), now)` yields a `Respond { status: OK, .. }`, for each record with index in `0xAFF1..=0xAFF3`: `write_im_record` (on `(0,1)` only; a bad record → `write_invalid_parameter()` — the Write response keeps the OK status, per-record statuses are out of scope: log at `warn!`). Add a `respond_ok(blocks)` helper next to `respond`. `Cm::new` keeps its signature; `Im0::default()` + `ImStore::new()` until `set_im` is called.

- [ ] **Step 1: Write the failing tests** (`src/cm/records.rs` tests + one in `cm/mod.rs`):

```rust
// records.rs
use super::*;
use crate::im::{Im0, ImStore, SwRevision};
use crate::testutil::{golden_alarm, RPC_OFF};

const BLOCKS: usize = RPC_OFF + 80 + 20;

#[test]
fn read_req_parses_the_cpu_im0_request() {
    let r = ReadReq::parse(&golden_alarm("im0_read_req")[BLOCKS..]).unwrap();
    assert_eq!((r.api, r.slot, r.subslot, r.index, r.record_data_length), (0, 0, 1, 0xAFF0, 0x8000));
    let i = ReadReq::parse(&golden_alarm("im0_read_req_if")[BLOCKS..]).unwrap();
    assert_eq!((i.slot, i.subslot), (0, 0x8000));
}

#[test]
fn read_res_matches_the_pnet_response_blocks() {
    let req = ReadReq::parse(&golden_alarm("im0_read_req")[BLOCKS..]).unwrap();
    let im0 = Im0 { order_id: "12345 Abcdefghijk".into(), serial_number: "007".into(), hardware_revision: 3, software_revision: SwRevision { prefix: 'V', functional: 0, bug_fix: 2, internal: 0 }, revision_counter: 0, profile_id: 0x1234, profile_specific_type: 0x5678 };
    let model = crate::cm::DeviceModel::pnet_sample(crate::eth::MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]));
    let store = ImStore::new();
    let data = read_record(&req, &RecordCtx { model: &model, im0: &im0, im: &store }).unwrap();
    assert_eq!(build_read_res(&req, &data), golden_alarm("im0_read_res")[BLOCKS..].to_vec());
}

#[test]
fn im1_only_on_the_dap_and_unknown_index_is_none() {
    let model = crate::cm::DeviceModel::pnet_sample(crate::eth::MacAddr([0; 6]));
    let (im0, store) = (Im0::default(), ImStore::new());
    let ctx = RecordCtx { model: &model, im0: &im0, im: &store };
    let mut req = ReadReq::parse(&golden_alarm("im0_read_req")[BLOCKS..]).unwrap();
    req.index = 0xAFF1;
    assert!(read_record(&req, &ctx).is_some());
    req.slot = 1;
    assert!(read_record(&req, &ctx).is_none());
    req.slot = 0; req.index = 0xF840;
    assert!(read_record(&req, &ctx).is_none());
}
```
In `cm/mod.rs` tests: drive `Cm` through the p-net goldens to `Data` (copy the setup from the existing `cm::mod` tests or `ar_replay`), then `handle_datagram(&golden_alarm("im0_read_req")[RPC_OFF..], cpu, now)` and assert the single outgoing PDU equals `golden_alarm("im0_read_res")[RPC_OFF..]` once `set_im(pnet_im0, ImStore::new())` was called (the RPC header of the response is rebuilt by `build_response_pdu` — it must match p-net's: same flags/`args_max` echo as the existing Write/PrmEnd responses; if a header field differs, compare `[RPC_OFF + 80..]` and record the delta in `docs/alarm-golden-frames.md`). Also assert a `Read` with a foreign `ar_uuid` answers `read_wrong_ar()` and that a `ReadImplicit` (patch `opnum` bytes 68..70 to `5`) answers OK from `Idle`.

- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4:** whole crate PASS (`ar_replay` untouched), clippy, fmt. **Step 5: Commit** `feat(cm): I&M0-3 Read/ReadImplicit/Write, AlarmCR parameters in ArParams`.

---

### Task 7: `rt` — station problem indicator bit

**Files:**
- Modify: `crates/pnio/src/rt/engine.rs` (`RtEngine::new` gains `problem: Arc<AtomicBool>`; `on_tick` computes the data status), `rt/runner.rs` (`RtConfig.problem_indicator: Arc<AtomicBool>`, passed to `RtEngine::new`), `rt/frame.rs` (`DataStatus::RUN_PRIMARY_VALID_PROBLEM = DataStatus(0x15)`), `device/mod.rs` (`RtConfig { problem_indicator: self.problem.clone(), .. }` — the field is created in Task 8; for this task add `problem: Arc<AtomicBool>` to `Device` initialised `false` in `new`), `tests/rt_replay.rs` (call site).

- [ ] **Step 1: Failing test** in `rt/engine.rs` tests (next to the existing `0x35` assertion at line ~444):

```rust
#[test]
fn problem_indicator_clears_bit_5_of_the_data_status() {
    let problem = Arc::new(AtomicBool::new(false));
    let mut e = RtEngine::new(layout(), DEV, CPU, Arc::new(RtStats::default()), problem.clone()); // reuse the test module's existing layout()/DEV/CPU helpers
    let inputs = vec![0u8; e.layout().input_cr.data_length as usize]; // or however the existing 0x35 test builds its input snapshot
    assert_eq!(e.on_tick(1, &inputs)[62], 0x35);
    problem.store(true, Ordering::Relaxed);
    assert_eq!(e.on_tick(1, &inputs)[62], 0x15);
    problem.store(false, Ordering::Relaxed);
    assert_eq!(e.on_tick(1, &inputs)[62], 0x35);
}
```
(Adapt the frame offset/inputs to the existing `0x35` test's helpers — the assertion index `62` is the one already used there.)

- [ ] **Step 2:** FAIL. **Step 3:** implement: in `on_tick`, `data_status: if self.problem.load(Ordering::Relaxed) { DataStatus::RUN_PRIMARY_VALID_PROBLEM } else { DataStatus::RUN_PRIMARY_VALID_OK }`. Update all `RtEngine::new` callers (`runner.rs:358`, `tests/rt_replay.rs:62` → `Arc::new(AtomicBool::new(false))`). **Step 4:** PASS, clippy, fmt. **Step 5: Commit** `feat(rt): station problem indicator in the data status from a shared atomic`.

---

### Task 8: `device` — alarm channel lifecycle, routing, diag queue, ERR-RTA, replay

**Files:**
- Modify: `crates/pnio/src/device/mod.rs`

**Interfaces:**
- Consumes: Tasks 3-7.
- Produces:

```rust
pub struct DeviceSetup { pub dcp: DcpConfig, pub model: DeviceModel, pub activity_seed: Uuid, pub rt: Option<RtOptions>, pub im0: Im0, pub im_store: Option<PathBuf> }
#[derive(Debug, Clone, PartialEq, Eq)] pub enum DiagCommand { Raise(Diagnosis), Clear { slot: Slot, channel: u16, error: ChannelError } }
/// Shared between `Device` (acyclic thread) and `IoDevice` (application).
pub struct DiagShared { pub queue: Mutex<VecDeque<DiagCommand>>, pub active: Mutex<Vec<Diagnosis>>, pub sent: AtomicU64, pub acked: AtomicU64, pub retries: AtomicU64, pub unexpected_rx: AtomicU64, pub send_failures: AtomicU64, pub rx_err_rta: AtomicU64, pub rx_no_channel: AtomicU64 }
impl<E, R> Device<E, R> {
    pub fn diag_shared(&self) -> Arc<DiagShared>;
    pub fn problem_indicator(&self) -> bool;
    pub fn alarm_in_flight(&self) -> bool;
    /// Poll bound for the caller's loop: 20 ms while an alarm is in flight or commands are queued, else `default`.
    pub fn poll_interval(&self, default: Duration) -> Duration;
    /// Announce a device-side abort on the alarm channel, then abort the AR. No-op when no AR / no channel.
    pub fn abort_with_err_rta(&mut self, reason: AbortReason, now: Instant) -> Result<(), DeviceError>;
    /// `run` calls this once the stop flag is seen: ERR-RTA "AR removed" + abort, so the CPU learns in ms.
    pub fn shutdown(&mut self, now: Instant) -> Result<(), DeviceError>;
}
```

Wiring inside `step` (order matters, see spec §5.6): (1) ETH drain: `if alarm::is_alarm_frame(&frame)` → `match &mut self.alarm { Some(ch) => actions = ch.on_frame(&frame, now), None => rx_no_channel += 1 }`, else the DCP path unchanged; (2) drain `diag_shared.queue` → `DiagStore` (`Raise` → `raise`, `Clear` → `clear`) → for each `Some(notification)`: if `self.alarm` is `Some`: `notification.specifier.sequence = ch.next_specifier_sequence()`, `ch.enqueue(AlarmReq { id, priority: Priority::Low, notification }, now)` (`Err(TooLong)` → `log::error!`, cannot happen for 6-byte payloads); publish `diag.active()` into `diag_shared.active` and `self.problem.store(diag.problem_indicator())`; (3) RPC drain unchanged; (4) `drain_rt_events`: `WatchdogExpired` → `abort_with_err_rta(AbortReason::RtWatchdog)` (ERR code2 `RTA_ABORT_DHT_WDT_EXPIRED`), `SocketError` → `abort_with_err_rta(AbortReason::RtSocket)` (code2 `RTA_ABORT_AR_REMOVED`); (5) `cm.tick`; (6) `alarm.on_tick(now)`; (7) apply every `AlarmAction`: `Send(f)` → `self.eth.send(&f)?`, `Acked` → `acked += 1`, `UnexpectedRx` → counter, `Abort(reason)` → `abort_with_err_rta(reason)` (code2 `RTA_ABORT_ALARM_SEND_FAILED` for `AlarmSendFailed`; **no** ERR-RTA for `ControllerErrRta` — the CPU already aborted — just `cm.abort`); copy `ch.stats()` into the atomics after each batch. `step`'s poll deadline also honours `alarm.next_deadline()` (min with `cm.next_deadline()`). `dispatch` on `Notify(Data, None)`: after `start_runner`, build `AlarmChannelConfig { local_ref: params.alarm_ref_local, remote_ref: params.alarm_ref_remote, rta_timeout: Duration::from_millis(100) * params.rta_timeout_factor as u32, rta_retries: params.rta_retries, max_alarm_data_length: params.max_alarm_data_length.min(200)... use the CPU's `max_alarm_data_length` from the request — add it to `ArParams` as `max_alarm_data_length_remote` in Task 6 if not already the remote value, peer_mac: params.initiator_mac, our_mac: self.setup.dcp.mac }`, `self.alarm = Some(AlarmChannel::new(cfg))`, then `for n in self.diag.replay() { … enqueue as above }`; on `Notify(Idle, Some(_))`: `self.alarm = None`. `abort_with_err_rta`: `if let Some(ch) = &mut self.alarm { let f = ch.err_rta(PnioStatus::rta_abort(code2_for(&reason))); self.eth.send(&f)?; }` then `let out = self.cm.abort(reason, now); self.dispatch(out, report)`. `shutdown`: `if self.cm.state() != Idle { abort_with_err_rta(AbortReason::Shutdown /* code2 17 */) }`. `run`: `while !stop { let w = self.poll_interval(Duration::from_millis(200)); self.step(Instant::now(), Some(w))?; } self.shutdown(Instant::now())`. `Device::new`: `cm.set_im(setup.im0.clone(), ImStore::load(setup.im_store.clone()))`, `diag: DiagStore::from_model(&setup.model)`, `problem: Arc<AtomicBool>`, `diag_shared: Arc<DiagShared>`, `alarm: None`, `next_alarm_id: u32`. `stop_runner` also `self.alarm = None`? No — the channel dies with the AR in `dispatch`, keep them separate.

- [ ] **Step 1: Write the failing tests** (`device/mod.rs` tests; reuse the module's `setup()` helper and the golden-driven bring-up used by `full_bring_up_through_the_loop`):

```rust
#[test]
fn diagnosis_raised_through_the_queue_hits_the_wire_and_the_problem_bit() {
    let (mut dev, eth) = device_in_data(); // helper to add: bring the mock device to Data with the goldens (as full_bring_up_through_the_loop does) and return (dev, eth handle for sent()/push_rx())
    let shared = dev.diag_shared();
    shared.queue.lock().unwrap().push_back(DiagCommand::Raise(Diagnosis { slot: Slot(1), channel: 0, error: ChannelError::LineBreak, severity: Severity::Fault, direction: Direction::Input }));
    dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    let sent = eth.sent();
    let last = sent.last().expect("a notification was sent");
    let pdu = pnio_alarm_parse(last); // = crate::alarm::parse_frame
    let RtaBody::Data(RtaData::Notification(n)) = pdu.body else { panic!() };
    assert_eq!(n.alarm_type, AlarmType::Diagnosis);
    assert_eq!(n.slot, 1);
    assert_eq!(pdu.header.send_seq, 0xFFFF);
    assert!(dev.problem_indicator());
    assert!(dev.alarm_in_flight());
    assert_eq!(dev.poll_interval(Duration::from_millis(200)), Duration::from_millis(20));
    assert_eq!(shared.active.lock().unwrap().len(), 1);
}

#[test]
fn controller_err_rta_aborts_and_drops_the_channel() {
    let (mut dev, eth) = device_in_data();
    eth.push_rx(golden_alarm("alarm_err_rta_cpu_removed"));
    dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    assert_eq!(dev.state(), ArState::Idle);
    assert!(!dev.alarm_in_flight());
    assert!(eth.sent().iter().all(|f| !crate::alarm::is_alarm_frame(f)), "no ERR-RTA answer to a controller abort");
}

#[test]
fn shutdown_sends_err_rta_ar_removed() {
    let (mut dev, eth) = device_in_data();
    dev.shutdown(Instant::now()).unwrap();
    let last = eth.sent().last().unwrap().clone();
    let pdu = crate::alarm::parse_frame(&last).unwrap();
    assert_eq!(pdu.body, RtaBody::Err(PnioStatus::rta_abort(PnioStatus::RTA_ABORT_AR_REMOVED)));
    assert_eq!(dev.state(), ArState::Idle);
}

#[test]
fn active_diagnosis_is_replayed_on_the_next_data() {
    let (mut dev, eth) = device_in_data();
    dev.diag_shared().queue.lock().unwrap().push_back(DiagCommand::Raise(/* as above */));
    dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    eth.push_rx(golden_alarm("alarm_err_rta_cpu_removed"));
    dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    assert_eq!(dev.state(), ArState::Idle);
    let before = eth.sent().len();
    reconnect(&mut dev, &eth); // helper: push the Connect/Write/PrmEnd/AppReady goldens again (the AR machine accepts a re-Connect from Idle)
    assert_eq!(dev.state(), ArState::Data);
    let replayed: Vec<_> = eth.sent()[before..].iter().filter(|f| crate::alarm::is_alarm_frame(f)).cloned().collect();
    assert_eq!(replayed.len(), 1);
    let RtaBody::Data(RtaData::Notification(n)) = crate::alarm::parse_frame(&replayed[0]).unwrap().body else { panic!() };
    assert_eq!(n.alarm_type, AlarmType::Diagnosis);
    assert!(dev.problem_indicator());
}

#[test]
fn alarm_frames_without_a_channel_are_counted_and_dropped() {
    let setup = setup();
    let eth = MockTransport::new();
    eth.push_rx(golden_alarm("alarm_ack_high_cpu"));
    let mut dev = Device::new(setup, eth, MockRpcTransport::new());
    dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    assert_eq!(dev.diag_shared().rx_no_channel.load(Ordering::Relaxed), 1);
}
```
Note for the implementer: `MockTransport` is `Send + Sync` and cloned by `Arc` in the existing `watchdog_event_aborts_the_ar` test — reuse that pattern for `device_in_data()` so the test keeps a handle after `Device::new` takes ownership.

- [ ] **Step 2:** FAIL. **Step 3:** implement (also `drain_rt_events` now maps `SocketError` to `AbortReason::RtSocket`, resolving the FOLLOWUPS item). **Step 4:** PASS (incl. `ar_replay`, `rt_replay`, `typed_replay` — `DeviceSetup` gained two fields: update every literal: `tests/ar_replay.rs`, `tests/rt_replay.rs`, `tests/typed_replay.rs`, examples `ar_bringup`/`rt_bringup` with `im0: Im0::default(), im_store: None`), clippy, fmt. **Step 5: Commit** `feat(device): alarm channel lifecycle, diagnosis queue, ERR-RTA on device aborts and stop, replay on reconnect`.

---

### Task 9: `config` + `gsdml` — `Im0` in the builder, GSDML I&M declarations

**Files:**
- Modify: `crates/pnio/src/config.rs`, `src/gsdml.rs`, `examples/gen_gsdml.rs`, `testdata/gsdml/sample-16real-32bool.xml`, `docs/gsdml.md`

**Interfaces:**
- Produces: `DeviceConfigBuilder::im0(self, im0: Im0) -> Self`; `DeviceConfig::im0(&self) -> &Im0`; `build()` runs `im0.validate()` → `ConfigError::Im(ImError)`; `setup()` fills `im0: self.im0.clone()` and `im_store: None` (serial default: if `im0.serial_number` is empty, `setup()` fills `format!("PNIO-{:02X}{:02X}{:02X}", mac.0[3], mac.0[4], mac.0[5])`); `GsdmlMeta` loses `order_number` (the DAP/module `OrderNumber`, `HardwareRelease`, `SoftwareRelease` now come from `cfg.im0()`: `order_id`, `format!("{}", hardware_revision)`, `format!("{}{}.{}.{}", prefix, functional, bug_fix, internal)`), DAP `VirtualSubmoduleItem` gains `Writeable_IM_Records="1 2 3"`.

- [ ] **Step 1: Failing tests**: in `config.rs` — `builder_accepts_im0_and_rejects_bad_ones` (`.im0(Im0 { order_id: "x".repeat(21), ..Default::default() }).build()` → `Err(ConfigError::Im(ImError::TooLong { .. }))`; default serial filled by `setup(mac, ip, None).im0.serial_number == "PNIO-CD19F8"` for the DEV MAC); in `gsdml.rs` — extend the existing structural test: DAP `VirtualSubmoduleItem` has `Writeable_IM_Records == "1 2 3"`, DAP `ModuleInfo/OrderNumber@Value == cfg.im0().order_id`, `SoftwareRelease@Value == "V0.1.0"`. The golden test (`sample-16real-32bool.xml`) will fail until regenerated.
- [ ] **Step 2:** FAIL. **Step 3:** implement; regenerate the golden: `cargo run -p pnio --example gen_gsdml -- --out /tmp/g` (check `gen_gsdml`'s `Args` for the exact flag names) then copy the produced file over `testdata/gsdml/sample-16real-32bool.xml`; validate against the XSD per `docs/gsdml.md#validation` (`lxml` recipe, TIA V21 XSD path) — record the validation in the commit message. Update `docs/gsdml.md`: new section "I&M and alarms" (what is declared, why `Writeable_IM_Records` is needed for TIA to write I&M1-3 at download, uninstall/reinstall reminder, standard channel diagnosis needs no declaration, `MayIssueProcessAlarm` stays false) and the "What is not declared yet" list.
- [ ] **Step 4:** PASS, clippy, fmt. **Step 5: Commit** `feat(config,gsdml): I&M0 identity in the config, Writeable_IM_Records and ModuleInfo from it`.

---

### Task 10: `api` — diagnosis API, `im_store`, stop with ERR-RTA; `typed_bringup` flags

**Files:**
- Modify: `crates/pnio/src/api.rs`, `examples/typed_bringup.rs`

**Interfaces:**
- Produces: `StartOptions.im_store: Option<PathBuf>`; `IoDevice::raise_diagnosis(&self, slot: Slot, channel: u16, error: ChannelError, severity: Severity) -> Result<(), ApiError>` (`ApiError::UnknownSlot(Slot)` if `cfg.submodule(slot)` is `None`), `clear_diagnosis(&self, slot, channel, error) -> Result<(), ApiError>`, `diagnoses(&self) -> Vec<Diagnosis>`, `alarm_stats(&self) -> AlarmStats` (from the `DiagShared` atomics); `stop()` unchanged in signature — the acyclic thread's `run_publishing_params` now ends with `dev.shutdown(Instant::now())` (and uses `dev.poll_interval(Duration::from_millis(200))`); `start_inner` sets `setup.im_store = im_store` before `Device::new` (change `cfg.setup(mac, ip, rt)` into a `let mut setup = …; setup.im_store = im_store;`), keeps `dev.diag_shared()` in `IoDevice`. `typed_bringup`: `--diag <slot>:<channel>:<error-name>` (repeatable; raised once `ready()` is first true; cleared before `stop()` on SIGINT/duration end), `--im-store <path>`; summary line gains `alarm_stats`.

- [ ] **Step 1: Failing test** in `api.rs` tests (there is an existing mock-transport start path — `start_with` + the p-net goldens; mirror `typed_replay.rs`): `raise_diagnosis_reaches_the_wire_and_is_listed` — start with `MockTransport`, push the Connect goldens, wait for `ready()`, `raise_diagnosis(Slot(1), 0, ChannelError::LineBreak, Severity::Fault)`, poll up to 500 ms until `eth.sent()` contains an alarm frame, assert `diagnoses().len() == 1`, `raise_diagnosis(Slot(9), …)` → `Err(ApiError::UnknownSlot(Slot(9)))`; `stop_sends_err_rta` — after `stop()`, the last ETH frame is an `Err` RTA with code2 17.
- [ ] **Step 2:** FAIL. **Step 3:** implement; `typed_bringup` flags with `clap` (`--diag` parsed by splitting on `:`; unknown error name → exit 2 with the accepted names listed). **Step 4:** PASS, clippy, fmt; `cargo build --release --target x86_64-unknown-linux-musl --example typed_bringup`. **Step 5: Commit** `feat(api): raise/clear channel diagnosis, I&M store path, ERR-RTA on stop; typed_bringup --diag/--im-store`.

---

### Task 11: `tests/alarm_replay.rs` — end-to-end replay against the goldens

**Files:**
- Create: `crates/pnio/tests/alarm_replay.rs`

- [ ] **Step 1: Write the test** (it fails only if Tasks 2-10 regressed; it is the integration gate):

```rust
//! Replay the 2026-08-30 alarm/I&M capture through `Device` with mock transports: the
//! device must emit p-net's bytes for every frame it originates.
mod common;
use common::{golden, golden_alarm, RPC_OFF};
use pnio::alarm::{parse_frame, AlarmType, RtaBody, RtaData};
use pnio::cm::{AbortReason, ArState, PnioStatus};
use pnio::config::{Direction, Slot};
use pnio::device::{Device, DeviceSetup, DiagCommand};
use pnio::diag::{ChannelError, Diagnosis, Severity};
use pnio::im::{Im0, ImStore, SwRevision};
use std::time::{Duration, Instant};

fn pnet_setup() -> DeviceSetup { /* copy of ar_replay's setup + im0: pnet identity (order "12345 Abcdefghijk", serial "007", hw 3, V0.2.0, profile 0x1234/0x5678), im_store: None */ }

#[test]
fn alarm_handshake_err_rta_and_im0_read_replay_byte_exact() {
    let setup = pnet_setup();
    let eth = pnio::eth::MockTransport::new(); // use the Arc-sharing pattern so `eth` stays usable after Device::new
    let rpc = pnio::rpc::MockRpcTransport::new();
    let cpu = "172.16.2.100:54766".parse().unwrap();
    let cpu_cm = "172.16.2.100:34964".parse().unwrap();
    for name in ["connect_req", "write_req", "prmend_req"] { rpc.push_rx(golden(name)[RPC_OFF..].to_vec(), cpu); }
    rpc.push_rx(golden("appready_res")[RPC_OFF..].to_vec(), cpu_cm);
    let mut dev = Device::new(setup, eth.clone(), rpc.clone());
    dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    assert_eq!(dev.state(), ArState::Data);

    // I&M0 read on the DAP → p-net's exact response blocks
    rpc.push_rx(golden_alarm("im0_read_req")[RPC_OFF..].to_vec(), cpu);
    dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    let last = rpc.sent().last().unwrap().0.clone();
    assert_eq!(last[80 + 20..], golden_alarm("im0_read_res")[RPC_OFF + 80 + 20..]);

    // diagnosis raise → notification (p-net's slot 1 idents 0x30/0x130 come from the model), CPU ack-rta + alarm-ack → our ack-rta golden
    dev.diag_shared().queue.lock().unwrap().push_back(DiagCommand::Raise(Diagnosis { slot: Slot(1), channel: 4, error: ChannelError::ShortCircuit, severity: Severity::Fault, direction: Direction::Input }));
    dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    let notif = eth.sent().last().unwrap().clone();
    let pdu = parse_frame(&notif).unwrap();
    assert_eq!((pdu.header.send_seq, pdu.header.ack_seq), (0xFFFF, 0xFFFE));
    let RtaBody::Data(RtaData::Notification(n)) = pdu.body else { panic!() };
    assert_eq!((n.alarm_type, n.slot, n.subslot, n.module_ident, n.submodule_ident, n.usi), (AlarmType::Diagnosis, 1, 1, 0x30, 0x130, 0x8000));
    eth.push_rx(golden_alarm("alarm_ack_rta_low_cpu"));
    eth.push_rx(golden_alarm("alarm_diag_ack_cpu"));
    dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    assert_eq!(eth.sent().last().unwrap(), &golden_alarm("alarm_ack_rta_low_dev"));
    assert!(dev.problem_indicator());
    assert_eq!(dev.diag_shared().acked.load(std::sync::atomic::Ordering::Relaxed), 1);

    // clear → disappears
    dev.diag_shared().queue.lock().unwrap().push_back(DiagCommand::Clear { slot: Slot(1), channel: 4, error: ChannelError::ShortCircuit });
    dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    let RtaBody::Data(RtaData::Notification(n)) = parse_frame(eth.sent().last().unwrap()).unwrap().body else { panic!() };
    assert_eq!(n.alarm_type, AlarmType::DiagnosisDisappears);
    assert!(!dev.problem_indicator());

    // controller ERR-RTA → Idle with the status, no reply
    let before = eth.sent().len();
    eth.push_rx(golden_alarm("alarm_err_rta_cpu_removed"));
    dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    assert_eq!(dev.state(), ArState::Idle);
    assert_eq!(eth.sent().len(), before);

    // stop from Data → ERR-RTA AR removed (reconnect first)
    for name in ["connect_req", "write_req", "prmend_req"] { rpc.push_rx(golden(name)[RPC_OFF..].to_vec(), cpu); }
    rpc.push_rx(golden("appready_res")[RPC_OFF..].to_vec(), cpu_cm);
    dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    assert_eq!(dev.state(), ArState::Data);
    dev.shutdown(Instant::now()).unwrap();
    let err = parse_frame(eth.sent().last().unwrap()).unwrap();
    assert_eq!(err.body, RtaBody::Err(PnioStatus::rta_abort(PnioStatus::RTA_ABORT_AR_REMOVED)));
}
```
(If `MockTransport`/`MockRpcTransport` are not `Clone`, wrap them in the `Arc` pattern from `device::tests::watchdog_event_aborts_the_ar` and implement the traits for `Arc<Mock…>` in `tests/common` — the existing device test shows how. Whether a reconnect after a controller ERR-RTA replays cleanly depends on `Cm`'s response cache: the second Connect uses the same RPC `seq_num` as the first → the cache answers with the cached Connect response. Use `synthetic_connect_req` from `tests/common` with a bumped `seq_num`, or drive the reconnect through `pnio::cm::Cm::abort` — pick the approach the existing tests already use for a re-Connect; `ar.rs` tests cover the takeover path.)

- [ ] **Step 2:** Run `cargo test -p pnio --test alarm_replay` → PASS (fix any regression in the implementing task, not here). **Step 3: Commit** `test(alarm): end-to-end replay of the alarm channel, ERR-RTA both ways and the I&M0 read against the goldens`.

---

### Task 12: Docs and follow-ups

**Files:**
- Modify: `README.md` (Status table rows `alarm`, `diag`, `im` = done with a one-liner each; Quick Start snippet after the typed write example: `dev.raise_diagnosis(Slot(1), 0, ChannelError::LineBreak, Severity::Fault)?;` + `StartOptions { im_store: Some("/var/lib/pnio/im.bin".into()), .. }`), `FOLLOWUPS.md` (mark resolved: Plan 3 "Minimal Read/ReadImplicit" and "Alarm channel", Plan 4 "ERR-RTA on device stop / ProblemIndicator" and `AbortReason::RtSocket`; add the spec §2 "Out" items as new follow-ups), `docs/bench-pnet-device.md` (new `## 6i. HIL — alarms, diagnosis and I&M (Plan 5)` with the **procedure only** — the six acceptance checks of spec §6 as a checklist with the exact commands: `typed_bringup --diag 1:0:line-break --im-store ~/bench/im.bin …`, what to look at in TIA, which capture to take — results are filled in after the HIL session), `docs/design/2026-08-30-pnio-alarm-diag-im-design.md` status line → "implemented (branch), HIL pending".

- [ ] **Step 1:** write the docs. **Step 2:** `cargo test --all`, `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, `grep -rn "TODO\|TBD" docs/alarm-golden-frames.md docs/gsdml.md README.md` → nothing. **Step 3: Commit** `docs: Plan 5 — README status, FOLLOWUPS, §6i HIL procedure, gsdml I&M section`.

---

## Self-review (done while writing)

- Spec coverage: §5.1 → T2; §5.2 → T3; §5.3 → T4; §5.4 → T5; §5.5 → T6 (+T3 for status/abort); §5.6 → T8; §5.7 → T7; §5.8 → T10; §5.9 → T9; §6 examples → T10, HIL procedure → T12 (execution is the human-driven step after the plan); §7 tests → T2-T8 unit, T11 integration; §8 edge cases → T3 (TooLong, retries, dedup, unexpected, ERR in), T5 (bad record, missing file), T8 (no channel, replay), T10 (UnknownSlot); §9 docs → T1, T9, T12.
- Type consistency: `AlarmChannel::new(cfg)` takes no `now` (T3 signature; the spec sketch had one — dropped, deadlines are set on `enqueue`); `AlarmReq.priority` is always `Low` for diagnosis (T8); `DiagStore::from_model` derives directions from the model (T4) so `Device` needs no config; `ArParams` field names used in T8 (`alarm_ref_local/remote`, `rta_timeout_factor`, `rta_retries`, `max_alarm_data_length`) match T6 — T6 must keep `max_alarm_data_length` as the **remote** (CPU's) value or add `max_alarm_data_length_remote`; T8 uses the CPU's value (256) for the `TooLong` bound.
- Placeholder scan: none; the two "helper to add" notes in T8/T11 name the existing test they copy.

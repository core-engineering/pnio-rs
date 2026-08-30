# Spec — Plan 5: `alarm` + `diag` + I&M records (alarm channel, application diagnosis, identification)

Date: 2026-08-30. Status: implemented on `feat/alarm-diag-im` (2026-08-30), HIL pending — see
`docs/bench-pnet-device.md` §6i.
Parent: [`2026-06-25-profinet-rt-device-design.md`](2026-06-25-profinet-rt-device-design.md) §5.1 (`alarm`, `im` modules), §5.2 (thread model: alarms and I&M on the acyclic thread), §6.4 (supervision: alarm reporting).
Builds on Plans 3, 4, 6, 7: `cm` already parses `AlarmCRBlockReq` and answers `AlarmCRBlockRes`; the acyclic `AF_PACKET` socket already receives FrameIDs `0xFC00..=0xFFFF` (alarm frames included, currently dropped); `Read`/`ReadImplicit` are refused with PNIORW "invalid index"; `rt::engine` emits data status `0x35`; `DeviceConfig` renders the GSDML; `IoDevice` is the facade.
Ground truth: `captures/plan5-20260830/plan5-alarm.pcapng` (git-ignored, 2026-08-30) — p-net `pn_dev` v0.2.0 against the 1515-2 PN: process alarm, standard and USI channel diagnosis appears/update/disappears, device-initiated ERR-RTA, controller ERR-RTA (download and reply), I&M0 reads on DAP/interface/modules. Decoded with Wireshark's `pn_io`/`pn_rt` dissectors (the clean-room oracle, as for Plans 2-4).

## 1. Goal

The device tells the controller what is wrong and who it is: an application raises/clears a **channel diagnosis** from Rust and the CPU sees it (diagnostic buffer, OB82, device fault state, `ProblemIndicator` in the cyclic data status); the device **announces its own stop** on the alarm channel instead of leaving the CPU to its watchdog; TIA reads **I&M0** and writes/reads **I&M1-3**, persisted across restarts.

**Success criteria**
1. `alarm`: RTA-PDU codec and a sender/receiver state machine that reproduces the captured handshake byte-for-byte (goldens) — notification → ACK-RTA → Alarm-Ack → ACK-RTA — with retries/timeouts from the negotiated `AlarmCR`, and ERR-RTA both ways.
2. `diag`: `raise_diagnosis`/`clear_diagnosis` on `IoDevice` produce `Diagnosis`/`Diagnosis disappears` alarms (USI `0x8000`, standard `ChannelErrorType`), set/clear the `ProblemIndicator` bit in the RT data status, survive an AR loss (replayed on the next `Data`).
3. Records: `Read`/`ReadImplicit` of `0xAFF0..=0xAFF3` and `Write` of `0xAFF1..=0xAFF3`, I&M1-3 persisted to an optional file.
4. GSDML: `Writeable_IM_Records="1 2 3"` and `ModuleInfo` from the same `Im0` data as the wire record; still `PNIO_Version="V2.3"`.
5. HIL (§8): the six acceptance checks pass with our GSDML and `pnio-dev` restored in TIA, including a 10-minute 1 ms run with an active diagnosis and zero missed ticks.
6. Everything existing stays green: `ar_replay`/`rt_replay`/`typed_replay`, the p-net profile, `typed_bringup`, `latency_probe`.

## 2. Scope

In:
- `alarm` (new): `rta.rs` (RTA-PDU header, `AlarmNotification{High,Low}`, `AlarmAck{High,Low}`, `AlarmSpecifier`, `ChannelDiagnosis` payload, ERR-RTA status), `channel.rs` (sender/receiver state machine, pure, `on_frame`/`on_tick`/`enqueue` → actions).
- `diag` (new): `ChannelError`, `Severity`, `Diagnosis`, `DiagStore` (raise/clear → alarm requests, `problem_indicator()`, replay list).
- `cm`: `ArParams` gains the AlarmCR parameters (`rta_timeout_factor`, `rta_retries`, `alarm_ref_local/remote`, `max_alarm_data_length`, tag headers); `records.rs` handles `Read`/`ReadImplicit`/`Write` for I&M; new `PnioStatus` constants (RTA abort codes); new `AbortReason` variants.
- `im` (new, small): `Im0` (config), `ImStore` (I&M1-3 in memory + optional file).
- `device`: routes `0xFC01`/`0xFE01` frames to `alarm::channel`, sends its frames on the acyclic socket (VLAN-tagged), drives its timer from the existing tick, drains the application's diag queue, sends ERR-RTA on stop and on internal aborts, replays diagnoses on `Data`.
- `rt`: `ProblemIndicator` bit from a shared `AtomicBool`.
- `config`/`gsdml`/`api`: `Im0` in the builder, GSDML attributes, `IoDevice` diag API and `StartOptions::im_store`.
- Docs, goldens, replay tests, HIL section.

Out (recorded in `FOLLOWUPS.md` at close-out):
- Process alarms (`AlarmType 0x0002`, `MayIssueProcessAlarm`, OB40) — the codec handles the notification block generically, but no API and no GSDML claim.
- Manufacturer-specific diagnosis codes (`0x0100..`) and `ChannelDiagList` texts; `ExtChannelDiagnosis` (USI `0x8002`); qualified channel diagnosis (`0x8003`); AR/API-level diagnosis; `MaintenanceStatus` alarm items.
- Plug/pull/return-of-submodule alarms, `ModuleDiffBlock`.
- Diagnosis record reads (`0x800A..0x800C`, `0xF80C`, `0xE00x`): TIA's *Channel diagnostics* page issued none in the capture (the CPU keeps the state it learned from alarms); refused with "invalid index" as today.
- I&M4, I&M5 (`IM5_Supported`), `I&M0FilterData` (`0xF840`), `RealIdentificationData` (`0xF841`).
- Controller-initiated alarms other than Alarm-Ack (the CPU sends none to an IO-Device).
- Loading *our* DCP responder under a storm (needs a second host, see bench §6h).

## 3. Decisions (locked in brainstorm)

1. **Approach 1**: the alarm channel lives on the acyclic thread next to `cm`, driven by the existing `Device` loop and tick; the RT thread only reads one atomic. No third thread, no RT-side sending.
2. **Standard channel diagnosis only** (`ChannelErrorType 0x0001..=0x0009`), USI `0x8000`, one alarm in flight, FIFO behind it.
3. **I&M0 from `DeviceConfig`** (builder `.im0(..)`), rendered in the GSDML `ModuleInfo` too — one source, no drift (Plan 6 rule). **I&M1-3 writable**, persisted to `StartOptions::im_store: Option<PathBuf>` (raw record bodies, no dependency); without a path they are volatile, documented.
4. **ERR-RTA on every device-side abort**: `stop()` (`AR removed`, code2 `17`), RT watchdog (`AR consumer DHT/WDT expired`, code2 `5`), alarm send failure (`AR alarm-send.cnf(-)`, code2 `3`), socket failure (code2 `17`).
5. **Controller ERR-RTA aborts the AR** immediately (`AbortReason::ControllerErrRta(PnioStatus)`) — replaces the "notice it at the next Connect" inference of Plan 3, which stays as a fallback.
6. **`PNIO_Version` stays `"V2.3"`**; `MayIssueProcessAlarm` stays `false`.
7. Ground truth = the 2026-08-30 p-net capture + Wireshark; the purchased IEC 61158-6-10 remains a later cross-check (FOLLOWUPS).

## 4. Wire formats (pinned from the capture)

All multi-byte fields big-endian. Alarm frames are Ethernet `0x8892` behind an 802.1Q tag: **High** = FrameID `0xFC01`, VLAN priority 6 (`TCI 0xC000`); **Low** = FrameID `0xFE01`, priority 5 (`TCI 0xA000`) — the tag headers the CPU sends in `AlarmCRBlockReq` (`AlarmCRTagHeaderHigh 0xC000`, `Low 0xA000`). Diagnosis alarms go on **Low**; ERR-RTA on the priority of the AR's last used channel (p-net and the CPU both used Low; we always use Low for ERR-RTA).

### 4.1 RTA-PDU header (12 bytes after the FrameID)

| Offset | Field | Notes |
|---|---|---|
| 0 | `AlarmDstEndpoint` u16 | peer's `LocalAlarmReference` (CPU sent `0x0000`) |
| 2 | `AlarmSrcEndpoint` u16 | our `LocalAlarmReference` (we answer `0x0000` in `AlarmCRBlockRes`, unchanged) |
| 4 | `PDUType` u8 | low nibble type: `1` DATA, `2` NACK, `3` ACK, `4` ERR; high nibble version `1` → `0x11`, `0x13`, `0x14` |
| 5 | `AddFlags` u8 | low nibble `WindowSize` = 1; bit 4 `TACK` (1 on DATA = "transport ack requested", 0 on ACK/ERR) |
| 6 | `SendSeqNum` u16 | see sequence rules |
| 8 | `AckSeqNum` u16 | see sequence rules |
| 10 | `VarPartLen` u16 | bytes that follow: 0 for ACK, 4 for ERR (PNIOStatus), block length for DATA |

**Sequence rules** (as observed, both sides symmetric): a side's first DATA carries `SendSeqNum = 0xFFFF`, then `0, 1, 2, …` (wrap to 0 after `0x7FFF`); `AckSeqNum` = the last DATA sequence accepted from the peer, `0xFFFE` before any. An ACK-RTA carries `SendSeqNum` = the sender's own last DATA sequence (`0xFFFE` if it never sent one) and `AckSeqNum` = the DATA it acknowledges. ERR-RTA carries the current counters. Captured: our first notification `FFFF/FFFE` → CPU ACK `FFFE/FFFF` → CPU Alarm-Ack DATA `FFFF/FFFF` → our ACK `FFFF/FFFF`; sixth device DATA `0004/0003`; device ERR-RTA after eight alarms `0006/0005`; CPU ERR-RTA at download (no alarms yet) `FFFE/FFFE`.

### 4.2 DATA var part

**AlarmNotification** (BlockType `0x0001` High / `0x0002` Low, version 1.0): `AlarmType` u16 (`0x0001` Diagnosis, `0x0002` Process, `0x0003` Pull, `0x0004` Plug, `0x0005` Status, `0x0006` Update, `0x0007` Redundancy, `0x0008` Controlled by supervisor, `0x0009` Released, `0x000A` Plug wrong submodule, `0x000B` Return of submodule, `0x000C` Diagnosis disappears, …), `API` u32, `SlotNumber` u16, `SubslotNumber` u16, `ModuleIdentNumber` u32, `SubmoduleIdentNumber` u32, `AlarmSpecifier` u16, `UserStructureIdentifier` u16, then USI-specific data. Captured process alarm: block length 25 = version 2 + 23 (no data beyond 1 byte); captured ExtChannelDiagnosis: 36.

`AlarmSpecifier` bits: 0-10 `SequenceNumber` (per-AR counter of notifications, starts at 0, we increment per alarm sent, wraps at `0x7FF`), 11 `ChannelDiagnosis` (1 while any channel diagnosis exists on the submodule *after* this alarm), 12 `ManufacturerSpecificDiagnosis` (0), 13 `SubmoduleDiagnosisState` (1 while any diagnosis exists on the submodule), 15 `ARDiagnosisState` (1 while any diagnosis exists on the AR). Captured: `0x0000` for the process alarm, bits 11/13/15 set for diagnosis appears; the p-net "disappears" kept them set because a USI diagnosis remained — we clear them when the store becomes empty for that submodule/AR.

**ChannelDiagnosis payload** (USI `0x8000`): `ChannelNumber` u16 (`0x8000` = whole submodule), `ChannelProperties` u16, `ChannelErrorType` u16. `ChannelProperties`: bits 0-7 `Type` (0 = unspecified, 1 = 1 bit, 2 = 2 bits, … 7 = 64 bits — we send 0), bit 8 `Accumulative` (0), bits 9-10 `Maintenance` (`00` fault/diagnosis, `01` maintenance required, `10` maintenance demanded), bits 11-12 `Specifier` (`00` all disappear, `01` appears, `10` disappears, `11` disappears but others remain), bits 13-15 `Direction` (`0` manufacturer-specific, `1` input, `2` output, `3` input/output). Captured p-net appears: `0x2801` (input, appears, fault, 1 bit).

**AlarmAck** (BlockType `0x8001` High / `0x8002` Low, version 1.0, length 18): `AlarmType` u16, `API` u32, `SlotNumber` u16, `SubslotNumber` u16, `AlarmSpecifier` u16 (echo), `PNIOStatus` u32 (`0x00000000` OK).

### 4.3 ERR-RTA

`VarPartLen 4`: PNIOStatus `ErrorCode 0xCF` (RTA error), `ErrorDecode 0x81` (PNIO), `ErrorCode1 0xFD` (`RTA_ERR_CLS_PROTOCOL`), `ErrorCode2`: `1` AR consumer DHT expired, `3` AR alarm-send.cnf(-), `5` AR consumer DHT/WDT expired, `11` AR alarm.ind(err), `17` AR removed, `0` reserved (what p-net sends). The CPU answered our device abort with `11` and its own download-time abort was `17`.

### 4.4 I&M records

Read request: `IODReadReqHeader` (BlockType `0x0009`, length 60, version 1.0): `SeqNumber`, `ARUUID`, `API`, `Slot`, `Subslot`, padding, `Index`, `RecordDataLength` (`0x8000` = max), `TargetARUUID`, padding. Read response: `IODReadResHeader` (`0x8009`, 60) with `RecordDataLength` = record bytes, followed by the record. `ReadImplicit` (opnum 5) uses the same headers with a nil `ARUUID` and no AR lookup.

**I&M0** (BlockType `0x0020`, length 56, version 1.0 — 60 bytes total): `VendorID` u16, `OrderID` 20 ASCII (space-padded), `IM_Serial_Number` 16 ASCII, `IM_Hardware_Revision` u16, `IM_Software_Revision` 4 (`prefix` ASCII `V`/`R`/`P`/`U`/`T`, `functional_enhancement` u8, `bug_fix` u8, `internal_change` u8), `IM_Revision_Counter` u16, `IM_Profile_ID` u16, `IM_Profile_Specific_Type` u16, `IM_Version` u8.u8 (`1.1`), `IM_Supported` u16 bitmask (bit 1 I&M1, bit 2 I&M2, bit 3 I&M3 → `0x000E` on the DAP, `0x0000` on the other submodules).
**I&M1** (`0x0021`, length 56): `IM_Tag_Function` 32 ASCII + `IM_Tag_Location` 22 ASCII. **I&M2** (`0x0022`, 18): `IM_Date` 16 ASCII (`YYYY-MM-DD HH:MM`). **I&M3** (`0x0023`, 56): `IM_Descriptor` 54 ASCII. Space-padded; a Write with a wrong block length/version is refused with PNIORW "invalid parameter".

Which submodule answers: `0xAFF0` on **every** submodule the device model knows (TIA reads DAP `0/1`, interface `0/0x8000` and `0/0x8001`, each module `n/1`) with the same content — `IM_Supported = 0x000E` included. The capture settles this: `im0_read_res_if` (interface subslot `0/0x8000`) carries the same mask as the DAP answer, so there is no "supported only on the DAP" distinction to encode. `0xAFF1..3` follow: one device-wide store, readable and writable on **every** known submodule; a `(slot, subslot)` absent from the model → "invalid index".

### 4.5 Negotiated AlarmCR (CPU 1515-2 PN, TIA V21)

`AlarmCRType 1`, `LT 0x8892`, `AlarmCRProperties 0` (user priority), `RTATimeoutFactor 1` (×100 ms), `RTARetries 3`, `LocalAlarmReference 0x0000`, `MaxAlarmDataLength 256`, tag headers `0xC000`/`0xA000`. We respond `LocalAlarmReference 0x0000`, `MaxAlarmDataLength 200` (unchanged from Plan 3).

## 5. Modules

### 5.1 `alarm::rta` (codec)

```rust
pub enum PduType { Data = 1, Nack = 2, Ack = 3, Err = 4 }
pub struct RtaHeader { pub dst_ref: u16, pub src_ref: u16, pub pdu_type: PduType, pub tack: bool, pub send_seq: u16, pub ack_seq: u16 }
pub enum Priority { High, Low }              // FrameID 0xFC01 / 0xFE01, VLAN prio 6 / 5
pub enum AlarmType { Diagnosis = 0x0001, Process = 0x0002, /* … */ DiagnosisDisappears = 0x000C, Other(u16) }
pub struct AlarmSpecifier { pub sequence: u16, pub channel_diag: bool, pub manufacturer_diag: bool, pub submodule_diag: bool, pub ar_diag: bool }
pub struct AlarmNotification { pub alarm_type: AlarmType, pub api: u32, pub slot: u16, pub subslot: u16, pub module_ident: u32, pub submodule_ident: u32, pub specifier: AlarmSpecifier, pub usi: u16, pub data: Vec<u8> }
pub struct AlarmAck { pub alarm_type: AlarmType, pub api: u32, pub slot: u16, pub subslot: u16, pub specifier: AlarmSpecifier, pub status: PnioStatus }
pub struct ChannelDiagnosis { pub channel: u16, pub properties: ChannelProperties, pub error_type: u16 }
pub enum RtaPdu { Data(RtaHeader, Priority, RtaData), Ack(RtaHeader, Priority), Err(RtaHeader, Priority, PnioStatus), Nack(RtaHeader, Priority) }
pub enum RtaData { Notification(AlarmNotification), Ack(AlarmAck), Unknown { block_type: u16, body: Vec<u8> } }
pub fn parse_frame(frame: &[u8]) -> Result<RtaPdu, RtaError>;           // Ethernet + optional tag + FrameID + RTA
pub fn build_frame(dst: MacAddr, src: MacAddr, pdu: &RtaPdu) -> Vec<u8>; // tagged, min length padded
```

Goldens (`crates/pnio/tests/fixtures/`, hex): `alarm_process_notif.hex` (device → CPU, High), `alarm_ack_rta_high_cpu.hex`, `alarm_ack_high_cpu.hex` (Alarm-Ack DATA), `alarm_ack_rta_high_dev.hex`, `alarm_diag_notif.hex`, `alarm_ack_rta_low_cpu.hex`, `alarm_diag_ack_cpu.hex`, `alarm_ack_rta_low_dev.hex`, `alarm_diag_disappears.hex`, `alarm_err_rta_dev.hex`, `alarm_err_rta_cpu.hex`, `alarm_err_rta_cpu_removed.hex`, `im0_read_req.hex`, `im0_read_res.hex` (DAP `0/1`, p-net identity), catalogued in `docs/alarm-golden-frames.md`. Round-trip tests: parse → build == golden bytes for every device-emitted frame; parse-only for the CPU ones.

### 5.2 `alarm::channel` (state machine, pure)

```rust
pub struct AlarmChannelConfig { pub local_ref: u16, pub remote_ref: u16, pub rta_timeout: Duration /* factor × 100 ms */, pub rta_retries: u16, pub max_alarm_data_length: u16, pub peer_mac: MacAddr, pub our_mac: MacAddr }
pub struct AlarmReq { pub id: u32, pub priority: Priority, pub notification: AlarmNotification }
pub enum AlarmAction { Send(Vec<u8>), Acked { id: u32, status: PnioStatus }, Abort(AbortReason), UnexpectedRx }
pub struct AlarmChannel { /* config, tx state, rx state, queue, counters */ }
impl AlarmChannel {
    pub fn new(cfg: AlarmChannelConfig, now: Instant) -> Self;
    pub fn enqueue(&mut self, req: AlarmReq, now: Instant) -> Result<Vec<AlarmAction>, AlarmError>; // AlarmError::TooLong
    pub fn on_frame(&mut self, frame: &[u8], now: Instant) -> Vec<AlarmAction>;
    pub fn on_tick(&mut self, now: Instant) -> Vec<AlarmAction>;
    pub fn err_rta(&mut self, status: PnioStatus) -> Vec<u8>;      // frame to send on abort (Low)
    pub fn in_flight(&self) -> Option<u32>; pub fn queued(&self) -> usize; pub fn stats(&self) -> AlarmStats;
}
```

Sender: `Idle` —`enqueue`→ `SentData { req, attempt, deadline }` —ACK-RTA(ack_seq == send_seq)→ `AwaitAlarmAck { req }` —DATA Alarm-Ack(same type/slot/subslot)→ emit our ACK-RTA + `Acked` → next queued or `Idle`. An Alarm-Ack matching the in-flight alarm is also accepted straight out of `SentData`: it may overtake the transport ACK, and its arrival proves delivery.

**Timeouts differ by state** (revised at Plan 5 close-out; the two states had the same policy in the first draft):
- `SentData` past `deadline = now + rta_timeout`: the peer never confirmed delivery — resend the identical DATA (attempt+1) until `rta_retries` is exhausted → `Abort(AlarmSendFailed)` (the device then sends ERR-RTA code2 3).
- `AwaitAlarmAck` past `deadline = now + 10 × rta_timeout` (1 s at the bench's `RTATimeoutFactor 1`): the transport already confirmed delivery, so a resend would only duplicate the alarm and an abort would take the AR down over a slow controller application. Log at `warn`, count `AlarmStats::ack_timeouts`, **drop** the alarm, go `Idle`, continue the queue. No `Abort`, the AR stays up. Receiver: any DATA → ACK-RTA (dedup: a DATA with `send_seq` == last accepted is re-acked, not re-processed); DATA that is not an Alarm-Ack for the in-flight alarm → `UnexpectedRx` (counted, acked, ignored); ERR-RTA → `Abort(ControllerErrRta(status))`; NACK → treated as unexpected. Sequence counters per §4.1; `AlarmSpecifier.sequence` per AR.

### 5.3 `diag`

```rust
pub enum ChannelError { ShortCircuit = 0x0001, Undervoltage = 0x0002, Overvoltage = 0x0003, Overload = 0x0004, Overtemperature = 0x0005, LineBreak = 0x0006, UpperLimitExceeded = 0x0007, LowerLimitExceeded = 0x0008, Error = 0x0009 }
pub enum Severity { Fault, MaintenanceRequired, MaintenanceDemanded }
pub const WHOLE_SUBMODULE: u16 = 0x8000;
pub struct Diagnosis { pub slot: Slot, pub channel: u16, pub error: ChannelError, pub severity: Severity, pub direction: Direction }
pub struct DiagStore { /* BTreeMap<(Slot, u16, ChannelError), Diagnosis>, per-submodule idents/direction from DeviceConfig */ }
impl DiagStore {
    pub fn raise(&mut self, d: Diagnosis) -> Option<AlarmNotification>;   // None if identical already present; Some(appears/update) otherwise
    pub fn clear(&mut self, slot: Slot, channel: u16, error: ChannelError) -> Option<AlarmNotification>; // None if absent; Some(disappears / disappears-but-others-remain)
    pub fn problem_indicator(&self) -> bool;   // any Severity::Fault active
    pub fn active(&self) -> Vec<Diagnosis>;
    pub fn replay(&self) -> Vec<AlarmNotification>;  // one "appears" per active diagnosis, for a fresh AR
}
```

Rules: `raise` of an existing `(slot, channel, error)` with a different severity = update (an `Diagnosis` alarm with the new properties); `Direction` comes from the submodule's config (`Input`/`Output`/`InputOutput` → 1/2/3); `AlarmSpecifier` flags computed from the store *after* the change; `ChannelProperties.Specifier` = appears / disappears / disappears-but-others-remain (`0b11`) when other diagnoses stay on the same channel.

### 5.4 `im`

```rust
pub struct Im0 { pub order_id: String /* ≤20 */, pub serial_number: String /* ≤16 */, pub hardware_revision: u16, pub software_revision: SwRevision /* prefix char, x, y, z */, pub revision_counter: u16, pub profile_id: u16, pub profile_specific_type: u16 }
pub struct Im1 { pub tag_function: String /* ≤32 */, pub tag_location: String /* ≤22 */ }
pub struct Im2 { pub date: String /* ≤16 */ }   pub struct Im3 { pub descriptor: String /* ≤54 */ }
pub struct ImStore { im1, im2, im3, path: Option<PathBuf> }
impl ImStore { pub fn load(path: Option<PathBuf>) -> ImStore; pub fn write(&mut self, index: u16, body: &[u8]) -> Result<(), RecordError>; pub fn read(&self, index: u16) -> Option<Vec<u8>>; }
pub fn encode_im0(vendor_id: u16, im0: &Im0, supported: u16) -> Vec<u8>;  // 60 bytes with block header
```

Builder: `DeviceConfigBuilder::im0(Im0)`; defaults when absent: `order_id = station_type` (truncated to 20), `serial_number = "PNIO-" + last 3 MAC octets in hex` (computed at `setup()`, since the MAC is only known there), `hardware_revision 1`, `software_revision V0.1.0` (crate version), `revision_counter 0`, `profile_id 0`, `profile_specific_type 0`. Validation in `build()`: ASCII only, length limits. File format: the three record bodies back to back, exactly as on the wire — I&M1 (54 bytes) + I&M2 (16) + I&M3 (54) = 124 bytes, no header; any other length → treated as absent (`log::warn!`). Written atomically (temp file + rename) on every accepted Write; unreadable/absent file at start → empty strings + `log::warn!`; write failure → `log::error!`, Write still answered OK (a local disk problem is not the controller's).

### 5.5 `cm` changes

- `ArParams` + `rta_timeout_factor`, `rta_retries`, `alarm_ref_remote` (already), `alarm_ref_local` (0), `max_alarm_data_length` (already), `alarm_tag_high/low`.
- `records.rs`: `handle_read(req, ar: Option<&ArParams>, implicit: bool, im: &ImStore, im0: &Im0Data, model: &DeviceModel) -> Response` and `handle_write_im(..)`; wired into the `Read`/`ReadImplicit` arm (replacing `read_index_unsupported()` for the I&M indices) and into the existing Write dispatch for `0xAFF1..3` (any slot/subslot ≠ DAP `0/1` → invalid index). The `MultipleWrite` path already iterates records; I&M writes inside it are handled by the same function.
- `status.rs`: `rta_abort(code2: u8) -> PnioStatus` (`CF 81 FD xx`), constants for code2 1/3/5/11/17; `write_invalid_parameter()` for bad I&M block shapes.
- `AbortReason` + `ControllerErrRta(PnioStatus)`, `AlarmSendFailed`, `RtSocket` (the FOLLOWUPS item); `ar.rs` gets `Event::ControllerErrRta(status)`.
- `CmOutput` + `alarm_channel_config: Option<AlarmChannelConfig>` on the `Data` transition (from `ArParams` + MACs) and `alarm_channel_drop` on abort.

### 5.6 `device` changes

- Owns `Option<AlarmChannel>` (created on `Data`, dropped on `Idle`), the `DiagStore`, the `ImStore`, an `Arc<AtomicBool>` `problem_indicator`, and `Arc<Mutex<VecDeque<DiagCommand>>>` shared with `IoDevice`.
- `step`: (1) drain the ETH acyclic socket: DCP frames as today, FrameID `0xFC01`/`0xFE01` → `alarm.on_frame` (frames arriving while no channel exists are counted and dropped); (2) drain the diag queue → `DiagStore` → `alarm.enqueue` (or nothing when `Idle`); (3) `alarm.on_tick(now)` with the existing tick; (4) apply actions: `Send` → `eth.send`, `Acked` → log/debug counter, `Abort(reason)` → send ERR-RTA (code by reason) then the existing abort path, `UnexpectedRx` → counter; (5) after every change, `problem_indicator.store(diag.problem_indicator())`.
- On `Data` (after ApplicationReady done): `alarm = AlarmChannel::new(cfg)`, then `for n in diag.replay() { alarm.enqueue(n) }`.
- On `stop()` while in `Data`: send ERR-RTA (`AR removed`) before closing sockets; on RT watchdog abort: ERR-RTA code2 5 (the `WatchdogExpired` event path); on socket errors: code2 17.
- Poll interval: 200 ms today → 20 ms while the diag queue is non-empty or an alarm is in flight (RTA timeout is 100 ms; 20 ms keeps retries on time without spinning).

### 5.7 `rt` change

`RtEngine::on_tick` builds the data status as `RUN_PRIMARY_VALID_OK` with bit 5 (`StationProblemIndicator`, 1 = OK) cleared when `problem_indicator` is set: `0x35` → `0x15`. One `AtomicBool::load(Relaxed)` per tick, passed in through `RtShared`. Unit test asserts `0x15` when the flag is set and `0x35` after clear.

### 5.8 `api` changes

```rust
pub struct StartOptions { /* existing */ pub im_store: Option<PathBuf> }
impl IoDevice {
    pub fn raise_diagnosis(&self, slot: Slot, channel: u16, error: ChannelError, severity: Severity) -> Result<(), ApiError>; // ApiError::UnknownSlot
    pub fn clear_diagnosis(&self, slot: Slot, channel: u16, error: ChannelError) -> Result<(), ApiError>;
    pub fn diagnoses(&self) -> Vec<Diagnosis>;
    pub fn alarm_stats(&self) -> AlarmStats;   // sent, acked, retries, unexpected_rx, send_failures
}
```
Both calls validate the slot against the config, push a `DiagCommand` and return; the effect on the wire follows within one poll interval. `stop()` gains the ERR-RTA step. `Validity`/`Freshness` unchanged (our own problem indicator is not a freshness signal).

### 5.9 `gsdml` changes

DAP `VirtualSubmoduleItem`: `Writeable_IM_Records="1 2 3"`; `<ModuleInfo>` of the DAP and of each module: `<OrderNumber Value="…"/>`, `<HardwareRelease Value="…"/>`, `<SoftwareRelease Value="V0.1.0"/>` from `Im0`. Structural test: the rendered values equal `encode_im0`'s fields; golden GSDML updated; XSD validation recipe unchanged (`docs/gsdml.md`). `file_name()` unchanged → the docs repeat the uninstall/reinstall step.

## 6. Examples and HIL

- `examples/typed_bringup` gains `--diag <slot>:<channel>:<error>` (raise after `ready()`, clear on SIGINT before stop) and `--im-store <path>`; `examples/gen_gsdml` unchanged except the new attributes; `ar_bringup`/`rt_bringup` untouched (p-net profile).
- HIL acceptance (§1 criterion 5), TIA project `PLC_BENCH` with `pnio-dev` restored, X1, 500 µs or 1 ms:
  1. **Diagnosis in/out**: `--diag 1:0:line-break` → CPU diagnostic buffer "diagnostic entering, line break, channel 0, slot 1", device red in TIA, data status `0x15` in a capture; SIGINT → "outgoing", `0x35`.
  2. **I&M1-3 write + persistence**: Plant designation/Location set in TIA → Write `0xAFF1` OK in the capture; restart `typed_bringup` with the same `--im-store` → Read `0xAFF1` returns them (TIA *Online & diagnostics* shows them).
  3. **I&M0 read**: TIA module diagnostics → Read `0xAFF0` OK on DAP/interface/modules, fields = builder values.
  4. **Device stop**: SIGINT → ERR-RTA on the wire, CPU logs "IO device failure" within 10 ms (diagnostic buffer timestamp vs capture), no watchdog wait.
  5. **Replay on reconnect**: diag active, CPU STOP→RUN (or cable pull) → the diagnosis is announced again and visible in the CPU.
  6. **RT non-regression**: 10-minute 1 ms run with an active diagnosis, `VERDICT: PASS`, 0 missed ticks.
- Report: `docs/bench-pnet-device.md` §6i.

## 7. Tests

- Unit: `alarm::rta` round trips on every golden; header/sequence tables; `AlarmSpecifier` and `ChannelProperties` bit packing; `channel` state machine (happy path, Alarm-Ack overtaking the transport ACK, retry, exhaustion → abort, `AwaitAlarmAck` timeout drops the alarm without aborting, duplicate DATA re-acked, unexpected DATA, ERR-RTA in, TooLong); `diag` (raise/update/clear, flags after change, replay, problem indicator per severity); `im` (encode I&M0 == golden body with p-net's identity, write validation, file round trip, missing/short file); `cm::records` (Read/ReadImplicit/Write responses byte-exact vs goldens); `rt` data status bit.
- Integration: `tests/alarm_replay.rs` — p-net Connect goldens bring the mock device to `Data`, then: raise → the device emits `alarm_diag_notif.hex` bytes (modulo sequence/idents pinned to p-net's), feed `alarm_ack_rta_low_cpu.hex` + `alarm_diag_ack_cpu.hex` → device emits `alarm_ack_rta_low_dev.hex`; clear → disappears frame; feed `alarm_err_rta_cpu.hex` → AR `Idle`, `last_abort == ControllerErrRta`; stop → `alarm_err_rta_dev.hex` shape; Read `0xAFF0` request golden → response golden.
- `capture_replay`/tshark validation extended to the new device-emitted frames.

## 8. Errors and edge cases

- Alarm data > `max_alarm_data_length` (200 ours / 256 CPU's — we bound by the CPU's value) → `AlarmError::TooLong` at `enqueue`; never on the wire.
- No ACK-RTA within `rta_timeout` × (`rta_retries`+1) → ERR-RTA code2 3 + abort `AlarmSendFailed`; diagnoses replayed after the CPU reconnects.
- Transport ACK received but no Alarm-Ack within `10 × rta_timeout` → `warn` + `AlarmStats::ack_timeouts` + the alarm is dropped; **no** ERR-RTA and **no** abort, the AR stays up and the queue continues. The controller keeps whatever it learned from the notification it did acknowledge at the transport level.
- ACK-RTA with an unexpected `ack_seq` → ignored (counted); DATA with a repeated `send_seq` → re-ACK only.
- ERR-RTA while `Idle` → dropped. Alarm frames from a MAC ≠ AR initiator → dropped, counted.
- `raise`/`clear` while `Idle` → store only; `raise` of an unknown slot → `ApiError::UnknownSlot`; `clear` of an absent diagnosis → `Ok(())`, no alarm.
- I&M Write on a `(slot, subslot)` absent from the device model / unknown index → PNIORW "invalid index"; bad block length/version → "invalid parameter"; non-ASCII → accepted as bytes (the CPU never sends any), stored verbatim.
- `im_store` path unwritable → logged once per failure, in-memory value kept, Write answered OK.
- Poll interval change must not alter DCP/RPC behaviour: only the sleep bound changes.

## 9. Docs

`docs/alarm-golden-frames.md` (new: capture provenance, frame table, key facts — sequence rules, VLAN priorities, block lengths, what the CPU reads and writes); `docs/gsdml.md` (I&M section: `Writeable_IM_Records`, `ModuleInfo`, why the p-net GSDML got no I&M1 write); `docs/bench-pnet-device.md` §6i; `README.md` (Status rows `alarm` / `diag` / `im`, Quick Start snippet with `raise_diagnosis`); `FOLLOWUPS.md` (Plan 3/4 alarm and Read items resolved; new: process alarms, manufacturer codes, ExtChannel, diagnosis records, I&M4/5, IEC cross-check, DCP responder storm with a second host).

## 10. Dependencies

None: the I&M file is raw record bytes (`std::fs` only). `roxmltree` stays dev-only.

## 11. Roles

Plan tasks in order: goldens + `docs/alarm-golden-frames.md` → `alarm::rta` → `alarm::channel` → `diag` → `im` + `cm::records` → `cm`/`device` wiring + ERR-RTA → `rt` bit → `api`/`config`/`gsdml` → examples → replay test → docs → HIL. Each task TDD against the goldens; HIL last, with the TIA project switched back to `pnio-dev`.

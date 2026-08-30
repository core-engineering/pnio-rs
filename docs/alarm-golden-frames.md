# Alarm and I&M golden frames

## Provenance

Captured on 2026-08-30 during a p-net v0.2.0 `pn_dev` handshake with a CPU 1515-2 PN (firmware V2.9.4) running TIA V21. Capture file: `captures/plan5-20260830/plan5-alarm.pcapng`, decoded with Wireshark 4.6.x using the `pn_io`/`pn_rt` dissectors. Ethernet MAC addresses: CPU `8c:f3:19:cd:19:f8`, p-net device `ec:1c:5d:61:e7:3c`. Test bench configuration: DGS-1008P switch in line; device on X1 at 32 ms (Plan 7bis baseline); I&M records read/write on DAP and interface.

## Inventory

| File | Frame | Direction | Bytes | What |
|---|---|---|---|---|
| `im0_read_req_if.hex` | 24410 | CPU -> p-net | 206 | Read request I&M0 on the interface submodule slot 0 subslot 0x8000 |
| `im0_read_res_if.hex` | 24411 | p-net -> CPU | 266 | Read response I&M0 on the interface submodule |
| `im0_read_req.hex` | 24414 | CPU -> p-net | 206 | DCE-RPC Read request (opnum 2), IODReadReqHeader slot 0 subslot 1, index 0xAFF0 I&M0, RecordDataLength 0x8000 |
| `im0_read_res.hex` | 24415 | p-net -> CPU | 266 | DCE-RPC Read response, IODReadResHeader + I&M0 block (60 bytes), p-net identity VendorID 0x0493 OrderID '12345 Abcdefghijk' |
| `alarm_err_rta_cpu.hex` | 47351 | CPU -> p-net | 64 | Alarm Low ERR-RTA at TIA download, PNIOStatus CF 81 FD 11 (AR removed), SendSeq 0xFFFE AckSeq 0xFFFE |
| `alarm_err_rta_dev_removed_reply.hex` | 47352 | p-net -> CPU | 36 | Alarm Low ERR-RTA reply, same status, SendSeq 0xFFFF AckSeq 0xFFFE |
| `alarm_process_notif.hex` | 50893 | p-net -> CPU | 61 | Alarm High 0xFC01 DATA: AlarmNotification Process, slot 1/1, USI 0x0010, 1 byte user data (SendSeq 0xFFFF AckSeq 0xFFFE) |
| `alarm_ack_high_cpu.hex` | 50894 | CPU -> p-net | 64 | Alarm High DATA: AlarmAck Process, slot 1/1, PNIOStatus OK (SendSeq 0xFFFF AckSeq 0xFFFF) |
| `alarm_ack_rta_high_cpu.hex` | 50895 | CPU -> p-net | 64 | Alarm High ACK-RTA for the process notification (SendSeq 0xFFFE AckSeq 0xFFFF) |
| `alarm_ack_rta_high_dev.hex` | 50896 | p-net -> CPU | 32 | Alarm High ACK-RTA for the CPU's AlarmAck (SendSeq 0xFFFF AckSeq 0xFFFF) |
| `alarm_diag_notif.hex` | 52813 | p-net -> CPU | 72 | Alarm Low 0xFE01 DATA: AlarmNotification Diagnosis, slot 1/1, USI 0x8002 ExtChannelDiagnosis, channel 4, properties 0x2801 (input, appears, fault), error type 0x0001 |
| `alarm_ack_rta_low_cpu.hex` | 52814 | CPU -> p-net | 64 | Alarm Low ACK-RTA for the diagnosis notification |
| `alarm_diag_ack_cpu.hex` | 52815 | CPU -> p-net | 64 | Alarm Low DATA: AlarmAck Diagnosis, slot 1/1, PNIOStatus OK |
| `alarm_ack_rta_low_dev.hex` | 52816 | p-net -> CPU | 32 | Alarm Low ACK-RTA for the CPU's AlarmAck |
| `alarm_diag_update_appears.hex` | 53455 | p-net -> CPU | 72 | Alarm Low DATA: Diagnosis (update, first half), properties 0x2801 appears |
| `alarm_diag_update_others_remain.hex` | 53460 | p-net -> CPU | 72 | Alarm Low DATA: Diagnosis (update, second half), properties 0x3801 disappears-but-others-remain |
| `alarm_diag_usi_disappears.hex` | 55387 | p-net -> CPU | 60 | Alarm Low DATA: AlarmNotification DiagnosisDisappears (0x000C), USI 0x1234 (manufacturer), SendSeq 0x0004 AckSeq 0x0003 |
| `alarm_diag_std_remove.hex` | 56029 | p-net -> CPU | 72 | Alarm Low DATA: Diagnosis with properties 0x2001 (input, specifier 0 = all disappear), error type 0x0001 - p-net's encoding of a standard-diagnosis removal |
| `alarm_err_rta_dev.hex` | 57310 | p-net -> CPU | 36 | Alarm Low ERR-RTA, PNIOStatus CF 81 FD 00 (device-initiated abort), SendSeq 0x0006 AckSeq 0x0005 |
| `alarm_err_rta_cpu_removed.hex` | 57311 | CPU -> p-net | 64 | Alarm Low ERR-RTA reply, PNIOStatus CF 81 FD 0B (AR alarm.ind(err)) |

## Key facts

### Wire formats

All multi-byte fields are big-endian. Alarm frames are Ethernet `0x8892` (PROFINET) behind an 802.1Q VLAN tag (12 bytes at frame start, then 4-byte VLAN TCI at offset 12):
- **High priority** alarms: FrameID `0xFC01`, VLAN priority 6 (TCI `0xC000`).
- **Low priority** alarms: FrameID `0xFE01`, VLAN priority 5 (TCI `0xA000`).

The tag headers match those sent by the CPU in the `AlarmCRBlockReq` negotiation on Connect.

I&M Read/Write frames are untagged IPv4/UDP/DCE-RPC frames (no VLAN tag, EtherType `0x0800` at offset 12).

### Sequence rules

Observed during the capture, symmetric on both sides:
- **First DATA** from a peer: `SendSeqNum = 0xFFFF`, then `0x0000, 0x0001, 0x0002, …` (wraps to 0 after `0x7FFF`).
- **AckSeqNum** = the last DATA sequence accepted from the peer; `0xFFFE` before any DATA is accepted.
- **ACK-RTA**: `SendSeqNum` = the sender's own last DATA sequence (`0xFFFE` if it never sent one); `AckSeqNum` = the DATA being acknowledged.
- **ERR-RTA**: carries the current `SendSeqNum` and `AckSeqNum` counters.

Captured handshake: device notif `FFFF/FFFE` → CPU ACK `FFFE/FFFF` → CPU Alarm-Ack `FFFF/FFFF` → device ACK `FFFF/FFFF`; device sixth DATA `0004/0003`; device ERR-RTA after eight alarms `0006/0005`; CPU download-time abort `FFFE/FFFE`.

### RTA-PDU header (12 bytes after FrameID `0xFC01`/`0xFE01`)

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 2 | `AlarmDstEndpoint` | Peer's `LocalAlarmReference` (CPU sends `0x0000`) |
| 2 | 2 | `AlarmSrcEndpoint` | Our `LocalAlarmReference` (p-net answers `0x0000` in responses) |
| 4 | 1 | `PDUType` | Low nibble: 1=DATA, 2=NACK, 3=ACK, 4=ERR; high nibble version (always 1 → `0x1X`) |
| 5 | 1 | `AddFlags` | Low nibble `WindowSize`=1; bit 4 `TACK` (1 on DATA="transport ack requested", 0 on ACK/ERR) |
| 6–7 | 2 | `SendSeqNum` | Sequence (see rules above) |
| 8–9 | 2 | `AckSeqNum` | Last DATA accepted from peer |
| 10–11 | 2 | `VarPartLen` | Bytes that follow: 0 for ACK, 4 for ERR (PNIOStatus), payload block length for DATA |

### DATA blocks (AlarmNotification / AlarmAck)

- **AlarmNotification** (BlockType `0x0001` High / `0x0002` Low, length ≥25): carries `AlarmType`, API, slot/subslot, module/submodule identifiers, `AlarmSpecifier`, USI, and USI-specific data. `AlarmSpecifier` bits: 0–10 `SequenceNumber` (per-AR counter from 0, wraps at `0x7FF`), 11 `ChannelDiagnosis` (set while channel diagnosis exists on this submodule *after* the alarm), 13 `SubmoduleDiagnosisState` (set while any diagnosis on the submodule), 15 `ARDiagnosisState` (set while any diagnosis on the AR).
- **AlarmAck** (BlockType `0x8001` High / `0x8002` Low, length 18): echoes `AlarmType`, API, slot/subslot, `AlarmSpecifier`, and includes `PNIOStatus` (captured: `0x00000000` = OK).
- **ChannelDiagnosis** payload (USI `0x8000`): `ChannelNumber`, `ChannelProperties` u16, `ChannelErrorType` u16. `ChannelProperties` bits: 0–7 `Type` (we send 0 = unspecified), bit 8 `Accumulative` (0), bits 9–10 `Maintenance` (00=fault, 01=maintenance required, 10=maintenance demanded), bits 11–12 `Specifier` (01=appears, 10=disappears, 11=disappears-but-others-remain), bits 13–15 `Direction` (1=input, 2=output, 3=both). Captured values: `0x2801` (input, appears, fault), `0x3801` (input, disappears-but-others-remain), `0x2001` (input, all disappear).

### ERR-RTA format

`VarPartLen 4`: `PNIOStatus` = `CF 81 FD xx` where `xx` is ErrorCode2:
- `0x00`: device-initiated abort (no specific error).
- `0x0B`: AR alarm.ind(err) — peer sent an unexpected alarm.
- `0x11`: AR removed — AR aborted or timed out.

### I&M0 record (index `0xAFF0`, 60 bytes)

BlockType `0x0020`, length 56 + 4 header = 60. Layout: `VendorID` (2B), `OrderID` (20B ASCII), `IM_Serial_Number` (16B ASCII), `IM_Hardware_Revision` (2B), `IM_Software_Revision` (4B: prefix char + `functional_enhancement`/`bug_fix`/`internal_change`), `IM_Revision_Counter` (2B), `IM_Profile_ID` (2B), `IM_Profile_Specific_Type` (2B), `IM_Version` (1B.1B = `1.1`), `IM_Supported` (2B bitmask: bit 1=I&M1, bit 2=I&M2, bit 3=I&M3). Captured p-net identity in `im0_read_res.hex`: VendorID `0x0493`, OrderID `12345 Abcdefghijk` (space-padded to 20), 16-byte serial, hardware rev 1, software `V0.2.0`, revision counter 0, profile ID 0, version `1.1`, supported mask `0x000E` (bits 1–3 set, all three writable records).

### Negotiated AlarmCR (CPU 1515-2 PN, TIA V21)

From the Connect request (`plan5-alarm.pcapng` frame 47341): `AlarmCRType 1`, `LT 0x8892`, `AlarmCRProperties 0` (user priority 0), `RTATimeoutFactor 1` (= 100 ms between retries), `RTARetries 3`, `LocalAlarmReference 0x0000`, `MaxAlarmDataLength 256`, alarm tag headers `0xC000` (High) and `0xA000` (Low). Device's response: `LocalAlarmReference 0x0000`, `MaxAlarmDataLength 200` (smaller than the CPU's request, honored by both sides).

### Which submodule answers I&M reads

- **I&M0 (`0xAFF0`)** readable on **every** submodule (DAP slot 0/subslot 1, interface slot 0/subslot 0x8000, every module), answered with the same content (captured: TIA reads it on DAP, interface, and each module's slot/1).
- **I&M1–I&M3 (`0xAFF1`–`0xAFF3`)** writable/readable **only on the DAP** slot 0/subslot 1; any other submodule → PNIORW "invalid index". TIA's capture shows no writes to I&M1–3 because p-net's GSDML lacks `Writeable_IM_Records` (a Plan 5 addition).
- **TIA never wrote I&M1–I&M3** in the capture (p-net GSDML read-only); the codec and state machine must still support Write to these indices on the DAP for when GSDML is updated.


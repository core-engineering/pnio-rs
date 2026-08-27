# Spec — Plan 3: `rpc` + `cm` — AR establishment (DCE-RPC, Connect → ApplicationReady)

Date: 2026-08-27. Status: design validated in brainstorm, awaiting user review.
Parent: [`2026-06-25-profinet-rt-device-design.md`](2026-06-25-profinet-rt-device-design.md) §5.1 (`cm`), §6, §7.

## 1. Goal

Bring the IO-Device from "discovered by DCP" to **AR state DATA** against a real S7-1500:
receive and answer the controller's DCE-RPC `Connect`, `Write` (parameters), `Control(PrmEnd)`,
then act as RPC **client** to send `Control(ApplicationReady)` and get it acknowledged.

**Ground truth**: bench of 2026-08-27 (CPU 1515-2 PN FW V2.9.4 ↔ p-net v0.2.0), reference
capture `captures/ar-connect-2026-08-27-164334.pcapng` (see `docs/bench-pnet-device.md` §6b).
Every codec is validated byte-exact against those frames.

**Success criteria**
1. Unit + replay tests: our responses are byte-identical to p-net's for the reference exchange
   (modulo our MAC / station name, injected from the golden frames in tests).
2. HIL: `examples/ar_bringup` on the edge logs `AR state: Data` and the capture shows the CPU's
   `Application Ready … Done`. The CPU then aborts the AR after ~96 ms (no cyclic frames yet):
   **expected**, and it is the entry point of Plan 4.

## 2. Scope

In:
- `rpc`: DCE-RPC v4 connectionless header, NDR array header, PNIO UUIDs, UDP transport
  (server 34964 + the ApplicationReady call), mock transport.
- `cm`: PNIO block codecs (Connect req/res, Write req/res incl. MultipleWrite, Control req/res,
  IOX ApplicationReady), `DeviceModel`, pure AR state machine.
- `dcp::set`: DCP Set **IP suite** (Set Ok when the requested IP equals the current one; never
  touches the interface). The CPU sends it before Connect even when the IP already matches.
- `device`: the acyclic thread (single `poll` loop over the AF_PACKET and UDP sockets, timers).
- `eth` follow-ups needed by the loop: bind `sll_protocol = 0x8892`, PROFINET multicast
  membership (`01:0e:cf:00:00:00`), `recv` timeout via `poll`.
- `examples/ar_bringup.rs`: first runnable device, cloning the p-net sample identity so the TIA
  project does not change.
- `docs/cm-golden-frames.md`, FOLLOWUPS update, bench doc §6c (HIL result).

Out (tracked in FOLLOWUPS at close-out):
- RPC fragmentation (rejected explicitly), `Read` / `ReadImplicit` (opnum 2/5), device-initiated
  `Release`, `ModuleDiffBlock` (a config mismatch rejects the Connect with an explicit status),
  L2 alarm channel / ERR-RTA (Plan 5), cyclic frames (Plan 4), VLAN `PACKET_AUXDATA` (Plan 4),
  our own GSDML (Plan 6), record content interpretation (PDInterfaceAdjust etc. are stored raw).

## 3. Decisions (locked in brainstorm)

| Subject | Decision | Why |
|---|---|---|
| Plan 3 target | AR up to ApplicationReady acknowledged, validated by replay **and** HIL | First real contact with the CPU; abort after 96 ms is the measurable, expected outcome |
| DCP Set | minimal Set IP in this plan | Required for HIL; the CPU always sends it |
| HIL identity | clone of the p-net sample (Vendor `0x0493`, Device `0x0002`, `rt-labs-dev`, DAP `0x1`, modules `0x30/0x31/0x32/0x40`) | Zero TIA change, frame-by-frame comparison with p-net possible |
| Architecture | separate `rpc` and `cm` modules, one blocking acyclic thread, no async runtime | Each layer has its own golden frames; `cm` is testable without I/O; RPC is reused by Plans 5-6 |
| Byte order | parse in the request's DREP, always **emit big-endian** | Observed: CPU requests are LE-headed, p-net's BE responses are accepted |
| Logging | add the `log` facade to the crate; `env_logger` in the example only | No logging today; the facade is free when no logger is installed |

## 4. Architecture

```
crates/profinet-rt/src/
  eth/        existing  + sll_protocol 0x8892, multicast membership, poll-based recv timeout
  dcp/        existing  + set.rs         DCP Set IP (parse, Set Ok / Set error)
  rpc/        new       header.rs        DCE-RPC v4 CL header (80 bytes), Drep
                        ndr.rs           NDR array header (request / response)
                        uuid.rs          Uuid + PNIO interface/object UUID helpers
                        transport.rs     RpcTransport trait + MockRpcTransport
                        udp.rs           UdpRpcTransport (std UdpSocket, port 34964)
  cm/         new       status.rs        PnioStatus (ErrorCode/Decode/Code1/Code2 + named ctors)
                        block.rs         BlockHeader + Connect/AlarmCR/IOCR/ExpectedSubmodule codecs
                        connect.rs       ConnectReq parse + validation vs DeviceModel + ConnectRes build
                        write.rs         IODWriteReq / MultipleWrite parse, IODWriteRes build
                        control.rs       IODControlReq/Res (PrmEnd), IOXBlockReq/Res (ApplicationReady)
                        model.rs         DeviceModel (identity, DAP, slots/subslots, IO lengths)
                        ar.rs            pure AR state machine: Event -> Vec<Action>
                        mod.rs           Cm: RPC datagram -> Event -> Actions, response cache
  device/     new       mod.rs           Device::run: poll loop, dispatch dcp/cm, timers, stop flag
examples/ar_bringup.rs  HIL binary (clap + env_logger)
tests/ar_replay.rs      reference capture replay (embedded hex), byte-exact responses
docs/cm-golden-frames.md
```

Rules:
- `rpc` and `cm` own **no socket**: pure `&[u8] -> Result<T, Error>` codecs and an `Action` list to
  execute. All I/O lives in `device` behind the mockable `EthTransport` / `RpcTransport` traits.
- `cm::ar` has no I/O and no clock: `fn on(&mut self, ev: Event, now: Instant) -> Vec<Action>`.
- `DeviceModel` is plain data. The HIL example fills it by hand; Plan 6 will generate it.

## 5. Codecs

### 5.1 `rpc::header` — DCE-RPC v4 connectionless, 80 bytes
`version=4`, `ptype` (Request 0, Ping 1, Response 2, Fault 3, Working 4, Nocall 5, Reject 6,
Ack 7, …), `flags1`, `flags2`, `drep[3]`, `serial_hi`, `object_uuid`, `interface_uuid`,
`activity_uuid`, `server_boot`, `if_version`, `seq_num`, `opnum`, `ihint`, `ahint`, `frag_len`,
`frag_num`, `auth_proto`, `serial_lo`.
- Integer fields and the first three UUID fields follow `drep[0]` bit 4 (0 = BE, 1 = LE). A `Drep`
  value is threaded through parse/build. Observed: CPU requests `drep = 10 00 00` (LE), p-net
  responses `00 00 00` (BE), CPU's ApplicationReady response follows the CPU's own DREP.
- Flags observed: requests `flags1 = 0x20` (idempotent); device responses `0x28`
  (idempotent | no-fack); CPU response to our call `0x0a` (last-frag | no-fack). We emit `0x28`
  on responses and `0x20` on our request.
- `frag_num != 0` or `flags1 & FRAG` → `RpcError::Fragmented` (not supported; the reference
  exchange peaks at 577 bytes).
- Constants: device interface `dea00001-6c97-11d1-8271-00a02442df7d`, controller interface
  `dea00002-6c97-11d1-8271-00a02442df7d`, object UUID
  `dea00000-6c97-11d1-8271-{instance:04x}{device:04x}{vendor:04x}` (observed
  `…-8271-000100020493`). Opnums: 0 Connect, 1 Release, 2 Read, 3 Write, 4 Control,
  5 ReadImplicit.

### 5.2 `rpc::ndr`
Request body: `args_max`, `args_len`, `max_count`, `offset`, `actual_count` (5 × u32 in DREP) then
the PNIO blocks. Response body: `status` (u32), `args_len`, `max_count`, `offset`, `actual_count`,
blocks. `actual_count` must equal `args_len` and fit the datagram, else `RpcError::NdrMismatch`.

### 5.3 `cm::block` — PNIO blocks (always big-endian)
`BlockHeader { block_type: u16, block_length: u16, version: (1, 0) }`.
Parsed (requests): `ARBlockReq` (ar_type, ar_uuid, session_key, initiator_mac,
initiator_object_uuid, ar_properties, activity_timeout_factor, initiator_udp_rt_port,
station_name), `IOCRBlockReq` (type, reference, lt, properties, data_length, frame_id,
send_clock_factor, reduction_ratio, phase, sequence, frame_send_offset, watchdog_factor,
data_hold_factor, tag_header, multicast_mac, APIs with IODataObject/IOCS frame offsets),
`ExpectedSubmoduleBlockReq` (per API/slot: module_ident, submodules with ident, properties,
input/output data descriptors), `AlarmCRBlockReq` (type, lt, properties, rta_timeout_factor,
rta_retries, local_alarm_reference, max_alarm_data_length, tag headers).
Built (responses): `ARBlockRes` (ar_type, ar_uuid, session_key, responder_mac,
responder_udp_rt_port = `0x8892`), `IOCRBlockRes` ×2 (type, reference, frame_id echoed),
`AlarmCRBlockRes` (type, our local_alarm_reference, max_alarm_data_length),
`ARServerBlockRes` (station_name_length, our station name, 4-byte padding).
No `ModuleDiffBlock`: a mismatch rejects the Connect (§6).

### 5.4 `cm::write`
`IODWriteReqHeader` (seq_number, ar_uuid, api, slot, subslot, index, record_data_length, 24-byte
padding) + data. `MultipleWrite` (index `0xe040`) nests complete header+data records, each aligned
to 4 bytes. Response: one `IODWriteResHeader` (additional_value_1/2, status) per record, nested
the same way. Records are accepted and **stored raw** in `ArContext.records`; no interpretation.

### 5.5 `cm::control`
`IODControlReq` (ar_uuid, session_key, control_command `0x0001` PrmEnd, block_properties) →
`IODControlRes` with command `0x0008` Done. `IOXBlockReq` (device → controller, command `0x0002`
ApplicationReady) → `IOXBlockRes` Done from the controller.

### 5.6 `dcp::set`
Frame ID `0xfefd`, service Set (`0x04`), request: IP suite block (option 1 / suboption 2,
`BlockQualifier` permanent/temporary, ip / mask / gateway). Response: Set (`0x04` / response)
with a Control/Response block (option 5 / suboption 4) carrying the addressed option/suboption
and a `BlockError`: `0x00` Ok if the requested IP equals the interface's current IP, else
`SetNotPossible`; any other option → `SuboptionNotSupported`. The interface is never modified
(the edge is also the TIA gateway; same policy as the p-net guard script).

### 5.7 Golden frames (`docs/cm-golden-frames.md`)
From `ar-connect-2026-08-27-164334.pcapng`: #50/#53 Connect req/res, #54/#56 Write, #57/#58 PrmEnd,
#59/#60 ApplicationReady, #47/#48 DCP Set req/res, #37 p-net Ident Ok (Dev-Role ≠ 0, settles that
follow-up). Hex-pinned with provenance, embedded in tests.

## 6. AR state machine (`cm::ar`)

```
          ConnectReq ok              PrmEndReq                AppReadyRsp ok
 Idle ────────────────► Connected ───────────► AppReadySent ──────────────► Data
  ▲  (rejected: stay      │ WriteReq → WriteRes     │ timeout ×3 / bad status     │ ReleaseReq
  │   Idle, error sent)   │                         ▼                             │ Abort(reason)
  └──────────────────────── Abort → Idle, Notify(Offline{reason}) ◄───────────────┘
```

`ArContext` (created on Connect, dropped on Abort): `ar_uuid`, `session_key`, `initiator_mac`,
`initiator_addr` (IP:port of the RPC), `initiator_object_uuid`, `iocr: [IocrParams; 2]`
(frame_id, data_length, reduction_ratio, watchdog_factor, data_hold_factor, frame offsets),
`alarm_cr` (references, max length), `records: Vec<Record>`, `created_at`.

Events: `ConnectReq`, `WriteReq`, `PrmEndReq`, `ReleaseReq`, `AppReadyRsp { status }`,
`Tick`, `Abort(reason)`. Actions: `Respond { bytes, to }`, `CallController { bytes, to,
activity }`, `Notify(ArState)`, `SetTimer(id, duration)`, `ClearTimer(id)`.

Connect validation (each failure → explicit `PnioStatus`, never silent):
- exactly one `ARBlockReq`, `ar_type = 0x0001` (IOCAR single), non-nil `ar_uuid`;
- exactly two IOCR (Input + Output), frame IDs in the RTC1 unicast range (`0x8000..=0xBBFF`),
  `data_length` consistent with the model's IO lengths + IOPS/IOCS; `reduction_ratio`,
  `watchdog_factor`, `data_hold_factor`, frame offsets stored for Plan 4;
- every expected (slot, subslot, module_ident, submodule_ident, input/output length) exists
  identically in `DeviceModel`;
- exactly one `AlarmCRBlockReq`;
- Connect outside `Idle` with a different `ar_uuid` → rejected "AR already established"; same
  `ar_uuid` and same RPC `seq_num`/activity → the cached response is resent (idempotent).

After PrmEnd: `Respond(Done)` then `CallController(ApplicationReady)` to
`initiator_ip:34964` (new activity UUID, initiator's object UUID, controller interface UUID,
opnum 4, BE). Timer 1 s, 3 attempts (mirrors the CPU's `RTARetries = 3`), then
`Abort(AppReadyFailed)`.

Activity timeout: no RPC traffic for `activity_timeout_factor × 100 ms` (observed 200 → 20 s)
while not in `Data` → `Abort(ActivityTimeout)`.

`Data`: nothing more in Plan 3; `Notify(ArState::Data)` exposes the IOCR parameters — Plan 4's
input. `Release` (opnum 1) accepted in any state ≠ Idle → `Respond(ok)` + Abort.

Every transition is logged (`log::info!`) with previous state, event, next state, reason.

## 7. Network, acyclic thread, HIL example

**`rpc::transport`**: `trait RpcTransport { fn recv(&self, timeout: Option<Duration>) ->
Result<Option<(Vec<u8>, SocketAddr)>, RpcError>; fn send(&self, buf: &[u8], to: SocketAddr) ->
Result<(), RpcError>; }` + `MockRpcTransport` (queues). `UdpRpcTransport` binds
`0.0.0.0:34964`; the ApplicationReady call is sent **from the same socket** (source port 34964,
the controller answers to the source). If the CPU refuses that in HIL, fallback: a dedicated
ephemeral client socket (design point to confirm on the bench).

**`device::Device::run(cfg, eth, rpc, stop)`** — single loop:
```
poll([eth_fd, udp_fd], until next timer deadline)
eth readable  → dcp::handle (Identify, Set) → L2 responses
udp readable  → rpc::parse → cm.on(Event) → execute Actions
timer expired → cm.on(Tick)
stop flag     → clean exit
```
`eth` prerequisites done here: `sll_protocol = htons(0x8892)` at `open` (no more spinning on
foreign traffic), `PACKET_ADD_MEMBERSHIP` for `01:0e:cf:00:00:00` (without it the CPU's
Identify never reaches a non-promiscuous socket), `recv` timeout through `poll`. The CPU's DCP
arrives VLAN-tagged (prio 0); with RX VLAN offload on, the kernel strips the tag and `recv` sees
plain `0x8892` — `PACKET_AUXDATA` stays in Plan 4.

**`examples/ar_bringup.rs`**: `--iface eno2 --name rt-labs-dev`; builds the p-net clone
`DeviceModel` (Vendor `0x0493`, Device `0x0002`, DAP `0x1` with subslots `1/0x8000/0x8001`,
slots 1-4 idents `0x30/0x31/0x32/0x40`, IO lengths 1 / 1 / 1+1 / 8+8); logs every AR transition;
**success = `AR state: Data`**, followed by the expected abort. Runs on the edge with
`cap_net_raw,cap_net_admin`; build on the edge (rustup) or `x86_64-unknown-linux-musl` from WSL
if glibc versions differ.

**HIL procedure**: stop `pn_dev`, start `ar_bringup`, capture with `tcpdump` (no filter), compare
our responses frame-by-frame with p-net's (`cm-golden-frames.md`). The TIA project is unchanged.

## 8. Errors and edge cases

Types (`thiserror`):
- `RpcError`: `TooShort`, `BadVersion`, `Fragmented`, `UnsupportedPtype`, `BadInterface`,
  `NdrMismatch`, `Io`.
- `CmError`: `Block(BlockError)` (unknown type, inconsistent length, version ≠ 1.0),
  `Reject(PnioStatus)`, `WrongState { event, state }`, `UnknownAr(Uuid)`.
- `PnioStatus`: `u32` structured as `ErrorCode / ErrorDecode / ErrorCode1 / ErrorCode2`, with
  named constructors for every emitted case (`connect_ar_block_invalid`, `connect_iocr_invalid`,
  `connect_expected_submodule_mismatch { slot, subslot }`, `write_index_unsupported`,
  `control_wrong_state`, `service_unsupported`, `ok`). Exact values from IEC 61158-6-10, each
  constant commented with its origin.

Policy (in `device`, not in the codecs):
- Unparsable datagram → **log + drop**; never panic, never kill the loop (same rule as DCP RX).
- Parsable but refused → **RPC response with non-zero `PnioStatus`** (visible in the CPU's
  diagnostic buffer).
- Unsupported opnum (Read, ReadImplicit) → `service_unsupported` response. Release is accepted.
- Retransmitted request (same activity + `seq_num`) → cached response resent (one entry per
  activity).
- ApplicationReady: bad status or 3 timeouts → `Abort(AppReadyFailed)`; the CPU restarts Identify.
- DCP Set: always a response, always a log line; interface never modified.
- No residual state between ARs: Connect → Abort → Connect with a new `ar_uuid` must succeed
  (dedicated test).

## 9. Tests

1. **Codecs, byte-exact vs golden** (unit, TDD): header parse/build LE (#50) and BE (#59);
   fragment/version refusal; NDR counters both DREPs and `NdrMismatch`; Connect #50 → expected
   structs, response → #53; Write #54 → 5 records (`0xe040` container, `0x8071`, `0x7b`, `0x7c`,
   `0x7d`), response → #56; PrmEnd #57 → #58; ApplicationReady build → #59 (activity injected),
   parse #60 → OK; DCP Set #47 → #48, different IP → `SetNotPossible`, name option →
   `SuboptionNotSupported`; `PnioStatus` round-trips.
2. **State machine** (unit, no I/O): state × event table, one test per cell; nominal
   Idle → Data; rejected Connect stays Idle; idempotent retransmission; 3 AppReady failures →
   Abort; Release → Abort; Connect after Abort succeeds.
3. **Replay integration** (`tests/ar_replay.rs`): the reference exchange embedded as hex, fed
   through `MockRpcTransport` + `MockTransport`; every `Respond` byte-exact vs p-net's frame;
   final state `Data`.
4. **HIL** (manual, documented in `docs/bench-pnet-device.md` §6c with the capture).

Exit criteria: suite green, `clippy --all-targets -D warnings`, `fmt`; HIL passed;
`docs/cm-golden-frames.md`; FOLLOWUPS updated — closed: `sll_protocol`, `recv` timeout,
DeviceRole ≠ 0; opened: RPC fragmentation, ModuleDiffBlock, Read/ReadImplicit, `PACKET_AUXDATA`.

## 10. Dependencies

Crate: `log`. Example / dev only: `env_logger`, `clap`. Nothing else.

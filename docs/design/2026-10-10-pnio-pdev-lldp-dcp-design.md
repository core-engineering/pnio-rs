# Spec — Plan 8: PDEV (`lldp` + `pdev`), DCP Set/Signal/Reset, GSDML V2.31+ profile

Date: 2026-10-10. Status: design approved in brainstorm, not implemented.
Parent: [`2026-06-25-profinet-rt-device-design.md`](2026-06-25-profinet-rt-device-design.md) §5.2 (thread model: everything here runs on the acyclic thread).
Builds on Plans 2-7: `dcp` answers Identify and a guarded Set-IP; `device::step` multiplexes the acyclic `AF_PACKET` socket, the RPC socket and the RT runner; `cm::records` serves I&M0-3; `diag::DiagStore` drives Diagnosis / DiagnosisDisappears alarms; `gsdml::render` emits a V2.3 GSD.
Ground truth: none yet. Task 0 of the plan is a capture campaign on the 1515-2 PN bench (X1, TIA V21) that produces every golden this plan needs (§8). No reference device (ET200SP) is available: our *responses* are validated structurally and through Wireshark's `pn_io`/`lldp` dissectors at the HIL, the CPU's diagnostic buffer being the oracle for semantics.

## 1. Goal

TIA V21 treats the device as a full topology node and writes its I&M1-3 records. The device answers the three DCP services a commissioning engineer uses from *Online & diagnostics* (assign name, flash, reset to factory), persists what it is told, emits and consumes LLDP, serves the PDEV records TIA reads, and raises a *remote mismatch* diagnosis when it is cabled to the wrong port. This is also the PDEV groundwork for S2 system redundancy (Plan 9).

**Success criteria**
1. GSDML `PNIO_Version="V2.31"` with `LLDP_NoD_Supported`, `ResetToFactoryModes`, `CertificationInfo`, `SupportedProtocols="LLDP"`, DCP/PTP boundary flags — accepted by the TIA GSD checker and by `lxml` against the v2.4 XSD.
2. TIA writes I&M1-3 (Plan 5 check 2, previously ✗) and reads them back after a device restart.
3. TIA's topology view is green with the device on X1; moving the cable produces *remote mismatch* in the CPU diagnostic buffer, moving it back clears it.
4. DCP Set-NameOfStation / Set-IP from TIA are applied (name in Identify and ARServerBlock, IP on the Linux interface), persisted across restarts, and the AR is re-established by the CPU without manual action.
5. DCP Signal answers Ok and is observable; DCP ResetToFactory modes 2 and 4 blank the name/IP (and I&M1-3 for mode 4) and TIA re-assigns the device.
6. TIA's per-second reads of `0xF841` / `0xF842` no longer answer "invalid index".
7. 10 minutes at 500 µs with LLDP running and a mismatch diagnosis active: zero missed ticks (non-regression of Plan 7).
8. Everything existing stays green (`dcp_replay`, `ar_replay`, `rt_replay`, `alarm_replay`, `typed_replay`, examples).

## 2. Scope

In:
- `dcp`: Set-NameOfStation (2/2), Set-IP (1/2, unguarded), Signal (5/3), ResetToFactory (5/6); `NetParams`; `NetStore` (file persistence); `IpConfigurator` (apply the IP suite to the interface).
- `eth`: `AfPacketTransport::open_with(iface, EthProto)` — any EtherType plus optional multicast membership; per-transport accept predicate.
- `lldp` (new, pure codec): LLDPDU encode/parse with PROFINET and IEEE 802.3 organisationally-specific TLVs.
- `pdev` (new): port/interface state, peer tracking with TTL, `PDPortDataCheck` / `PDPortDataAdjust` / `PDInterfaceAdjust` storage, `PDPortDataReal` / `PDInterfaceDataReal` / `PDRealData` / `PDExpectedData` builders, peer evaluation → diagnosis.
- `cm::records`: index dispatch table for Read and Write; per-record status in the MultipleWrite response; `Cm::set_station_name`.
- `diag`: `ExtChannelDiagnosis` (USI `0x8002`) construction; `DiagStore` entries with an extended error type.
- `device`: second socket (LLDP) in `wait_any_readable`, `pdev` deadline and tick, mutable `NetParams`, abort-on-rename, LLDP shutdown PDU on stop.
- `api`: `StartOptions` (`net_store`, `apply_ip`, `lldp`, `on_signal`), `DeviceHooks`, `IoDevice::{net_params, signal_count, peer}`.
- `gsdml` / `config`: V2.31+ attributes, golden regenerated.
- Docs, goldens, replay tests, HIL §6j, release 0.2.0.

Out (recorded in `FOLLOWUPS.md` at close-out):
- SNMP (LLDP-MIB), MRP, PTP / IRT, `PDIRData`, `PDSyncData`, delay measurement.
- Process alarms, plug/pull alarms, `ModuleDiffBlock` (unchanged from Plan 5).
- Record indices `0xF820` (ARData), `0xF00C`, diagnosis record reads (`0x800A..`, `0xF80C`).
- Rejecting Write records on unknown indices (today's lenient ACK is kept, see §5.5).
- ResetToFactory mode 8 (device restart) and modes 1 / 3 — answered `SuboptionNotSupported`.
- A second physical port. `pdev` is written for one port (subslot `0x8001`); the record builders take the port state as a parameter so a second port is a loop, not a rewrite.
- Applying `AdjustLinkState` (forcing the link down) and `AdjustMAUType`: stored and read back, never acted on.

## 3. Decisions (locked in brainstorm)

1. **One thread, two modules.** `lldp` and `pdev` run on the acyclic thread inside `device::step`, like `alarm`/`diag`. No new thread, no tokio, RT thread untouched. (Rejected: an LLDP thread — nothing to parallelise at one frame per 5 s; delegating to `lldpd` — no PROFINET TLVs on transmit, no mock.)
2. **DCP Set-Name / Set-IP are applied and persisted** (user choice). Temporary qualifier = applied, not persisted. The IP suite is pushed to the Linux interface through `IpConfigurator` (default: the `ip` command; the binary already needs `CAP_NET_ADMIN`).
3. **A Set that changes the name or IP while an AR exists is accepted and aborts the AR** (p-net behaviour; the CPU re-identifies and reconnects). Verified at the HIL (§7 check 4).
4. **ResetToFactory modes 2 and 4 only, no restart.** Mode 2 blanks name + IP and clears the PDEV check/adjust records; mode 4 additionally clears I&M1-3. The process keeps running; the AR is aborted.
5. **Signal = Ok + `log::info!` + counter + optional `on_signal` hook.** Blinking anything is the application's business.
6. **LLDP complete**: transmit, receive, `PDPortDataReal` with the peer, `PDPortDataCheck` evaluated, *remote mismatch* diagnosis (user choice; the ET200SP behaviour).
7. **`PNIO_Version="V2.31"`**: the lowest value that triggers the checker rules we now satisfy. Higher versions add mandates (and nothing we need). Hypothesis H1 — *TIA ignores `Writeable_IM_Records` under V2.3* — is what success criterion 2 tests; §7 has the fallback if it is false.
8. **Lenient Write ACK on unknown indices is kept** (HIL-proven); the new per-record status infrastructure only reports errors on indices we handle. Tightening is a follow-up.
9. **Goldens first** (task 0): no code beyond the GSDML bump before the capture campaign has run.

## 4. Wire formats

Pinned from the capture campaign (§8) where marked *(capture)*; from IEC 61158-6-10 / the Wireshark dissector otherwise. Any value marked *(verify)* is confirmed against the capture before the corresponding task is implemented.

### 4.1 DCP blocks (Set request, FrameID `0xFEFD`, Option/Suboption)

| Block | Opt/Sub | Payload | Notes |
|---|---|---|---|
| NameOfStation | 2/2 | BlockQualifier u16 (bit 0: 1 = permanent, 0 = temporary), name bytes | *(capture)* TIA *Assign name* |
| IPParameter | 1/2 | BlockQualifier u16, IP, subnet, gateway (4×3) | already decoded |
| Signal | 5/3 | BlockQualifier u16, SignalValue u16 = `0x0100` (flash once) | *(capture)* TIA *Flash LED* |
| ResetToFactory | 5/6 | BlockQualifier u16 — the reset mode, encoded as `mode << 1` *(verify)*: `0x0004` = mode 2, `0x0008` = mode 4 | *(capture)* TIA *Reset to factory settings* with and without "retain I&M data" |

Response: one Control/Response block (5/4) per request block, `[option, suboption, BlockError]`, as today. `BlockError`: `0x00` Ok, `0x02` SuboptionNotSupported, `0x03` SuboptionNotSet (invalid value), `0x05` SetNotPossible.

NameOfStation syntax (IEC 61158-6-10 §4.3.1.4.15, DNS-label form): 1-240 bytes, labels of `[a-z0-9-]` separated by `.`, each label 1-63 chars, no leading or trailing `-`, not of the form `port-xyz` / `port-xyz-abcde`, not an IPv4 dotted quad. Uppercase is rejected (TIA lowercases on its side). Empty name is valid only through ResetToFactory.

### 4.2 LLDPDU (EtherType `0x88CC`, destination `01:80:c2:00:00:0e`, untagged)

Transmitted by the device, in order:
1. Chassis ID (type 1), subtype 7 (locally assigned) = NameOfStation; if the name is empty, subtype 4 (MAC address).
2. Port ID (type 2), subtype 7 = `port-001` **or** `port-001.<NameOfStation>` *(capture: mirror the 1515's format on X1; the same string is what `PDPortDataCheck.CheckPeers` carries, so the two must agree)*.
3. TTL (type 3) = 20 s. Shutdown PDU on `stop()`: TTL 0, carrying only Chassis ID, Port ID, TTL and End (the dissector flags any other TLV in a TTL-0 PDU).
4. PROFINET TLVs (type 127, OUI `00-0E-CF`): subtype 1 `PNIO_Delay` (RxDelayLocal, RxDelayRemote, TxDelayLocal, TxDelayRemote, CableDelayLocal = 5 × u32, all 0), subtype 2 `PNIO_PortStatus` (RTClass2 u16 = 0, RTClass3 u16 = 0), subtype 5 `PNIO_ChassisMAC` (6 bytes).
5. IEEE 802.3 TLV (type 127, OUI `00-12-0F`, subtype 1 MAC/PHY): autoneg support/status `0x03`, PMD advertised `0x6C00` (10/100 HD+FD; `0x6c01` would also claim 1000BASE-T FD — pinned from the dissector, see `docs/pdev-golden-frames.md` §3.3), operational MAU type `0x0010` (100BASE-TX full duplex).
6. End (type 0).

Transmit period 5 s (TTL/4), plus one immediate LLDPDU after a name change. Received LLDPDUs: parsed leniently, unknown TLVs skipped; a PDU with TTL 0 removes the peer; a peer expires `ttl` seconds after its last PDU.

### 4.3 PDEV records (IEC 61158-6-10, BlockVersion 1.0 unless stated)

BlockType numbers below are from memory of the standard; every one of them is pinned from the Wireshark dissector in `docs/pdev-golden-frames.md` during task 0 before the record tasks start, and the document, not this list, is the reference.

Read, on subslot `0x8001` (port) or `0x8000` (interface):
- `0x802A` **PDPortDataReal** (BlockType `0x020F`): SlotNumber, SubslotNumber, LengthOwnPortID, OwnPortID (padded to 2), NumberOfPeers (0 or 1), per peer: LengthPeerPortID, PeerPortID, LengthPeerChassisID, PeerChassisID, LineDelay u32 = 0, PeerMACAddress; then MAUType u16, Reserved u16, DomainBoundary u32, MulticastBoundary u32, LinkState u16 (bits 0-7 port state: 1 up, 2 down; bits 8-15 link state: 0), Reserved u16, MediaType u32 = 1 (copper) *(field order and padding are pinned from the dissector in `docs/pdev-golden-frames.md` §2.3: BlockVersion 1.0 only; that document, not this sentence, is the layout reference)*.
- `0x8080` **PDInterfaceDataReal** (BlockType `0x0240`): LengthOwnChassisID, OwnChassisID, MACAddress, IPAddress, Subnetmask, StandardGateway.
- `0x802B` / `0x802F`: the stored Check / Adjust record, byte-identical to what was written; when nothing has been written the response carries no block (an engineered-but-empty port), never "invalid index".
- `0xF841` **PDRealData** and `0xF842` **PDExpectedData**: `MultipleBlockHeader` (BlockType `0x0400`: API, SlotNumber, SubslotNumber) wrapping, respectively, PDPortDataReal + PDInterfaceDataReal, and PDPortDataCheck + PDPortDataAdjust, one header per (slot, subslot) *(capture: what TIA reads every second, `docs/bench-pnet-device.md:1109`)*.

Write:
- `0x802B` **PDPortDataCheck** (BlockType `0x0200`): SlotNumber, SubslotNumber, then sub-blocks. **CheckPeers** (`0x020A`): NumberOfPeers, per peer LengthPeerPortID, PeerPortID, LengthPeerChassisID, PeerChassisID. **CheckMAUType** (`0x020C`): MAUType. Others (`CheckLineDelay`, `CheckLinkState`, `CheckSyncDifference`, `CheckMRP*`) stored, not evaluated.
- `0x802F` **PDPortDataAdjust** (BlockType `0x0202`): sub-blocks `AdjustDomainBoundary`, `AdjustMulticastBoundary`, `AdjustMAUType`, `AdjustLinkState`, `AdjustPeerToPeerBoundary`, `AdjustDCPBoundary`, `AdjustPreambleLength` — stored raw, read back, no behaviour.
- `0x8071` **PDInterfaceAdjust** (BlockType `0x0250`): MultipleInterfaceMode — stored raw (already present in the Plan 3 golden Connect).

MultipleWrite response: one `WriteRes` per record with its own `PnioStatus`; the outer status stays Ok.

### 4.4 Remote-mismatch diagnosis

Channel diagnosis on slot 0, subslot `0x8001`, ChannelNumber `0x8000` (submodule), ChannelProperties: type 0, direction 0, maintenance = Fault (severity bits 00 → *fault*), ChannelErrorType `0x8001` *Remote mismatch* (pinned from the dissector: `0x8000` *Data transmission impossible* carries link-down/MAUType ext codes instead — `docs/pdev-golden-frames.md` §2.10), USI `0x8002` **ExtChannelDiagnosis** with ExtChannelErrorType *(verify against the dissector's names and the TIA diagnostic-buffer text)*:
- `0x8000` *Remote mismatch: peer name of station mismatch*
- `0x8001` *Remote mismatch: peer name of port mismatch*
- `0x8005` *Remote mismatch: no peer detected*

ExtChannelAddValue = 0. One entry per port at a time, priority: no peer > chassis mismatch > port mismatch. Raised/cleared through `DiagStore`, so the Diagnosis / DiagnosisDisappears alarms, the ProblemIndicator and the replay on reconnect are the Plan 5 mechanics unchanged.

## 5. Modules

### 5.1 `dcp`

- `NetParams { name: String, ip: [u8;4], subnet: [u8;4], gateway: [u8;4] }` — the single source of truth for the device's identity on the wire. `Device` owns it; `DcpConfig.properties` and `DeviceModel.station_name` are *derived* and refreshed through `Device::apply_net(&NetParams)` whenever it changes.
- `set.rs`: `SetBlock` gains `NameOfStation { permanent, name }`, `Signal { value }`, `ResetToFactory { mode }`. `decide_set(&[SetBlock], &NetParams, ar_active) -> Vec<SetDecision>` is pure: it validates (name syntax; IP suite well-formed — any suite is accepted, including `0.0.0.0/0/0`; signal value `0x0100`; reset mode ∈ {2, 4}) and returns, per block, the `BlockError` plus an `Effect` (`SetName{permanent}`, `SetIp{permanent}`, `Signal`, `Reset{mode}`). The old "IP must equal the configured one" rule is deleted with its test.
- `name.rs`: `validate_station_name(&str) -> Result<(), NameError>` with the §4.1 rules, unit-tested on the spec's examples.
- `store.rs`: `NetStore { path: Option<PathBuf> }`, `load() -> Option<NetParams>`, `save(&NetParams)` — text file, one `key=value` per line (`name`, `ip`, `subnet`, `gateway`, dotted quads), written to `.net-<pid>.tmp` then renamed, exactly `ImStore`'s recipe. Missing or malformed file → `None` + warning.
- `IpConfigurator` trait: `fn apply(&self, iface: &str, p: &NetParams) -> io::Result<()>`. `IpCommand` (default): `ip addr flush dev <iface>`, then if `ip != 0.0.0.0`: `ip addr add <ip>/<prefix> dev <iface>`, and if the gateway is neither `0.0.0.0` nor `ip`: `ip route replace default via <gw> dev <iface>`. `MockIp` records calls. Failure → the block answers `SetNotPossible`, nothing persisted.

Order of effects in `Device` for a Set with Ok decisions: apply (`IpConfigurator` for IP; in-memory for name) → persist if permanent → send the DCP response → if the name or IP changed and an AR exists, abort it (`AbortReason::DcpSet`, ERR-RTA as for any device-initiated abort) → immediate LLDPDU.

Startup: `NetStore::load()` wins; else `NetParams { name: cfg.station_name, ip: StartOptions.ip, subnet: /24, gateway: ip }` as today. The file is only created by the first permanent Set or Reset.

Reset: mode 2 → `NetParams { name: "", ip: 0, subnet: 0, gateway: 0 }` applied + persisted, `pdev.clear_engineering()`; mode 4 → mode 2 + `ImStore::clear()` (124 zero bytes, persisted). AR aborted (`AbortReason::ResetToFactory`). Identify then answers the AllSelector with an empty name, which is what TIA expects before re-assigning.

### 5.2 `eth`

- `EthProto { ethertype: u16, multicast: Option<[u8;6]> }`; `AfPacketTransport::open_with(iface, EthProto)`; `open(iface)` = `open_with(iface, EthProto::PROFINET)` (unchanged behaviour). The socket is bound to `htons(ethertype)`; `PACKET_ADD_MEMBERSHIP` on `multicast` if given.
- The `is_profinet_frame` post-filter becomes `accepts(&self, frame) -> bool`: EtherType equal to the transport's, untagged or behind one VLAN tag. `PACKET_OUTGOING` still dropped.
- No cBPF needed on the LLDP socket (the kernel binding already selects the EtherType).
- `MockTransport` unchanged (one instance per socket in tests).

### 5.3 `lldp` (pure)

- `Lldpdu { chassis_id: ChassisId, port_id: PortId, ttl: u16, pnio_delay, pnio_port_status, pnio_chassis_mac: Option<[u8;6]>, mac_phy: Option<MacPhy>, unknown: usize }`; `ChassisId`/`PortId` = `{ subtype: u8, value: Vec<u8> }` with `as_str()`.
- `encode(&self) -> Vec<u8>` (TLV order of §4.2), `parse(&[u8]) -> Result<Lldpdu, LldpError>` (stops at End or at the buffer end, counts unknown TLVs, errors only on a truncated TLV).
- `DeviceLldp::build(&NetParams, mac, port_name_fmt) -> Lldpdu` — the device's own PDU.
- `Peer { chassis_id: String, port_id: String, mac: [u8;6], mau_type: Option<u16>, seen_at: Instant, ttl: Duration }` + `Peer::from_pdu(&Lldpdu, src_mac, now)`.
- Goldens: the 1515's LLDPDU on X1 *(capture)*; p-net's LLDPDU if a `pn_dev` run is cheap at capture time. Round-trip tests; a test that our PDU decodes to the fields we put in; a test that the CPU golden parses into the expected `Peer`.

### 5.4 `pdev`

- `PortState { peer: Option<Peer>, check: Option<CheckRecord>, adjust: Option<Vec<u8>>, check_written_at: Option<Instant>, ever_seen_peer: bool }`, `IfaceState { iface_adjust: Option<Vec<u8>> }`, `Pdev { port: PortState, iface: IfaceState, link: Box<dyn LinkProbe>, next_tx: Instant, period: Duration }`.
- `LinkProbe` trait: `fn link_up(&self) -> bool`; `SysfsLink { iface }` reads `/sys/class/net/<iface>/carrier`; `MockLink`.
- `on_rx(&mut self, frame, now)`: parse, update `peer` (TTL 0 → `None`), `ever_seen_peer = true`.
- `tick(&mut self, now) -> Vec<PdevAction>`: expire the peer; emit `Send(lldpdu)` when `next_tx` is due; re-evaluate. `next_deadline()` = min(`next_tx`, peer expiry).
- `write_record(index, subslot, body) -> Result<(), PnioStatus>`: decodes CheckPeers / CheckMAUType, stores the rest raw; sets `check_written_at`; re-evaluates. `read_record(index, subslot, &NetParams, mac) -> Option<Vec<u8>>`: the §4.3 builders.
- `evaluate(now) -> Option<Mismatch>`: no `check` → `None`. With a check: peer present and both IDs equal → `None`; chassis differs → `Chassis`; port differs → `Port`; no peer → `NoPeer` only if `ever_seen_peer` (it expired) **or** `now - check_written_at ≥ 2 × period` (grace: the CPU's first LLDPDU may arrive after the Connect that wrote the check).
- `Pdev` emits `PdevAction::Diag(Option<Mismatch>)` when the evaluation changes; `Device` maps it to `DiagStore::raise_ext` / `clear` on (0, `0x8001`, `0x8000`).
- `clear_engineering()`: `check`, `adjust`, `iface_adjust` → `None`, evaluation cleared. Called on ResetToFactory and on AR loss (TIA rewrites them in every Connect).

### 5.5 `cm`

- `records.rs`: `read_record` dispatches on index through a `match` that now covers `0xAFF0..=0xAFF3` (unchanged), `0x802A`, `0x802B`, `0x802F`, `0x8080`, `0xF841`, `0xF842` (via `RecordCtx` gaining `pdev: &Pdev`, `net: &NetParams`, `mac`). Unknown → `read_index_unsupported` as today.
- Write: `Ar::handle_write` keeps storing records, but `build_write_res` takes `&[PnioStatus]` (one per record). `Cm` computes the statuses before answering: `0xAFF1..=0xAFF3` → `write_im_record` result (a failure now reaches the wire instead of being logged only); `0x802B`, `0x802F`, `0x8071` → `pdev.write_record` result (`write_invalid_parameter` on a malformed block); any other index → Ok (decision 8). Golden `write_req.hex` / `write_res.hex` still match (all Ok).
- `Cm::set_station_name(&str)` updates `DeviceModel.station_name` (ARServerBlock of the next Connect). `Cm::abort(reason)` already exists for device-initiated aborts; two new `AbortReason` variants: `DcpSet`, `ResetToFactory`.

### 5.6 `diag`

- `DiagEntry` gains `ext: Option<ExtChannelError { ext_type: u16, add_value: u32 }>`. `DiagStore::raise_ext(...)` / existing `raise` (ext = None). Alarm payload builder emits USI `0x8002` with the extended fields when `ext` is set, `0x8000` otherwise (the Plan 5 parse path for `ExtChannelDiagnosis` already exists; this is its encode twin, round-trip tested).

### 5.7 `device`

- `Device` gains `net: NetParams`, `net_store: NetStore`, `hooks: DeviceHooks`, `pdev: Option<Pdev>`, `lldp_eth: Option<Box<dyn EthTransport>>`, `signal_count: Arc<AtomicU32>`.
- `step`: deadline = min(cm, alarm, `pdev.next_deadline()`); `wait_any_readable` includes the LLDP fd; stage 1b drains the LLDP socket into `pdev.on_rx`; stage 5b `pdev.tick(now)` → send LLDPDUs on `lldp_eth`, push diag changes into the existing diag path (stage 2 already turns `DiagStore` changes into alarms).
- DCP dispatch (stage 1) now runs the §5.1 effect sequence with `&mut self`.
- `shutdown`: LLDP shutdown PDU (TTL 0) before the existing ERR-RTA.
- `DeviceSetup` gains `net_store: Option<PathBuf>`; `DeviceHooks { ip: Box<dyn IpConfigurator>, link: Box<dyn LinkProbe>, lldp: Option<Box<dyn EthTransport>>, on_signal: Option<Arc<dyn Fn() + Send + Sync>> }` with `Default` = real implementations and no LLDP transport (the facade opens it).

### 5.8 `api`

- `StartOptions` + `net_store: Option<PathBuf>`, `apply_ip: bool` (default `true`; `false` swaps in a no-op `IpConfigurator`), `lldp: bool` (default `true`), `on_signal`.
- `IoDevice::start` opens the LLDP socket (`open_with(iface, EthProto::LLDP)`) when `lldp` is on and passes `DeviceHooks`. `start_with` takes a `DeviceHooks` so tests inject mocks.
- `IoDevice` + `net_params() -> NetParams`, `signal_count() -> u32`, `peer() -> Option<Peer>` (snapshots through the existing shared-state pattern used for `ar_params()`).
- `examples/linux/typed_bringup.rs`: `--net-store <path>`, prints the current name and the peer in its status line. `examples/gen_gsdml.rs`: unchanged interface, new output.

### 5.9 `gsdml` / `config`

- DAP: `PNIO_Version="V2.31"`, `LLDP_NoD_Supported="true"`, `ResetToFactoryModes="2 4"` *(XSD: value list — verify the separator)*, child `<CertificationInfo ConformanceClass="A" ApplicationClass="" NetloadClass="I"/>` *(placement per XSD)*.
- `InterfaceSubmoduleItem`: `SupportedProtocols="LLDP"`, `PTP_BoundarySupported="true"`, `DCP_BoundarySupported="true"`, `DCP_HelloSupported="false"`, `DelayMeasurementSupported="false"`.
- `PortSubmoduleItem`: unchanged unless the XSD or checker demands an attribute at V2.31 (added one by one, each justified in `docs/gsdml.md`).
- Golden `testdata/gsdml/sample-16real-32bool.xml` regenerated; `render_is_well_formed_and_structurally_consistent` asserts the new values. `docs/gsdml.md` validation section updated (checker rule codes now satisfied).

## 6. Examples and HIL (§6j of `docs/bench-pnet-device.md`)

Bench: 1515-2 PN X1 ↔ edge `eno2`, TIA V21, 500 µs. Binary: musl `typed_bringup` with `--net-store ~/bench/pnio-net`, `--im-store`, caps `cap_net_raw,cap_net_admin,cap_sys_nice,cap_ipc_lock`.

1. GSD checker accepts the V2.31 GSD; project compiles with the device, topology configured (CPU X1 P1 ↔ device port-001).
2. Download, AR up; TIA online topology view green; CPU diagnostic buffer clean. Wireshark: our LLDPDU decodes cleanly, our `0x802A`/`0x8080`/`0xF841` responses decode without "unknown" remainder.
3. I&M1-3: fill plant designation / location / date / descriptor in TIA, download → Write records captured → power-cycle the device → values read back in TIA (success criterion 2). **If no Write appears**: confirm the fields are set in the module properties, retry with `PNIO_Version="V2.35"`, capture, and record the outcome against H1 before going further.
4. Assign name from *Online & diagnostics*: Identify shows the new name, AR re-established, `~/bench/pnio-net` updated, restart keeps the name. Same with a new IP (`ip addr` on the edge reflects it).
5. Flash LED: `signal_count` increments, log line.
6. Move the cable to X2 (or another switch port): *remote mismatch* in the CPU diagnostic buffer within ~25 s (TTL + period); move back: entry disappears. Unplug the cable: AR loss as before, no stale mismatch after reconnect.
7. Reset to factory with "retain I&M" (mode 2) and without (mode 4): name blank in Identify, TIA re-assigns; mode 4 blanks I&M1-3.
8. Trace TIA's `0xF841`/`0xF842` reads for one minute: zero "invalid index".
9. 10 min at 500 µs with the mismatch active: `PASS`, zero missed ticks.

## 7. Tests

- Unit (inline): `dcp::set` decode/decide for every block and error; `dcp::name` syntax matrix; `NetStore` round-trip, temporary vs permanent, reset content, malformed file; `lldp` round-trip + goldens; `pdev` builders against hand-encoded expectations and the dissector's field order; `evaluate()` matrix (no check / match / chassis / port / no peer before and after grace / expiry); Write dispatcher statuses; `diag` USI `0x8002` encode/parse round-trip; `device` loop with `MockTransport` ×2, `MockIp`, `MockLink`: bring-up → LLDPDU sent at t0 and t0+5 s; CPU LLDPDU fed → `0x802A` shows the peer; Check written with a different port → Diagnosis alarm; peer fixed → DiagnosisDisappears; Set-name → `MockIp` not called, AR aborted, Identify answers the new name, `NetStore` file written; Set-IP → `MockIp` called; Reset mode 4 → I&M blank.
- Integration: `tests/pdev_replay.rs` replays the captured Connect (with `0x802B`/`0x802F`) and the DCP goldens through `Device`, asserts statuses and the content of our records structurally. Existing replays untouched and green.
- GSDML: golden + `lxml` validation recipe (manual, documented).

## 8. Capture campaign (task 0) and goldens

Prerequisite: GSDML task done (V2.31 GSD installed in TIA, topology configured), 0.1.0 binary running (it answers errors; we want the requests and the CPU's own frames). Capture on `eno2` without filter, one pcapng per step, under `captures/plan8-<date>/` (git-ignored) with a README:
- `connect`: AR establishment — expect `0x802B` CheckPeers, `0x802F`, `0x8071` in the MultipleWrite; the CPU's LLDPDU on X1.
- `reads`: 60 s of TIA online — `0x802A`, `0x8080`, `0xF841`, `0xF842`, `0xF820`, `0xF00C` requests.
- `dcp-name`, `dcp-ip`, `dcp-flash`, `dcp-reset-keep-im`, `dcp-reset-all`: one *Online & diagnostics* action each.
- `im-write`: I&M1-3 download (may be empty under H1 — still informative).

Extracted into `testdata/lldp/`, `testdata/pdev/`, `testdata/dcp/` as hex, documented in `docs/pdev-golden-frames.md` (format of `docs/dcp-golden-frames.md`). The *(verify)* items of §4 are resolved in that document before the code tasks start.

## 9. Errors and edge cases

- LLDP socket cannot be opened (no capability): `start` fails with `ApiError::Io` unless `lldp: false`.
- `IpConfigurator` failure: DCP block answers `SetNotPossible`, nothing persisted, logged at error.
- `NetStore` write failure: logged, the in-memory change stands (same policy as `ImStore`).
- Name/IP set to the current values: Ok, no abort, no interface reconfiguration; a permanent Set still re-saves the whole identity (so a temporary Set followed by a permanent same-value Set lands in the file).
- Peer flapping (PDU every 5 s from two different ports — a misconfigured switch): the last PDU wins; evaluation may toggle; acceptable, logged at debug.
- Check written with `NumberOfPeers = 0` (TIA "no partner" topology): no mismatch ever, `evaluate()` → `None`.
- Check or Adjust for an unknown subslot: `write_invalid_parameter`.
- `0xF841` read before any peer / engineering: records with `NumberOfPeers = 0` and no Check/Adjust blocks, never "invalid index".

## 10. Docs

`docs/pdev-golden-frames.md` (new), `docs/gsdml.md` (V2.31 section, checker rules, H1 outcome), `docs/bench-pnet-device.md` §6j, `README.md` (Limitations: DCP and LLDP lines removed, SNMP/MRP lines added), `CHANGELOG.md` 0.2.0, `FOLLOWUPS.md` Plan 8 section.

## 11. Release 0.2.0 (last task of the plan)

`doc(hidden)` on internal `pub mod`s, remove `pub fn version()`, `Device::run_with` callback (so `IoDevice::run_publishing_params` stops duplicating the loop), `TooLongTotal` mixed-submodule test, CHANGELOG, README, `cargo publish --dry-run`, tag `v0.2.0`, publish (user decision at that point).

## 12. Dependencies

None new. `nix` already covers the sockets; the `ip` command is invoked through `std::process::Command`.

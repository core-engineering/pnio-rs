# Changelog

All notable changes to `pnio` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the crate is pre-1.0 and
follows SemVer's 0.y.z rules (breaking changes bump the minor version).

## [Unreleased]

Plan 8 (PDEV / LLDP / DCP Set). Implemented; **bench validation (capture campaign, HIL) pending**:
wire facts are provisional (dissector-sourced) in `docs/pdev-golden-frames.md`.

### Added
- GSDML V2.31 profile: `PNIO_Version="V2.31"`, `LLDP_NoD_Supported`, `ResetToFactoryModes="2 4"`,
  `CertificationInfo`, interface `SupportedProtocols="LLDP"`.
- `dcp`: Set-NameOfStation, Signal and Reset-to-factory (modes 2 and 4); `dcp::name` validation;
  `NetParams`, `NetStore` (text file), `IpConfigurator` (`IpCommand` / `NoopIp` / `MockIp`).
- `eth`: `EthProto` and `AfPacketTransport::open_with`.
- `lldp`: LLDPDU codec (PROFINET and IEEE 802.3 TLVs), `LldpError`.
- `pdev`: single-port state and peer tracking with TTL; `pdev::records` for PDPortDataReal
  (`0x802A`), PDInterfaceDataReal (`0x8080`), PDPortDataCheck (`0x802B`), PDPortDataAdjust
  (`0x802F`), PDInterfaceAdjust (`0x8071`), PDRealData (`0xF841`), PDExpectedData (`0xF842`).
- `diag`: remote-mismatch ExtChannelDiagnosis (ChannelErrorType `0x8001`, USI `0x8002`).
- `cm`: per-record Write statuses, `Cm::set_station_name`, `AbortReason::{DcpSet, ResetToFactory}`.
- `device`: DCP effects, LLDP socket, PDEV tick, hooks, TTL-0 LLDPDU at shutdown.
- `device`: `Device::run_with(stop, after_step)`, the loop of `Device::run` with a per-step callback.
- `api`: `StartOptions { net_store, apply_ip, lldp, on_signal }`, `start_with(.., hooks)`,
  `IoDevice::{net_params, signal_count, peer, port_mismatch}`.
- End-to-end test over a veth pair (`tests/veth_e2e.rs`, `scripts/veth-e2e.sh`, CI job): the
  device on real `AF_PACKET`/UDP sockets against a scripted controller in its own network
  namespace (DCP Identify/Set/Signal, AR establishment, cyclic data both ways, LLDP, shutdown).
- `typed_bringup`: `--net-store`, `--apply-ip` (opt-in: applying needs root or
  `setcap cap_net_admin+ep` on the `ip` binary), `--no-lldp`.

### Changed
- MSRV raised from 1.74 to 1.77: `thiserror` 2.0.21 requires rustc 1.77.
- API surface: the protocol layers `alarm`, `cm`, `dcp`, `device`, `eth`, `lldp`, `pdev`, `rpc`
  and `rt` are `#[doc(hidden)]` (still `pub`, no 0.x stability promise), as are the plumbing items
  `DeviceConfig::{model, setup, dcp_properties, activity_seed}`, `diag::DiagStore`,
  `IoDevice::image` and `Peer::from_pdu`. The types the facade exposes are re-exported from
  `pnio::api` (`RtOptions`, `ArState`, `AbortReason`, `AlarmStats`, `MacAddr`, `NetParams`, `Peer`,
  `Mismatch`, `Validity`, `Freshness`, `WatchdogState`, `RtStats`, `StatsSnapshot`, `Histogram`,
  `HistSnapshot`, `DeviceError`, `ImageError`, `TransportError`, `RpcError`); the old paths keep
  compiling.
- `StartOptions` has four new fields (breaking for struct literals); `DeviceSetup` gained
  `net_store` and `iface`; `Device::new_with(.., hooks, now)`.
- `dcp::decide_set(req, &DeviceProperties) -> Vec<(u8, u8, SetBlockError)>` →
  `decide_set(&SetRequest) -> Vec<SetDecision>` (per-block `Effect`, no current-properties
  guard); `cm::build_write_res(req, statuses)` signature; `start_with` signature (takes
  `DeviceHooks`); new `Action::WriteAccepted`.
- Non-OK I&M writes now reach the wire (per-record Write status).
- `IpCommand` flushes IPv4 only (`ip -4 addr flush`): the IPv6 link-local address survives.
- The stored IP suite is re-applied to the interface at startup when a `NetStore` supplied
  the identity (a failure is logged, never fatal).

### Fixed
- TIA's `0xF841` / `0xF842` probes no longer answer "invalid index".
- DCP Set-IP no longer requires the suite to equal the configured one.

### Removed
- `pnio::version()`, a smoke test left over from the scaffold (the crate version is in Cargo metadata).

## [0.1.0] — 2026-08-31

First published version (crates.io `pnio 0.1.0`, tag `v0.1.0`). A PROFINET RT IO-Device (Conformance Class A, RT class 1) in pure
Rust, validated byte-exact against captures from an S7-1500 (1515-2 PN, TIA V21) and in
hardware-in-the-loop at 1 ms and 500 µs on Linux `PREEMPT_RT`.

### Added
- `eth`: L2 framing (VLAN-tagged `0x8892`), `AF_PACKET` transport with classic BPF filters, mock transport.
- `dcp`: Identify (name filter, all) and Set-IP on the device side.
- `rpc` / `cm`: DCE-RPC CL codec over UDP 34964, AR establishment state machine (Connect, Write,
  PrmEnd, ApplicationReady, Release, controller reconnect), record dispatch.
- `rt`: cyclic PPM/CPM exchange with IOPS/IOCS, cycle counter, consumer watchdog, data status
  (incl. the station problem indicator), `SCHED_FIFO` runner with CPU pinning and memory
  locking; 1 ms and 500 µs held on a real controller with zero missed ticks.
- `alarm`: RTA-PDU codec and a one-alarm-in-flight sender/receiver (retries, dedup, ERR-RTA
  both ways), byte-exact against a p-net capture.
- `diag`: application channel diagnosis (`raise_diagnosis` / `clear_diagnosis`) → Diagnosis /
  DiagnosisDisappears alarms, problem indicator, replay after an AR loss.
- `im` + `cm::records`: I&M0 from the device configuration, I&M1-3 readable/writable with an
  optional raw-bytes file store; `Read` / `ReadImplicit` of `0xAFF0..=0xAFF3`.
- `config` / `gsdml`: typed device declaration (builder) and a GSDML V2.4 rendered from the same
  object (`Writeable_IM_Records`, `MinDeviceInterval` 500 µs / 1 ms), accepted by TIA V21.
- `api`: `IoDevice` facade — typed reads/writes, per-slot consistent snapshots, diagnosis API,
  alarm statistics, `im_store`.
- Examples: `typed_bringup` (HIL bring-up with verdict), `latency_probe`, `gen_gsdml`,
  `ar_bringup`, `rt_bringup`; bench scripts and HIL reports under `docs/` and `bench/`.

### Known limitations
- DCP Set-NameOfStation / Signal (flash) / Reset-to-factory not implemented: the station name
  comes from the configuration.
- Process alarms, manufacturer-specific diagnosis, plug/pull alarms, diagnosis record reads
  (`0x800x`, `0xF8xx`) are not implemented (answered with PNIORW "invalid index").
- TIA V21 issued no I&M1-3 write to this device with `PNIO_Version="V2.3"`; the I&M1-3 write
  path is unit-tested but not yet exercised by a controller.
- Development identity `0xFFFF`/`0x0001`; not certified by PI. Linux only (`AF_PACKET`).

[Unreleased]: https://github.com/core-engineering/pnio-rs/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/core-engineering/pnio-rs/releases/tag/v0.1.0

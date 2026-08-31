# Changelog

All notable changes to `pnio` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the crate is pre-1.0 and
follows SemVer's 0.y.z rules (breaking changes bump the minor version).

## [Unreleased]

## [0.1.0] — 2026-08-31

First published version. A PROFINET RT IO-Device (Conformance Class A, RT class 1) in pure
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

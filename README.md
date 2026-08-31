# pnio

[![CI](https://github.com/core-engineering/pnio-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/core-engineering/pnio-rs/actions/workflows/ci.yml)
![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)
![MSRV](https://img.shields.io/badge/MSRV-1.74-informational)
![status](https://img.shields.io/badge/status-0.1%20pre--release-orange)

A **PROFINET RT IO-Device** (Conformance Class A, RT class 1) in **pure Rust**, for Linux
`PREEMPT_RT`. It lets an edge computer sit on a PLC's PROFINET IO system like any other
field device — the controller (an S7‑1500 in our bench) exchanges cyclic process data with it
every **500 µs or 1 ms**, sees its **diagnosis alarms**, reads its **I&M identification**, and
you program the device side in Rust with typed reads and writes.

Validated **byte‑exact** against captures of a real S7‑1515‑2 PN (TIA Portal V21) and in
hardware‑in‑the‑loop: a 12 h 51 soak at 500 µs with 0 missed cycles, tick lateness p99.99 ≈ 20 µs on a
4‑core Atom.

> **Disclaimer.** Community project, **not affiliated with, endorsed or certified by**
> PROFIBUS & PROFINET International (PI). "PROFINET" is a registered trademark of PNO. This is a
> **clean‑room** implementation from the public IEC 61158/61784 standards and from Wireshark
> captures; no normative text is reproduced.

## What you get

- **Cyclic real‑time exchange** (`rt`): PPM/CPM with IOPS/IOCS, cycle counter, consumer watchdog,
  data status (incl. the station problem indicator), on a `SCHED_FIFO` thread pinned to an
  isolated core, memory locked. 500 µs and 1 ms send clocks held against a real controller.
- **AR establishment** (`cm`, `rpc`): DCE‑RPC Connect / Write / PrmEnd / ApplicationReady /
  Release, controller reconnect, record reads and writes.
- **Discovery** (`dcp`): Identify and Set‑IP on the device side.
- **Alarm channel** (`alarm`): RTA codec and a sender/receiver with retries, deduplication and
  ERR‑RTA in both directions — the controller learns about a device stop within a millisecond
  instead of waiting for its watchdog.
- **Channel diagnosis** (`diag`): `raise_diagnosis` / `clear_diagnosis` from your code → the
  CPU's diagnostic buffer, OB82 and the device fault state; re‑announced after an AR loss.
- **I&M records** (`im`): I&M0 from the device configuration, I&M1‑3 readable/writable with an
  optional file store.
- **Typed configuration → GSDML** (`config`, `gsdml`): declare slots and fields in Rust, render the
  GSDML V2.4 TIA imports from the same object — no drift between what the controller expects and
  what the device does.
- **A small facade** (`api::IoDevice`): typed `BOOL`/`INT`/`WORD`/`DINT`/`REAL` reads and writes,
  per‑cycle‑consistent slot snapshots, freshness/validity of the controller's data.

## Quick Start

```bash
git clone https://github.com/core-engineering/pnio-rs.git
cd pnio-rs
cargo test                                   # unit suite + byte-exact capture replays
cargo run --example gen_gsdml -- --out .     # the GSDML matching the sample declaration
```

Linux only: the `AF_PACKET` backend needs `CAP_NET_RAW`/`CAP_NET_ADMIN` at runtime (plus
`CAP_SYS_NICE`/`CAP_IPC_LOCK` for the real‑time thread); tests that need a real socket are
`#[ignore]`d.

Declare the device's process data, start it, exchange typed values:

```rust
use pnio::api::{IoDevice, StartOptions};
use pnio::config::{DeviceConfig, Slot};
use pnio::data::FieldType;
use pnio::device::RtOptions;

let cfg = DeviceConfig::builder("pnio-dev")
    .input(Slot(1), &[FieldType::Real; 16])   // device -> controller (%I)
    .output(Slot(3), &[FieldType::Real; 16])  // controller -> device (%Q)
    .build()
    .expect("valid declaration");

let dev = IoDevice::start(cfg, StartOptions {
    iface: "eno2".into(),
    ip: [172, 16, 2, 10],
    rt: Some(RtOptions { iface: "eno2".into(), cpu_pin: Some(3), rt_priority: Some(80), lock_memory: true }),
    app_cpus: Some(vec![0, 1, 2]),
    im_store: Some("/var/lib/pnio/im.bin".into()),   // persists the I&M1-3 written by TIA
}).expect("start (needs cap_net_raw/cap_net_admin/cap_sys_nice/cap_ipc_lock)");

while !dev.ready() {
    std::thread::sleep(std::time::Duration::from_millis(5));
}
// Echo the controller's first REAL back into our first input, consistently per cycle.
if let Ok(out) = dev.outputs(Slot(3)) {
    dev.with_inputs(Slot(1), |w| w.real(0, out.real(0)?)).ok();
}
```

Tell the controller something is wrong, then that it is fixed:

```rust
use pnio::diag::{ChannelError, Severity};

dev.raise_diagnosis(Slot(1), 0, ChannelError::LineBreak, Severity::Fault)?;
// -> "Wire break on input channel 0" in the CPU's diagnostic buffer, device shown in fault,
//    ProblemIndicator cleared in every cyclic frame until:
dev.clear_diagnosis(Slot(1), 0, ChannelError::LineBreak)?;
```

`IoDevice::alarm_stats()` mirrors the live alarm channel (counters restart with each AR);
`alarm_rx_no_channel()` is the one process‑wide counter. The GSDML for your declaration comes from
`pnio::gsdml::render` (see the `gen_gsdml` example and [`docs/gsdml.md`](docs/gsdml.md) for the
TIA import and XSD validation steps).

> ⚠️ **Identity.** The builder's default identity (`vendor_id = 0xFFFF`, `device_id = 0x0001`)
> is a **development value, not a PI‑assigned ID**. Set `.identity(vendor_id, device_id)` before
> any deployment outside the lab.

## Status

| Area | Module | Status |
|---|---|---|
| L2 framing, `AF_PACKET` transport, BPF filters, mock | `eth` | ✅ |
| DCP Identify (name / all), Set‑IP | `dcp` | ✅ — Set‑name, Signal, Reset‑to‑factory: see Limitations |
| DCE‑RPC CL codec, UDP transport | `rpc` | ✅ |
| AR state machine, records, reconnect | `cm` | ✅ HIL |
| Cyclic exchange, watchdog, RT thread | `rt` | ✅ HIL — 500 µs / 1 ms, 0 missed ticks |
| Alarm channel (RTA, retries, ERR‑RTA) | `alarm` | ✅ HIL — byte‑exact vs a p‑net capture |
| Channel diagnosis, problem indicator, replay | `diag` | ✅ HIL |
| I&M0 read; I&M1‑3 read/write + file store | `im`, `cm::records` | ✅ I&M0 HIL; I&M1‑3 unit‑tested (see Limitations) |
| Typed configuration, generated GSDML | `config`, `gsdml` | ✅ HIL — imported by TIA V21 |
| Device facade, typed I/O | `api` | ✅ HIL |
| pcap/pcapng replay harness (feature `capture`, default on) | `capture` | ✅ |

Measured on the bench (Atom E3940, Debian 13 `PREEMPT_RT`, S7‑1515‑2 PN, TIA V21, X1 port at
500 µs): tick lateness p99.99 13‑22 µs / max ≤ 66 µs, 12 h 51 soak with 0 anomalies over 79.7 M
application samples, controller‑side abort detected in 0.65 ms on device stop. Full reports:
[`docs/bench-pnet-device.md`](docs/bench-pnet-device.md).

## Limitations (0.1)

- **No DCP Set‑NameOfStation, Signal (LED flash) or Reset‑to‑factory**: the station name comes
  from the configuration; TIA's *Assign PROFINET device name* is not served yet.
- **No process alarms** (OB40), manufacturer‑specific diagnosis codes, plug/pull alarms,
  `ModuleDiffBlock`, or diagnosis record reads (`0x800x`, `0xF8xx` — answered "invalid index").
- **I&M1‑3 writes were never issued by TIA V21** to this device (GSDML `PNIO_Version="V2.3"`);
  the write path is unit‑tested only.
- **RT class 1 only**: no IRT, no MRP, no LLDP/PTP topology (the V2.31+ GSDML profile).
- **Linux only**, single AR, development Vendor ID — **not PI‑certified**.

## Architecture

Pure Rust, four runtime dependencies (`libc`, `nix`, `log`, `thiserror`; `pcap-file` behind the
`capture` feature). Everything on the wire is big‑endian, exactly as in Siemens memory. Two
threads: an **acyclic** loop (DCP, RPC/AR, alarms, records) and a **real‑time** loop (cyclic
frames only — one atomic load, no allocation, no lock held across a syscall) that share a
per‑cycle‑consistent I/O image. `unsafe` is confined to the raw‑socket and scheduler modules
(`eth::afpacket`, `eth::poll`, `rt::sched`, `rt::runner`); the rest of the crate is
`#![deny(unsafe_code)]`.

Design documents: [`docs/design/`](docs/design/) — the overall device design and one design note
per subsystem (AR, cyclic exchange, 1 ms determinism, configuration/GSDML/API, alarms/diagnosis/I&M).

## Clean‑room approach

Derived from the **public IEC standards** (IEC 61158‑6‑10 protocol, 61158‑5‑10 services,
61784‑2‑3 RT profiles) and from **Wireshark captures** of real traffic. Every frame the device
emits that has a real counterpart is pinned byte‑exact as a "golden frame" with its provenance
([`docs/dcp-golden-frames.md`](docs/dcp-golden-frames.md),
[`docs/cm-golden-frames.md`](docs/cm-golden-frames.md),
[`docs/alarm-golden-frames.md`](docs/alarm-golden-frames.md)). No copyleft code is included.

## Documentation

- [`CHANGELOG.md`](CHANGELOG.md) — releases and known limitations
- [`docs/design/`](docs/design/) — design documents; [`docs/plans/`](docs/plans/) — the
  task‑by‑task implementation plans they were built from
- [`docs/gsdml.md`](docs/gsdml.md) — typed configuration, generated GSDML, TIA import, XSD validation
- [`docs/bench-pnet-device.md`](docs/bench-pnet-device.md) — bench setup and every HIL report;
  [`bench/README.md`](bench/README.md) — campaign scripts
- [`FOLLOWUPS.md`](FOLLOWUPS.md) — open points, by subsystem
- [`CONTRIBUTING.md`](CONTRIBUTING.md)

## Roadmap

0.1 (this): CC‑A device with alarms, diagnosis and I&M, validated on an S7‑1500. Next: the
V2.31+ GSDML profile (LLDP, PTP/DCP boundaries, reset‑to‑factory, certification info), DCP
Set‑name/Signal, process alarms, then the 250 µs work (`PACKET_MMAP`/busy‑poll).

## License

Your choice: [MIT](LICENSE-MIT) or [Apache‑2.0](LICENSE-APACHE) (`SPDX: MIT OR Apache-2.0`).
Unless stated otherwise, any contribution submitted for inclusion is under this dual license.

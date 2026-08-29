# pnio

[![CI](https://github.com/core-engineering/pnio-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/core-engineering/pnio-rs/actions/workflows/ci.yml)
![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)
![status](https://img.shields.io/badge/status-pre--1.0%20(WIP)-orange)

**IO-Device PROFINET RT (Class 1 / Conformance Class A)** stack in **pure Rust**, for Linux
**PREEMPT_RT** — designed to close control loops on the *edge* side with an S7‑1500
(IO‑Controller), target cycle **< 2 ms**.

> **Disclaimer.** Community project, **not affiliated with, nor endorsed or certified by**
> PROFIBUS & PROFINET International (PI). "PROFINET" is a registered trademark of PNO.
> This library is a **clean‑room** implementation derived from the public standard
> IEC 61158/61784. No normative text is reproduced herein.

## Why

Acyclic exchange protocols (S7comm, Modbus, OPC UA) are unsuitable for
**deterministic control**: the **real-time cyclic channel** of PROFINET is required. Existing
stacks impose licensing constraints (e.g. `p-net` is GPLv3 + commercial). This project
aims for a **reusable stack under a permissive dual license**, with full IP ownership.

## Status

Active development, **pre‑1.0**. Validated **byte‑exact** against real captures from an
S7‑1500 (1515‑2 PN).

| Layer | Module | Status |
|---|---|---|
| L2 Ethernet layer (header + VLAN, AF_PACKET transport, mock) | `eth` | ✅ |
| **pcap & pcapng** capture replay harness | `capture` | ✅ |
| Process type codecs (INT/WORD/DINT/REAL big‑endian, packed BOOL) | `data` | ✅ |
| **DCP** device side: Identify (request parsing + byte-exact response, dispatch) | `dcp` | ✅ |
| DCP Set-IP (guarded) ✅, Get / Set‑name / Flash ⏳ | `dcp` | ⏳ |
| DCE-RPC CL codec + UDP transport | `rpc` | ✅ |
| AR establishment (DCE/RPC, state machine) | `cm` | ✅ AR reaches DATA on a real S7-1500 (HIL 2026-08-28) |
| Acyclic device loop + bring-up example | `device` | ✅ |
| RT cyclic exchange (PPM/CPM, IOPS/IOCS, watchdog, `SCHED_FIFO` thread) | `rt` | ✅ 1 ms held on PREEMPT_RT (edge Atom E3940). **L2-pair profile (default, Plan 7bis, HIL 2026-08-28)**: under load p99.99 13 µs / max 22.4 µs, idle p99.99 20 µs / max 22.7 µs, 0 missed ticks / 0 watchdog expirations over 1.2 M cycles (600078 + 597969). Single-core profile (Plan 7, HIL 2026-08-28), under load on CPUs 0-2: p99.99 147-203 µs / max ≤ 284 µs — see `docs/bench-pnet-device.md` §6e/§6f |
| Alarms + I&M | `alarm`/`im` | ⏳ |
| Typed device configuration (builder, layout rule, DCP/model derivation) | `config` | ✅ |
| Generated GSDML (V2.4, matches `config` by construction — see `docs/gsdml.md`) | `gsdml` | ✅ HIL with our own GSDML on the S7-1500: 2026-08-29 — see `docs/bench-pnet-device.md` §6g |
| Typed device facade (`IoDevice`, typed reads/writes, per-slot snapshots) | `api` | ✅ HIL with our own GSDML on the S7-1500: 2026-08-29 — see `docs/bench-pnet-device.md` §6g |
| HIL integration + determinism (real S7‑1500, jitter measurement) | — | 1 ms held against a real S7-1500, idle and under load, on PREEMPT_RT ✅ (HIL 2026-08-28, `docs/bench-pnet-device.md` §6e/§6f — L2-pair profile now the default); see the `rt` row above for numbers |

## Architecture

- **Pure Rust**, no heavy dependencies; everything is **big‑endian** ("Motorola" format,
  identical to Siemens memory — no word-swap).
- Protocol layer decomposition (`eth` → `dcp` → `cm`/AR → `rt`/alarms), each layer
  independently testable.
- Runtime target: Debian **PREEMPT_RT**, 1 ms send clock, `SCHED_FIFO` RT thread. The I/O
  image shared with the application is mutex-protected, publishing a per-cycle-consistent
  snapshot on each side (non-blocking on the RT side). The Plan 7 HIL campaign measured
  `input_snapshot_reused + output_publish_deferred` at 0.07-0.12 % of ticks at 1 ms; the spec's
  §9 rule (< 0.1 % → `Mutex` stays, otherwise a seqlock) is exceeded under the spec's own load
  (0.11-0.12 %), so keeping the `Mutex` + `try_lock` image is a **deliberate deviation from
  that rule** — every run showed zero dropped frames, and the overshoot is only 0.01-0.02
  points. The Plan 7bis L2-pair campaign measured 0.13 % idle / 0.15 % under load — still over
  the line, by a slightly wider margin, and the deviation stands unchanged (`docs/bench-pnet-device.md`
  §6f). A lock-free seqlock stays a FOLLOWUP, triggered only if a consumer needs every single
  cycle's outputs.

## Quick Start

```bash
git clone https://github.com/core-engineering/pnio-rs.git
cd pnio-rs
cargo test          # unit suite + capture-replay integration test
cargo clippy --all-targets -- -D warnings
```

The `AfPacketTransport` backend (raw L2 sockets) requires Linux and the `CAP_NET_RAW`
capability at runtime; tests that depend on it are marked `#[ignore]`.

Declare the device's process data in Rust, start it, exchange typed values:

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
}).expect("start (needs cap_net_raw/cap_net_admin/cap_sys_nice)");

while !dev.ready() {
    std::thread::sleep(std::time::Duration::from_millis(5));
}
if let Ok(out) = dev.outputs(Slot(3)) {
    dev.with_inputs(Slot(1), |w| w.real(0, out.real(0)?)).ok();
}
```

`cargo run --example gen_gsdml` renders the matching GSDML for the sample declaration above and
prints the resulting controller address map; see [`docs/gsdml.md`](docs/gsdml.md) for the layout
rule, the TIA import steps and the XSD validation recipe.

> ⚠️ **Identity**: `DeviceConfig::builder`'s default identity (`vendor_id = 0xFFFF`, `device_id =
> 0x0001`, used above) is a **development value, not a PI-assigned ID**. Replace it via
> `.identity(vendor_id, device_id)` before any deployment outside the lab.

## Clean‑room Approach

The implementation is derived from the **public IEC standard** (IEC 61158‑6‑10 for the
protocol, 61158‑5‑10 for services, 61784‑2‑3 for RT profiles) and from **Wireshark captures**
of real traffic. Reference frames ("golden frames") and their provenance are documented in
[`docs/dcp-golden-frames.md`](docs/dcp-golden-frames.md). No third-party copyleft code is
included.

## Documentation

- Design: [`docs/superpowers/specs/`](docs/superpowers/specs/)
- Implementation plans (TDD, task by task): [`docs/superpowers/plans/`](docs/superpowers/plans/)
- Typed configuration, generated GSDML, import/validation in TIA:
  [`docs/gsdml.md`](docs/gsdml.md)
- Test benches: [`docs/bench-capture-protocol.md`](docs/bench-capture-protocol.md),
  [`docs/bench-pnet-device.md`](docs/bench-pnet-device.md)
- 1 ms HIL campaign scripts and usage: [`bench/README.md`](bench/README.md)

## Roadmap

`cm` (AR) ✅ → `rt` (cyclic exchange) ✅ → determinism (1 ms, `PREEMPT_RT`) ✅ → Plan 7bis
(L2-pair isolation) ✅ → `config`/GSDML/typed API (Plan 6) ✅ → **next: `alarm`/`im`
(Plan 5)**. `PACKET_MMAP`/busy-poll stays deferred, only needed if a future campaign under the
original CPU-0-2 load layout still needs the p99.99 budget. Details in the plans above.

## License

Your choice: [MIT](LICENSE-MIT) or [Apache‑2.0](LICENSE-APACHE)
(`SPDX: MIT OR Apache-2.0`).

Unless stated otherwise, any contribution submitted for inclusion is under this dual license.

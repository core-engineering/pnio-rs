# profinet-rt

[![CI](https://github.com/core-engineering/profinet-rt/actions/workflows/ci.yml/badge.svg)](https://github.com/core-engineering/profinet-rt/actions/workflows/ci.yml)
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
| RT cyclic exchange (PPM/CPM, IOPS/IOCS, watchdog, `SCHED_FIFO` thread) | `rt` | ⏳ |
| Alarms + I&M | `alarm`/`im` | ⏳ |
| Config model + GSDML + public API | `config` | ⏳ |
| HIL integration + determinism (real S7‑1500, jitter measurement) | — | ⏳ |

## Architecture

- **Pure Rust**, no heavy dependencies; everything is **big‑endian** ("Motorola" format,
  identical to Siemens memory — no word-swap).
- Protocol layer decomposition (`eth` → `dcp` → `cm`/AR → `rt`/alarms), each layer
  independently testable.
- Runtime target: Debian **PREEMPT_RT**, 1 ms send clock, `SCHED_FIFO` RT thread, double-buffer/seqlock
  I/O image (coming with the `rt` layer).

## Quick Start

```bash
git clone https://github.com/core-engineering/profinet-rt.git
cd profinet-rt
cargo test          # unit suite + capture-replay integration test
cargo clippy --all-targets -- -D warnings
```

The `AfPacketTransport` backend (raw L2 sockets) requires Linux and the `CAP_NET_RAW`
capability at runtime; tests that depend on it are marked `#[ignore]`.

## Clean‑room Approach

The implementation is derived from the **public IEC standard** (IEC 61158‑6‑10 for the
protocol, 61158‑5‑10 for services, 61784‑2‑3 for RT profiles) and from **Wireshark captures**
of real traffic. Reference frames ("golden frames") and their provenance are documented in
[`docs/dcp-golden-frames.md`](docs/dcp-golden-frames.md). No third-party copyleft code is
included.

> ⚠️ For a real deployment, a legitimate **Vendor ID** from PI is required (the example
> uses test values).

## Documentation

- Design: [`docs/superpowers/specs/`](docs/superpowers/specs/)
- Implementation plans (TDD, task by task): [`docs/superpowers/plans/`](docs/superpowers/plans/)
- Test benches: [`docs/bench-capture-protocol.md`](docs/bench-capture-protocol.md),
  [`docs/bench-pnet-device.md`](docs/bench-pnet-device.md)

## Roadmap

`cm` (AR) ✅ → **next: `rt`** (1 ms cyclic) → `alarm`/`im` → `config`/GSDML/API → HIL
integration & determinism measurement. Details in the plans above.

## License

Your choice: [MIT](LICENSE-MIT) or [Apache‑2.0](LICENSE-APACHE)
(`SPDX: MIT OR Apache-2.0`).

Unless stated otherwise, any contribution submitted for inclusion is under this dual license.

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
| RT cyclic exchange (PPM/CPM, IOPS/IOCS, watchdog, `SCHED_FIFO` thread) | `rt` | ✅ 1 ms held on PREEMPT_RT (edge Atom E3940, HIL 2026-08-28): idle p99.99 48 µs / max 111 µs; under full load (CPUs 0-2) p99.99 147-203 µs / max ≤ 284 µs; load off the L2 sibling (CPUs 0-1) p99.99 92 µs / max 148 µs; 0 missed ticks / 0 watchdog expirations over 2.4 M cycles |
| Alarms + I&M | `alarm`/`im` | ⏳ |
| Config model + GSDML + public API | `config` | ⏳ |
| HIL integration + determinism (real S7‑1500, jitter measurement) | — | 1 ms held against a real S7-1500, idle and under load, on PREEMPT_RT ✅ (HIL 2026-08-28, `docs/bench-pnet-device.md` §6e); see the `rt` row above for numbers |

## Architecture

- **Pure Rust**, no heavy dependencies; everything is **big‑endian** ("Motorola" format,
  identical to Siemens memory — no word-swap).
- Protocol layer decomposition (`eth` → `dcp` → `cm`/AR → `rt`/alarms), each layer
  independently testable.
- Runtime target: Debian **PREEMPT_RT**, 1 ms send clock, `SCHED_FIFO` RT thread. The I/O
  image shared with the application is mutex-protected, publishing a per-cycle-consistent
  snapshot on each side (non-blocking on the RT side). Confirmed by the Plan 7 HIL campaign:
  `input_snapshot_reused + output_publish_deferred` is 0.08-0.12 % of ticks at 1 ms, right at
  the spec's 0.1 % line either side, but every run showed zero dropped frames — the `Mutex` +
  `try_lock` image stays; a lock-free seqlock is deferred to Plan 7bis, triggered only if a
  consumer needs every single cycle's outputs.

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
- 1 ms HIL campaign scripts and usage: [`bench/README.md`](bench/README.md)

## Roadmap

`cm` (AR) ✅ → `rt` (cyclic exchange) ✅ → determinism (1 ms, `PREEMPT_RT`) ✅ → **next:
`alarm`/`im` (Plan 5) or `config`/GSDML/typed API (Plan 6)** → Plan 7bis (L2-pair isolation +
`PACKET_MMAP`/busy-poll if still needed). Details in the plans above.

## License

Your choice: [MIT](LICENSE-MIT) or [Apache‑2.0](LICENSE-APACHE)
(`SPDX: MIT OR Apache-2.0`).

Unless stated otherwise, any contribution submitted for inclusion is under this dual license.

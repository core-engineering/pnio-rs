# Roadmap

`pnio` is meant for **industrial prototyping**: putting a Linux edge computer on a PLC's
PROFINET IO system as a field device, to close control loops or exchange process data
cyclically with the controller, without a commercial stack or a copyleft one.

This page says what works today, what comes next and in which order. It is maintained by one
person, on a bench that is currently **unavailable** (see [Help wanted](#help-wanted)).
**There are no dates**: the order below is a priority, not a commitment, and it can change with
what users ask for.

## Status legend

| Status | Meaning |
|---|---|
| **Validated** | Implemented, unit-tested and validated in hardware-in-the-loop against a real controller |
| **Implemented, bench pending** | Implemented and unit-tested; the wire facts and the HIL run still need a bench |
| **Planned** | Next in line; design not written yet |
| **Exploration** | Studied, not committed to; may never ship |
| **Out of scope** | Not planned, with the reason |

## 0.1 — available now (crates.io)

**Validated** against an S7-1515-2 PN (TIA Portal V21), at 500 µs and 1 ms, Linux `PREEMPT_RT`:

- PROFINET RT IO-Device, Conformance Class A, RT class 1: cyclic exchange with IOPS/IOCS,
  watchdog, data status, on an isolated real-time core.
- AR establishment and controller reconnect (DCE-RPC Connect / Write / PrmEnd /
  ApplicationReady / Release).
- DCP Identify and Set-IP.
- Alarm channel (RTA with retries, ERR-RTA both ways), channel diagnosis to the CPU's
  diagnostic buffer, I&M0.
- Typed configuration in Rust and the matching GSDML (V2.4), imported by TIA.
- Typed I/O facade (`api::IoDevice`).

Measured: tick lateness p99.99 13-22 µs, 12 h 51 soak at 500 µs with 0 anomalies. Details in
the [README](README.md#status) and [`docs/bench-pnet-device.md`](docs/bench-pnet-device.md).

## 0.2 — implemented, bench pending

On branch [`feat/pdev-lldp-dcp`](https://github.com/core-engineering/pnio-rs/tree/feat/pdev-lldp-dcp)
(unit-tested, CI green), released once a capture campaign and a HIL run have confirmed it:

- **LLDP** neighbourhood and **PDEV** records (port and interface data, peer check, remote
  mismatch diagnosis): the controller sees the device in its topology view.
- **DCP Set-NameOfStation, Signal (LED flash) and Reset-to-factory**: TIA's *Assign PROFINET
  device name* and *Flash LED* work, with an optional persistent store.
- **GSDML V2.31** profile, expected to let TIA write I&M1-3 (to be confirmed at HIL).

Until the bench run, the wire values these features rely on come from the Wireshark dissector,
not from captures: [`docs/pdev-golden-frames.md`](https://github.com/core-engineering/pnio-rs/blob/feat/pdev-lldp-dcp/docs/pdev-golden-frames.md).

## Next — planned

- **Process alarms** (OB40), plug/pull alarms, `ModuleDiffBlock`, manufacturer-specific
  diagnosis codes.
- **Diagnosis and AR record reads** (`0x800x`, `0xF80C`, `0xF820`, `0xF00C`), answered
  "invalid index" today.
- **API clean-up before 1.0**: hide the internal modules from the documentation, trim the public
  surface.

## Then — planned

- **250 µs send clock**: `PACKET_MMAP` and busy-polling on the RT core.
- **Second physical port**: the PDEV code handles one port today.

## Later — planned, larger work

- **MRP ring client** (Media Redundancy Protocol, MRC role, the controller being the ring
  manager). Needs the second port first, then two NICs and a ring on the bench. Linux's bridge
  MRP support is the likely starting point; the forwarding jitter it adds to other devices'
  frames will have to be measured.

## Exploration — safety functions

A black-channel safety layer over the cyclic data, as a **test device**: virtual commissioning
and test benches for safety PLC programs, fault injection, training.

> **Not for safety functions.** A general-purpose Linux computer running this crate cannot reach
> any SIL. This work, if it ships, will be a test and simulation tool only, never a component of
> a safety function.

## Out of scope

- **IRT / PTP** (RT classes 2-3, isochronous mode): needs dedicated switching hardware.
- **SNMP** (LLDP-MIB).
- **IO-Controller role**: `pnio` is a device.
- **Windows and macOS**: the transport is Linux `AF_PACKET`.
- **PI certification of the crate**: a product built on it can go through certification, the
  library itself will not.

## Help wanted

The fastest way to move this roadmap forward is to run `pnio` on hardware the maintainer does not
have. Only one combination has been tested: S7-1515-2 PN (FW V2.9), TIA Portal V21, Intel Atom
E3940 with an Intel `igb` NIC, Debian 13 `PREEMPT_RT`.

**Useful to test:**

- **Other controllers**: other S7-1500 CPUs or firmware, S7-1200, ET 200SP CPU, CODESYS-based
  controllers, Beckhoff TwinCAT, Phoenix Contact PLCnext.
- **Other hosts**: other x86 CPUs, ARM boards (`aarch64`, never built so far), other NICs and
  drivers, standard (non-RT) kernels at relaxed update times.
- **The 0.2 branch**, if you have a controller and TIA: the capture campaign is described step by
  step in Task 2 of the [Plan 8 implementation plan](https://github.com/core-engineering/pnio-rs/blob/feat/pdev-lldp-dcp/docs/plans/2026-10-10-pnio-pdev-lldp-dcp.md).

**What to send**, in a [GitHub issue](https://github.com/core-engineering/pnio-rs/issues/new):

- the controller (model, firmware, engineering tool and version) and the update time;
- the host: CPU, `uname -a`, NIC driver (`ethtool -i <iface>`);
- what you ran (`typed_bringup`, `rt_bringup`) and how far it got: AR state reached, the
  `rt_bringup` verdict and statistics, the controller's diagnostic buffer entries;
- if something fails, a `pcapng` capture of the AR start-up, taken **without** a capture filter.
  Only share captures from a test network: they contain MAC and IP addresses.

Reports that say "it works on X" are as valuable as bug reports.

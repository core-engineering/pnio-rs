# Spec — `profinet-rt`: PROFINET RT IO-Device Stack in Rust

- **Date** : 2026-06-25
- **Status** : design validated, pending user review
- **Author** : Camille Martin
- **Projet** : `203-profinet-rt`

## 1. Context & objective

Provide a **reusable Rust communication library** that enables an *edge* machine
(Debian PREEMPT_RT) to participate in the **PROFINET RT cyclic exchange** with an
**S7-1500** IO-Controller, in order to close **control loops** on the edge side
with a cycle time **< 2 ms**.

The initial requirement referred to a "Profinet READ/WRITE interface" and a
"proven OT protocol rather than S7". The scoping clarified that:

- Neither S7/S7+, nor Modbus, nor OPC UA are suitable: they are
  **acyclic** request/response protocols, unsuitable for a deterministic control loop.
- The real need is the **RT cyclic channel** of PROFINET, where the edge acts as an
  **IO-Device** polled by the controller every cycle (the PLC supplies the PVs, the edge
  computes and returns the MVs).

## 2. Scope

### In scope

- **PROFINET RT class 1, Conformance Class A (CC-A) IO-Device stack**, in **pure Rust**
  (no C dependency), targeting **Linux PREEMPT_RT**.
- Send clock **1 ms**, reduction ratio 1 (effective cycle < 2 ms).
- Cyclic exchange of **typed process data**: `BOOL`, `INT`, `DINT`, `REAL`,
  `WORD`, in both directions.
- Services required for interoperability with an S7-1500:
  DCP, AR establishment (DCE/RPC), cyclic RT (PPM/CPM), minimal alarms,
  I&M0 records.
- **Generic library API** for data mapping, provided via configuration; **example GSDML**
  delivered for the 16 `REAL` + 32 `BOOL` per direction case.

### Out of scope

- Any **control / business logic**: it *consumes* the library via a clean API and
  is unaware of PROFINET.
- **IRT / isochronous (CC-C)**, TSN.
- Official **PROFINET certification** (PI label).
- **IO-Controller** role (the edge is IO-Device only).
- Topology / LLDP (CC-B) — *not required for CC-A*, see possible future extensions.

## 3. Scoping decisions (locked)

| Subject | Decision | Reason |
|---|---|---|
| Role | Edge = **IO-Device** | The S7-1500 remains IO-Controller and owns the real I/O |
| Protocol | **PROFINET RT class 1 / CC-A** | Only deterministic cyclic channel for control loops |
| Cycle | **send clock 1 ms**, < 2 ms | User requirement, achievable in software on PREEMPT_RT |
| Language | **Rust 100% native** | Avoid GPLv3 / `p-net` commercial licence; own the IP |
| Determinism | **Software** (no HW offload) | < 2 ms RT class 1 achievable on a properly configured PREEMPT_RT |
| Target | **Functional interop**, no certification | Internal edge ↔ S7-1500 use |
| Deliverable | **Reusable communication crate** | Reuse across other projects |

## 4. Why not `p-net` (decision rationale)

`p-net` (rt-labs) is **dual-licensed GPLv3 + commercial**. The free version is
GPLv3 (copyleft → contaminates the entire edge product) and advertised as
*"not intended for production use"*. The production version requires purchasing a
commercial licence. Writing a native Rust stack avoids **both** the GPLv3 contamination
**and** the licensing fee, **and** gives full IP ownership. No mature PROFINET stack
exists in Rust (new ground).

> **Licence hygiene**: the implementation is **clean-room**. No `p-net` code
> (or any other GPL stack) is incorporated. Reference = the **legally acquired
> IEC 61158 / 61784 standard** + the Wireshark dissector + our own captures.

## 5. Architecture

### 5.1 Layer breakdown

Each layer has a single responsibility, a defined interface, and can be tested
in isolation.

| Module | Responsibility | Depends on | Risk |
|---|---|---|---|
| `eth` | L2 frame I/O EtherType `0x8892` via `AF_PACKET` (ideally `PACKET_MMAP`), behind a mockable trait | NIC | low |
| `dcp` | Discovery & Config Protocol: identify, set-name-of-station, set-IP, flash | `eth` | medium |
| `cm` (Context Manager) | DCE/RPC over UDP 34964: **AR** state machine (Connect / Write / Read / Dcontrol / Ccontrol / Release), records | UDP | **high** |
| `rt` | Cyclic exchange **PPM** (producer edge→PLC) / **CPM** (consumer PLC→edge): IOPS/IOCS, data status, cycle counter, consumer watchdog | `eth` | **high** |
| `alarm` | Minimal RT alarm channel: Application-Ready, plug / return-of-submodule | `cm` | medium |
| `im` | **I&M0** records (mandatory identification) | `cm` | low |
| `config` | Config model (slots / submodules + typed variable map) ↔ GSDML | — | medium |
| `api` | `ProfinetDevice` + typed accessors, I/O image management, AR state | all | — |

### 5.2 Thread model (core of determinism)

- **RT thread**: `SCHED_FIFO`, pinned to an isolated core (`isolcpus` / `nohz_full`),
  NIC IRQs routed to that core. Owns the PPM/CPM loop clocked at the send clock.
  **Hard constraint: no allocation, no blocking lock, no slow I/O syscall in this loop.**
- **Acyclic thread**: normal priority; handles DCP, RPC/AR, alarms, I&M.
- **RT ↔ application exchange**: shared I/O image via **double-buffer / seqlock**
  (non-blocking access on the RT side), **per-cycle** coherence contract. The consuming
  application never touches the network.

### 5.3 Type mapping (note: PROFINET = big-endian "Motorola")

Since both PROFINET and Siemens memory are big-endian, **no word-swap** is needed
(unlike Modbus). A submodule's data is a byte array, bracketed by the
provider/consumer status bytes (IOPS/IOCS).

| Type | Size | Wire encoding |
|---|---|---|
| `BOOL` | 1 bit | packed bits (8/byte), exposed by index |
| `INT` | 2 B | i16 big-endian |
| `WORD` | 2 B | u16 big-endian |
| `DINT` | 4 B | i32 big-endian |
| `REAL` | 4 B | f32 IEEE-754 big-endian |

Example case (16 `REAL` + 32 `BOOL` per direction): 64 + 4 = **68 bytes** per direction,
well within RT frame limits (~1440 B).

### 5.4 Public API sketch

```rust
let cfg = DeviceConfig::builder()
    .station_name("edge-reg-01")
    .vendor_id(0x0000)          // TEST ID in dev — to be regularized (see §7)
    .device_id(0x0001)
    .send_clock(SendClock::Ms1)
    .input_submodule(Slot(1), &[Field::Real; 16])   // PLC -> edge
    .input_submodule(Slot(2), &[Field::Bool; 32])
    .output_submodule(Slot(3), &[Field::Real; 16])  // edge -> PLC
    .output_submodule(Slot(4), &[Field::Bool; 32])
    .build()?;

let dev = ProfinetDevice::start(cfg, "eth0")?;       // lance threads RT + acyclique

// control loop (consumer, OUTSIDE lib):
let pv:  f32     = dev.read_real(Slot(1), 0)?;       // latest consistent image
let cmd: bool    = dev.read_bool(Slot(2), 5)?;
dev.write_real(Slot(3), 0, mv)?;                     // published on the next cycle
let st:  ArState = dev.ar_state();                   // RUN / Offline / Connecting ...
```

## 6. Data flow & lifecycle

1. **Discovery**: the IO-Controller (TIA) finds the device via DCP, assigns
   station name + IP.
2. **AR establishment**: DCE/RPC `Connect` → parameter write (records) →
   `Dcontrol`/`Ccontrol` → **Application-Ready** alarm → AR in **DATA/RUN** state.
3. **Cyclic exchange**: CPM consumes the PLC outputs (device inputs),
   PPM produces the device outputs, at every send clock tick; IOPS/IOCS and
   data status handled.
4. **Supervision**: consumer watchdog (frame loss → AR Offline);
   reconnection; alarm reporting.
5. **Shutdown**: clean AR `Release`.

## 7. Error handling & edge cases

- **AR timeout** / Controller rejection → explicit log + return to Connecting state.
- **Consumer watchdog**: absence of CPM frames beyond `cycle × ratio ×
  factor` → AR Offline, data frozen + invalidity indicator exposed to the application.
- **Data status**: PRIMARY/BACKUP handling and "problem indicator" bit.
- **Vendor ID / Device ID**: **test** IDs during development; a device actually deployed
  must obtain a **legitimate Vendor ID** from PI (otherwise risk of network collision)
  — must be regularised before any broad deployment.

## 8. Test strategy

1. **Per-layer unit tests** against **Wireshark-captured "golden" frames**
   (parse/serialize DCP, RPC, RT, alarms) — in **TDD**.
2. **Integration**: *mock IO-Controller* harness replaying a captured exchange;
   targeted AR state machine tests.
3. **Hardware-in-the-loop**: real S7-1500 + TIA Portal → automated verification
   that the AR reaches **RUN** and that typed values round-trip correctly.
4. **Determinism**: cycle jitter measurement under load (`cyclictest`-style method)
   on the edge, validation that the 1 ms send clock is maintained.

## 9. Risks & dependencies

| Risk | Impact | Mitigation |
|---|---|---|
| AR state machine (#1) | PLC does not reach RUN | Wireshark captures + incremental layer-by-layer bring-up |
| IOPS/IOCS & data status | Data rejected by PLC | Golden tests + early HIL |
| GSDML ↔ engineered config match | Connection refused | Example GSDML locked, generated from config |
| NIC / driver jitter on Linux | 1 ms cycle not met | Dedicated NIC + `PACKET_MMAP`, core isolation, IRQ affinity, PREEMPT_RT |
| Standard access | Implementation blind | **Purchase IEC 61158/61784 standard** (in progress on user's side) |

### Intellectual property / publication

- **Implementing the protocol** from the standard = legal (clean-room; a protocol
  is not protectable, only the *text* of the standard is → do not copy it).
- **Trademark** "PROFINET®" = property of PNO, use reserved for PI members;
  **descriptive** use permitted. No logo, no "certified".
- **Patents**: low exposure for RT class 1 / CC-A (no IRT/TSN), not zero;
  legal review recommended **before industrialisation/distribution**.
- **Open-source publication** feasible: crate `profinet-rt` (name free on
  crates.io), **dual MIT/Apache-2.0 licence**, **PI non-affiliation disclaimer**,
  **no standard text** or GPL code in the repo. *Nothing pushed public before
  the standard is acquired and the final name validated.*

## 10. Possible future extensions (out of current scope)

- **CC-B**: add LLDP + topology data (enhanced network diagnostics).
- Alternative `eth` backend (XDP/AF_XDP) for even lower latency.
- Multi-AR / Shared-Device.
- Shorter cycles (500/250 µs) depending on actual jitter measurements.

## 11. Naming & deliverables

- Project: `203-profinet-rt` — Crate: **`profinet-rt`**.
- Deliverables: the crate, the **example GSDML** (16 `REAL` + 32 `BOOL`), a demo
  binary, configuration and deployment documentation (PREEMPT_RT tuning).

# Plan 8 — PDEV (`lldp` + `pdev`), DCP Set/Signal/Reset, GSDML V2.31+ — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** TIA V21 sees `pnio` as a full topology node (LLDP, PDEV records, remote-mismatch diagnosis), writes its I&M1-3, and can assign its name/IP, flash it and reset it to factory over DCP — all persisted; GSDML at `PNIO_Version="V2.31"`; release 0.2.0.

**Architecture:** Two new pure modules, `lldp` (LLDPDU codec) and `pdev` (port state, peer tracking, PDEV record builders, mismatch evaluation), driven from the existing single-threaded acyclic loop in `device::step` through a second `AF_PACKET` socket (EtherType `0x88CC`). `dcp` grows the Set-Name/Signal/Reset blocks, a `NetParams` identity, a `NetStore` file and an `IpConfigurator`. `cm` gets per-index record dispatch with per-record Write statuses. `diag` learns the extended channel diagnosis. The RT thread is untouched.

**Tech Stack:** Rust 2021 (MSRV 1.74), `nix 0.31`, `libc`, `thiserror 2`, `log`; dev: `roxmltree 0.21`, `clap 4`. No new dependencies.

**Spec:** `docs/design/2026-10-10-pnio-pdev-lldp-dcp-design.md` (read it first; §4 wire formats, §5 modules, §6 HIL, §9 edge cases).

## Global Constraints

- MSRV 1.74 (`rust-version = "1.74"` in `Cargo.toml`); CI builds the lib on 1.74 and stable, `cargo clippy -D warnings`, `cargo doc -D warnings`, `#![warn(missing_docs)]` = every `pub` item documented.
- `#![deny(unsafe_code)]` crate-wide; `unsafe` only in `eth::afpacket`, `eth::poll`, `rt::sched`, `rt::runner` (already `#[allow(unsafe_code)]`).
- No new crate dependencies. No tokio. Nothing new on the RT thread.
- Linux-only code behind `#[cfg(target_os = "linux")]` (api, afpacket, examples under `examples/linux/`); `cargo check --target x86_64-apple-darwin --all-targets` must keep passing.
- Tests: unit tests inline (`#[cfg(test)] mod tests`), goldens as hex in `crates/pnio/testdata/<module>/*.hex` loaded through `crate::testutil` (unit) or `tests/common/mod.rs` (integration). Run with `cargo test -p pnio` (NTFS: the workspace already runs fine single-threaded; if a filesystem test flakes, re-run with `--test-threads=1`).
- Commits: conventional prefix (`feat(dcp):`, `test(lldp):`, `docs:`), end with `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` — every commit step below, including those written as a one-line message. Push after each merge to `main` (NTFS/WSL2 corruption risk).
- Work on branch `feat/pdev-lldp-dcp` in a worktree (`.worktrees/feat-pdev-lldp-dcp`); merge fast-forward to `main` at the end (Task 16).
- Every `(verify)` item in spec §4 is resolved in `docs/pdev-golden-frames.md` (Task 2) **before** the task that implements it starts; the implementer reads that document, not the spec's guess. A provisional, dissector-sourced version of that document already exists (commit `6c2ba3a`): implementers read it now; Task 2 confirms or overrides each line from the captures.
- Bench-free execution: Tasks 3-14 do not wait for the Task 2 captures. Tests that would load a Task 2 golden build a **synthetic** frame instead, from the facts in `docs/pdev-golden-frames.md`, in a helper commented `// synthetic — replace by the Task 2 golden`; Task 2, when it runs, adds the golden-based twin next to each of them.

## Deviations from spec (ruled 2026-10-10)

The spec (`docs/design/2026-10-10-pnio-pdev-lldp-dcp-design.md`) is the authority except on these points, where the plan's form was ruled in:

- `dcp::decide_set` takes `&SetRequest` only (no current properties): the "IP must equal configured" guard is gone (spec decision 2).
- `SetBlock` shapes are the plan's (Task 4: `NameOfStation { qualifier, name }`, `Signal { qualifier, value }`, `ResetToFactory { qualifier }`, `IpSuite`/`Other` unchanged).
- `lldp` type names are the plan's (Task 7: `TlvId`, `MacPhy`, `Lldpdu`, `Peer`, `LldpError`).
- `Pdev` is owned by `Cm` (`Cm::set_pdev`/`pdev`/`pdev_mut`) and the PDEV records are free functions in `pdev::records` (`read`, `parse_check`, builders); spec §5's `Pdev::write_record`/`read_record` methods and "Device owns Pdev" are superseded.
- `RecordCtx` gains `pdev: Option<&Pdev>` only; the net identity and MAC are reached through `Pdev::net()` / `Pdev::mac()`.
- `diag`: a port-only `DiagStore::set_port_mismatch` (Task 11); no generic `DiagEntry.ext` / `raise_ext`.
- `DeviceHooks::default()` = `NoopIp` + an always-up link + no LLDP transport + no signal hook; the facade (`api::IoDevice::start`) supplies the real implementations.
- The capture campaign is Task 2 (spec "task 0").
- Effect order of a DCP Set follows the **spec**: apply → persist → respond → abort → LLDPDU (Task 12 `handle_set`).
- A ResetToFactory whose `IpConfigurator` call fails answers `SetNotPossible` and applies/persists nothing (spec §9).
- Constants are defined once: `ETHERTYPE_LLDP` and `LLDP_MULTICAST_MAC` live in `eth::header` (Task 6); `lldp` and `EthProto::LLDP` import them (`lldp` depends on `eth`, not the reverse). `dcp::ipconf` reuses `dcp::store::dotted` (`pub(crate)`).
- The TTL-0 LLDPDU at shutdown is best effort: `if let Err(e) = t.send(..) { log::warn!(..) }`.

## Review Focus

Inputs the spec implies but no task's tests exercised at first draft; each now has a test pinned to the owning task:

1. A DCP Set carrying **both** NameOfStation and IPParameter in one request (TIA "Assign device name" + "Assign IP" in one go) — both must apply, one response block each, one abort, one store write (Task 12).
2. A `PDPortDataCheck` with **two** peers in `CheckPeers` (TIA "alternative partner") — we have one port: evaluation must treat "any expected peer matches" as a match, never panic (Task 8).
3. An LLDPDU from a **non-PROFINET** neighbour (a managed switch with system-name TLVs, no PROFINET TLVs, chassis subtype MAC) — must parse into a `Peer` with a MAC-string chassis id and not be dropped (Task 7).
4. `NetStore` file with **CRLF** line endings (edited on Windows over the WSL mount) — must load (Task 5).
5. A station name of exactly **240 bytes**, and of 241 — boundary accepted / rejected (Task 3).

---

## File structure

```
crates/pnio/src/
  dcp/mod.rs        (modify) dispatcher returns decisions instead of a frame; NetParams; re-exports
  dcp/set.rs        (modify) SetBlock + Effect + decide_set rewrite
  dcp/name.rs       (create) station-name syntax
  dcp/store.rs      (create) NetStore
  dcp/ipconf.rs     (create) IpConfigurator, IpCommand (linux), MockIp, NoopIp
  eth/proto.rs      (create) EthProto + accepts (all platforms)
  eth/header.rs     (modify) ETHERTYPE_LLDP, LLDP_MULTICAST_MAC
  eth/afpacket.rs   (modify) open_with(EthProto); is_profinet_frame removed
  eth/mod.rs        (modify) export EthProto, ETHERTYPE_LLDP, LLDP_MULTICAST_MAC
  lldp/mod.rs       (create) Lldpdu, TlvId, MacPhy, Peer, encode/parse, build_device_pdu, port_id_string, parse_frame
  pdev/mod.rs       (create) Pdev, LinkProbe, Mismatch, PdevAction, CheckRecord, evaluate/tick/on_rx
  pdev/records.rs   (create) PDEV record builders (read) and CheckPeers/CheckMAUType parse (write)
  cm/records.rs     (modify) read dispatch for 0x802A/0x802B/0x802F/0x8080/0xF841/0xF842; write_pdev_record
  cm/write.rs       (modify) build_write_res with per-record statuses
  cm/ar.rs          (modify) AbortReason::{DcpSet, ResetToFactory}; handle_write returns Action::WriteAccepted { req: WriteReq }; Ar::set_station_name
  cm/mod.rs         (modify) write status computation, set_station_name, RecordCtx with pdev
  testutil.rs       (modify) golden_in (Task 2), dcp_set_frame (Task 4), im1_record (Task 5), synthetic PDEV records (Task 9)
  diag.rs           (modify) port mismatch entry (ExtChannelDiagnosis)
  device/mod.rs     (modify) NetParams, DCP effects, LLDP socket, pdev tick, hooks, shutdown PDU
  api.rs            (modify) StartOptions, DeviceHooks plumbing, accessors
  gsdml.rs          (modify) V2.31 attributes
  lib.rs            (modify) pub mod lldp; pub mod pdev
  im.rs             (modify) ImStore::clear
crates/pnio/testdata/{lldp,pdev,dcp}/*.hex   (create, Task 2)
crates/pnio/tests/pdev_replay.rs             (create)
crates/pnio/examples/linux/typed_bringup.rs  (modify)
docs/pdev-golden-frames.md (create), docs/gsdml.md, docs/bench-pnet-device.md §6j, FOLLOWUPS.md, CHANGELOG.md, README.md
```

---

### Task 0: Branch and worktree

**Files:** none (git only)

- [ ] **Step 1: Create the worktree**

```bash
cd /mnt/c/Users/martinc8/projects/203-profinet-rt
git worktree add .worktrees/feat-pdev-lldp-dcp -b feat/pdev-lldp-dcp main
cd .worktrees/feat-pdev-lldp-dcp
cargo test -p pnio 2>&1 | tail -3
```

Expected: `test result: ok. 300 passed` (plus the integration suites), 0 failed.

- [ ] **Step 2: Note the `.worktrees/` entry is git-ignored** (it is, since Plan 3). Nothing to commit.

---

### Task 1: GSDML V2.31 profile

**Files:**
- Modify: `crates/pnio/src/gsdml.rs:116-148` (module doc), `:188` (DAP line), `:230-238` (interface/port items), tests `:440-466`
- Modify: `crates/pnio/testdata/gsdml/sample-16real-32bool.xml`
- Modify: `docs/gsdml.md` (validation section)

**Interfaces:**
- Produces: a GSD TIA's checker accepts at V2.31. No Rust API change.

- [ ] **Step 1: Flip the structural test to the new claims**

In `gsdml.rs` tests, replace the `PNIO_Version`/`SupportedProtocols`/absent-claims block with:

```rust
        // V2.31: the lowest PNIO_Version that triggers TIA's checker rules for
        // LLDP/boundary/ResetToFactory/CertificationInfo — all of which Plan 8 implements.
        assert_eq!(dap.attribute("PNIO_Version"), Some("V2.31"));
        assert_eq!(dap.attribute("LLDP_NoD_Supported"), Some("true"));
        assert_eq!(dap.attribute("ResetToFactoryModes"), Some("2 4"));
        let cert = find("CertificationInfo");
        assert_eq!(cert.len(), 1, "exactly one CertificationInfo under the DAP");
        assert_eq!(cert[0].attribute("ConformanceClass"), Some("A"));
        assert_eq!(cert[0].attribute("ApplicationClass"), Some(""));
        assert_eq!(cert[0].attribute("NetloadClass"), Some("I"));
        let iface = find("InterfaceSubmoduleItem")[0];
        assert_eq!(iface.attribute("SupportedProtocols"), Some("LLDP"));
        assert_eq!(iface.attribute("PTP_BoundarySupported"), Some("true"));
        assert_eq!(iface.attribute("DCP_BoundarySupported"), Some("true"));
        assert_eq!(iface.attribute("DCP_HelloSupported"), Some("false"));
        assert_eq!(iface.attribute("DelayMeasurementSupported"), Some("false"));
```

- [ ] **Step 2: Run, expect failure**

Run: `cargo test -p pnio gsdml::tests::render_is_well_formed -- --nocapture`
Expected: FAIL on `PNIO_Version` (`Some("V2.3")`).

- [ ] **Step 3: Change the render**

In `render`, the DAP line becomes:

```rust
        <DeviceAccessPointItem ID="DAP1" PNIO_Version="V2.31" PhysicalSlots="0..{n_slots}" ModuleIdentNumber="0x00000001" MinDeviceInterval="{mdi}" DNS_CompatibleName="{station}" FixedInSlots="0" ObjectUUID_LocalIndex="1" MultipleWriteSupported="true" DeviceAccessSupported="false" CheckDeviceID_Allowed="true" NameOfStationNotTransferable="false" LLDP_NoD_Supported="true" ResetToFactoryModes="2 4">
          <ModuleInfo>
            <Name TextId="T_DAP_Name"/>
            <InfoText TextId="T_DAP_Info"/>
            <VendorName Value="{vendor}"/>
{identity}
          </ModuleInfo>
          <CertificationInfo ConformanceClass="A" ApplicationClass="" NetloadClass="I"/>
          <IOConfigData MaxInputLength="{max_in}" MaxOutputLength="{max_out}" MaxDataLength="{max_data}"/>
```

and the interface item:

```rust
            <InterfaceSubmoduleItem ID="DAP1_IF" SubslotNumber="32768" TextId="T_Interface" SubmoduleIdentNumber="0x00008000" SupportedRT_Classes="RT_CLASS_1" SupportedProtocols="LLDP" DCP_HelloSupported="false" PTP_BoundarySupported="true" DCP_BoundarySupported="true" DelayMeasurementSupported="false">
```

**Element order inside `DeviceAccessPointItem` is fixed by the XSD** (`ModuleInfo`, `CertificationInfo`, `SubslotList?`, `IOConfigData`, `UseableModules`, …). If `lxml` validation in step 6 reports an order error, move `CertificationInfo` to where the XSD puts it and update the golden; do not drop it.

- [ ] **Step 4: Regenerate the golden**

```bash
cargo run -p pnio --example gen_gsdml -- --out /tmp/gsd
cp /tmp/gsd/GSDML-V2.4-CoreEngineering-pnio-20260829.xml crates/pnio/testdata/gsdml/sample-16real-32bool.xml
```

(`--out` is a **directory** (`examples/gen_gsdml.rs` `Args::out`); the file name comes from `gsdml::file_name(&meta)`. Checked: `render_matches_the_golden` compares `render(&sample(), &meta())` (the `gsdml.rs` test module's `sample()`/`meta()`) with this file; the example's default arguments build the same configuration and the same `GsdmlMeta` (same vendor name, family, info text, date `2026-08-29`) — the only difference, `station_type("pnio sample device")`, is not rendered into the GSD — so the example's output is byte-identical to `render(&sample(), &meta())`.) Then `cargo test -p pnio gsdml` → both `render_matches_the_golden` and the structural test PASS.

- [ ] **Step 5: Update the module doc** (`gsdml.rs:116-148`): replace the paragraph explaining V2.3 by one sentence per new attribute saying what in the crate backs it (`lldp` module, `dcp` ResetToFactory modes 2/4, PDEV boundary records stored by `pdev`, CC-A as the honest conformance claim). Keep the `MAUTypeItem` and `SupportedProtocols` notes, updated.

- [ ] **Step 6: Validate against the XSD (from WSL, lxml 6 installed)**

Following `docs/gsdml.md#validation`, against both the v2.31 and the v2.4 schema (same directory of the TIA V21 install, read through the `/mnt/c` mount):

```bash
for v in v2.31 v2.4; do
python3 -c "
import sys
from lxml import etree
xsd = '/mnt/c/Program Files/Siemens/Automation/Portal V21/Data/Hwcn/Custom/Interpreter/GSD/XSD/GSDML-DeviceProfile-' + sys.argv[1] + '.xsd'
s = etree.XMLSchema(etree.parse(xsd))
d = etree.parse('crates/pnio/testdata/gsdml/sample-16real-32bool.xml')
print(sys.argv[1], s.validate(d)); print(s.error_log)
" $v
done
```

Expected: `v2.31 True`, `v2.4 True`, empty error logs. Record the outcome in `docs/gsdml.md` (new subsection "V2.31 profile (Plan 8)": attributes added, XSD results, date). The TIA import (uninstall the old `pnio-dev` GSD first, same name; the GSD checker must report no error) is a manual step the user runs later on the Windows host; leave a "TIA import: pending (user)" line in that subsection.

- [ ] **Step 7: Commit**

```bash
git add crates/pnio/src/gsdml.rs crates/pnio/testdata/gsdml/sample-16real-32bool.xml docs/gsdml.md
git commit -m "feat(gsdml): V2.31 profile — LLDP_NoD, ResetToFactoryModes 2 4, CertificationInfo CC-A, LLDP + boundary flags on the interface

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 2: Capture campaign and goldens (bench, manual + extraction)

Bench task: it may run after Tasks 3-13 (bench-free execution, see Global Constraints). Until it runs, the tests of Tasks 4, 7, 9, 10 and 12 use synthetic frames; Task 14's `pdev_replay.rs` waits for it.

**Files:**
- Create: `crates/pnio/testdata/lldp/cpu_x1.hex` (the 1515's LLDPDU, full Ethernet frame)
- Create: `crates/pnio/testdata/pdev/connect_write_check.hex` (the Connect-phase MultipleWrite carrying `0x802B`/`0x802F`/`0x8071`, PNIO blocks only, same offset convention as `testdata/cm/write_req.hex`)
- Create: `crates/pnio/testdata/pdev/read_802a_req.hex`, `read_8080_req.hex`, `read_f841_req.hex`, `read_f842_req.hex` (TIA read requests, PNIO blocks)
- Create: `crates/pnio/testdata/dcp/set_name_req.hex`, `set_ip_req.hex`, `signal_req.hex`, `reset_keep_im_req.hex`, `reset_all_req.hex` (full Ethernet frames, VLAN-tagged or not — the Task 4 `blocks_of` helper handles both)
- Modify: `docs/pdev-golden-frames.md` (provisional dissector-sourced version from commit `6c2ba3a`: add §4 campaign and §5 decoded goldens, confirm or override every line of §1-3 and §6)
- Modify: the test modules of `dcp/set.rs`, `lldp/mod.rs`, `pdev/records.rs`, `cm/mod.rs` (golden twins of the synthetic tests, step 5)
- Modify: `captures/README.md` (git-ignored directory; the README lists the campaign)

**Interfaces:**
- Produces: the hex goldens above and the pinned facts in `docs/pdev-golden-frames.md`: the Port ID format, the ResetToFactory qualifier encoding, the exact `BlockType`/`BlockVersion` of every PDEV block TIA wrote or read, the CheckPeers content TIA writes for our topology, the LLDP TLVs the CPU sends (and their order), the MAC/PHY TLV bytes.

- [ ] **Step 1: Prepare the bench**

Edge: current `main` binary (`~/bench/typed_bringup`, Plan 5 build, md5 `05e869f2…`), `pnio-dev` at 500 µs, cabled on X1. TIA: the Task 1 GSD installed, device re-added if needed, **topology configured** (Topology view: CPU X1 P1 ↔ `pnio-dev` port 1), I&M1 plant designation and location filled in the device properties, project compiled and downloaded.

- [ ] **Step 2: Capture, one file per step**, `captures/plan8-<YYYYMMDD>/`, with `sudo tcpdump -i eno2 -w <name>.pcapng` (no filter — the `vlan` keyword trap from Plan 3):

| file | action |
|---|---|
| `connect.pcapng` | restart `typed_bringup`, wait for AR `Data`, 30 s |
| `lldp.pcapng` | 30 s idle (≥ 5 CPU LLDPDUs) |
| `reads.pcapng` | TIA online on the device, *Diagnostics → PROFINET interface → Ports* open, 60 s |
| `im-write.pcapng` | download with changed I&M1 fields (H1 test) |
| `dcp-name.pcapng` | *Online & diagnostics → Functions → Assign PROFINET device name*, new name `pnio-dev-b` |
| `dcp-ip.pcapng` | *Assign IP address*, same IP re-assigned |
| `dcp-flash.pcapng` | *Flash LED* |
| `dcp-reset-keep-im.pcapng` | *Reset to factory settings*, "Retain I&M data" ticked |
| `dcp-reset-all.pcapng` | same, unticked |

The device answers Set with `SuboptionNotSupported` today; TIA will show an error — expected, we want the requests.

- [ ] **Step 3: Extract the goldens**

Use Wireshark (Windows) or `tshark` on the edge: `tshark -r connect.pcapng -Y 'pn_io.index == 0x802b' -T fields -e frame.number`, then `File → Export Packet Bytes` (or `tshark -x`) into hex files with the same line format as `testdata/cm/write_req.hex` (check `crates/pnio/src/testutil.rs:14` `parse_hex` for what it accepts: whitespace-separated hex bytes, `#` comments). PNIO-block goldens start at the first PNIO `BlockHeader` (strip Ethernet/IP/UDP/RPC/NDR: offset 142 for a Write request as in `write.rs` tests, `RPC_OFF + 80 + 20` for a Read request as in `records.rs` tests — verify on the frame).

- [ ] **Step 4: Complete `docs/pdev-golden-frames.md`** (it already holds the provisional dissector facts; every confirmed line gets the frame number that proves it, every contradicted one is corrected and the code/test that used it is fixed in its own commit) so that it holds, in this order:
  1. Campaign table (file, date, what).
  2. **Pinned facts** (one line each, with the frame number proving it): Port ID string the CPU sends (`port-001` or `port-001.<name>`); TLV list and order of the CPU's LLDPDU with raw bytes of the PROFINET and MAC/PHY TLVs; `BlockType`/`BlockVersion` of PDPortDataCheck, CheckPeers, CheckMAUType (if present), PDPortDataAdjust and each Adjust sub-block, PDInterfaceAdjust; the exact CheckPeers payload TIA wrote (peer port id + chassis id strings); the ResetToFactory `BlockQualifier` values for both actions; the Signal block value; the NameOfStation qualifier value; which indices TIA reads, with their `(slot, subslot)` and `record_data_length`; whether `im-write.pcapng` contains `0xAFF1` writes (H1 result).
  3. Every spec §4 `(verify)` item restated with its resolved value.
  4. Decoded dumps of each golden (Wireshark-style), as `docs/dcp-golden-frames.md` does.

- [ ] **Step 5: Add a parse-only test per golden family** so the files are exercised from day one. In `crates/pnio/src/testutil.rs` add:

```rust
/// A Plan 8 golden from `testdata/<dir>/<name>.hex`.
pub fn golden_in(dir: &str, name: &str) -> Vec<u8> {
    let path = format!("{}/testdata/{dir}/{name}.hex", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    parse_hex(&text)
}
```

and in `crates/pnio/src/cm/write.rs` tests:

```rust
    #[test]
    fn plan8_connect_write_carries_the_pdev_records() {
        let w = WriteReq::parse(&crate::testutil::golden_in("pdev", "connect_write_check")).unwrap();
        let idx: Vec<u16> = w.records.iter().map(|r| r.index).collect();
        assert!(idx.contains(&0x802B), "PDPortDataCheck present: {idx:x?}");
        assert!(idx.contains(&0x802F), "PDPortDataAdjust present: {idx:x?}");
    }
```

Run: `cargo test -p pnio plan8_connect_write` → PASS. (If TIA wrote no `0x802F`, the pinned facts say so and the assertion is reduced to `0x802B`.)

Then, next to every synthetic test (each helper commented `// synthetic — replace by the Task 2 golden`), add its golden twin with the same assertions, the frame coming from `golden_in(..)` instead of the helper: `dcp/set.rs` `parses_set_name_request`/`parses_signal_and_reset_requests` (`golden_in("dcp", "set_name_req" | "signal_req" | "reset_keep_im_req" | "reset_all_req")`), `lldp` `parses_the_cpu_lldpdu` (`golden_in("lldp", "cpu_x1")`), `pdev::records` `parses_a_check_peers_record` and `read_requests_target_the_indices_we_serve` (`golden_in("pdev", "connect_write_check" | "read_802a_req" | "read_8080_req" | "read_f841_req" | "read_f842_req")`), `cm` `pdev_check_write_is_stored_and_read_back_and_f841_is_served` (the golden's PNIO blocks wrapped with that test module's `rpc_request`). Name each twin `<synthetic name>_golden`. If a golden contradicts a synthetic assertion, the golden wins: fix the synthetic helper, the code and `docs/pdev-golden-frames.md` together. Run `cargo test -p pnio golden` → PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/pnio/testdata/lldp crates/pnio/testdata/pdev crates/pnio/testdata/dcp docs/pdev-golden-frames.md crates/pnio/src
git commit -m "test: Plan 8 goldens from the 1515 X1 campaign (LLDP, PDEV writes/reads, DCP name/IP/signal/reset) + pinned wire facts

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 3: `dcp::name` — station-name syntax

**Files:**
- Create: `crates/pnio/src/dcp/name.rs`
- Modify: `crates/pnio/src/dcp/mod.rs` (`pub mod name;`, `pub use name::{validate_station_name, NameError};`)

**Interfaces:**
- Produces: `pub fn validate_station_name(name: &str) -> Result<(), NameError>`; `pub enum NameError { Empty, TooLong(usize), BadChar(char), BadLabel, LooksLikePort, LooksLikeIp }`.

- [ ] **Step 1: Write the failing tests** (`name.rs`, bottom):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_and_dotted_names() {
        for n in ["pnio-dev", "pnio-dev-b", "a", "device-1.plant-2.site", "x".repeat(240).as_str()] {
            assert_eq!(validate_station_name(n), Ok(()), "{n}");
        }
    }

    #[test]
    fn rejects_empty_too_long_uppercase_and_bad_labels() {
        assert_eq!(validate_station_name(""), Err(NameError::Empty));
        assert_eq!(validate_station_name(&"x".repeat(241)), Err(NameError::TooLong(241)));
        assert_eq!(validate_station_name("Pnio"), Err(NameError::BadChar('P')));
        assert_eq!(validate_station_name("pnio_dev"), Err(NameError::BadChar('_')));
        assert_eq!(validate_station_name("-pnio"), Err(NameError::BadLabel));
        assert_eq!(validate_station_name("pnio-"), Err(NameError::BadLabel));
        assert_eq!(validate_station_name("a..b"), Err(NameError::BadLabel));
        assert_eq!(validate_station_name(&format!("{}.b", "x".repeat(64))), Err(NameError::BadLabel));
    }

    #[test]
    fn rejects_port_names_and_ip_literals() {
        assert_eq!(validate_station_name("port-001"), Err(NameError::LooksLikePort));
        assert_eq!(validate_station_name("port-001-12345"), Err(NameError::LooksLikePort));
        assert_eq!(validate_station_name("port-001.pnio-dev"), Ok(())); // only the *first label* form is reserved
        assert_eq!(validate_station_name("192.168.0.50"), Err(NameError::LooksLikeIp));
        assert_eq!(validate_station_name("1.2.3"), Ok(())); // not four numeric labels
    }
}
```

- [ ] **Step 2: Run, expect compile failure** — `cargo test -p pnio dcp::name` → "cannot find function `validate_station_name`".

- [ ] **Step 3: Implement**

```rust
//! `NameOfStation` syntax (IEC 61158-6-10 §4.3.1.4.15, DNS-label form).

use thiserror::Error;

/// Why a station name was refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NameError {
    /// Empty name (only ResetToFactory may blank the name).
    #[error("empty name")]
    Empty,
    /// Longer than 240 bytes.
    #[error("name is {0} bytes, max 240")]
    TooLong(usize),
    /// A character outside `[a-z0-9-.]`.
    #[error("invalid character {0:?}")]
    BadChar(char),
    /// A label is empty, longer than 63, or starts/ends with `-`.
    #[error("invalid label")]
    BadLabel,
    /// The first label is `port-xyz` or `port-xyz-abcde` (reserved for port IDs).
    #[error("reserved port-name form")]
    LooksLikePort,
    /// Four numeric labels: an IPv4 literal.
    #[error("looks like an IPv4 address")]
    LooksLikeIp,
}

/// Maximum `NameOfStation` length in bytes.
pub const MAX_NAME_LEN: usize = 240;

/// Validate a `NameOfStation` against the DNS-label rules.
pub fn validate_station_name(name: &str) -> Result<(), NameError> {
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if name.len() > MAX_NAME_LEN {
        return Err(NameError::TooLong(name.len()));
    }
    if let Some(c) = name
        .chars()
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '.'))
    {
        return Err(NameError::BadChar(c));
    }
    let labels: Vec<&str> = name.split('.').collect();
    for l in &labels {
        if l.is_empty() || l.len() > 63 || l.starts_with('-') || l.ends_with('-') {
            return Err(NameError::BadLabel);
        }
    }
    if labels.len() == 4 && labels.iter().all(|l| l.bytes().all(|b| b.is_ascii_digit())) {
        return Err(NameError::LooksLikeIp);
    }
    if is_port_form(labels[0]) {
        return Err(NameError::LooksLikePort);
    }
    Ok(())
}

/// `port-xyz` or `port-xyz-abcde` with x, y, z, a..e digits.
fn is_port_form(label: &str) -> bool {
    let parts: Vec<&str> = label.split('-').collect();
    let digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
    match parts.as_slice() {
        ["port", xyz] => digits(xyz, 3),
        ["port", xyz, abcde] => digits(xyz, 3) && digits(abcde, 5),
        _ => false,
    }
}
```

- [ ] **Step 4: Run** `cargo test -p pnio dcp::name` → 3 PASS.

- [ ] **Step 5: Commit** — `git add crates/pnio/src/dcp/name.rs crates/pnio/src/dcp/mod.rs && git commit -m "feat(dcp): NameOfStation syntax validation" -m "Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"`

---

### Task 4: `dcp::set` — NameOfStation / Signal / ResetToFactory blocks, `NetParams`, pure decisions

**Files:**
- Modify: `crates/pnio/src/dcp/set.rs` (whole decision layer), `crates/pnio/src/dcp/mod.rs` (`NetParams`, re-exports), `crates/pnio/src/testutil.rs` (`dcp_set_frame`)
- Test: inline in `set.rs`; synthetic TIA Set requests built with `testutil::dcp_set_frame` from `docs/pdev-golden-frames.md` §1 (the Task 2 goldens `testdata/dcp/*.hex` get twins later), and `testdata/cm/dcp_set_req.hex` (p-net Set-IP)

**Interfaces:**
- Produces:

```rust
/// The device's identity on the wire: the single source of truth `Device` owns.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NetParams { pub name: String, pub ip: [u8; 4], pub subnet: [u8; 4], pub gateway: [u8; 4] }
impl NetParams { pub fn is_blank(&self) -> bool; pub fn prefix_len(&self) -> u8; }

pub enum SetBlock {
    IpSuite { qualifier: u16, ip: [u8;4], subnet: [u8;4], gateway: [u8;4] },   // unchanged
    NameOfStation { qualifier: u16, name: String },
    Signal { qualifier: u16, value: u16 },
    ResetToFactory { qualifier: u16 },
    Other { option: u8, suboption: u8 },
}
pub const QUALIFIER_PERMANENT: u16 = 0x0001;
/// What `Device` must do for one accepted block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect { SetName { name: String, permanent: bool }, SetIp { ip: [u8;4], subnet: [u8;4], gateway: [u8;4], permanent: bool }, Signal, Reset { mode: u8 } }
/// One block's outcome: the Control/Response tuple plus the effect to apply when `Ok`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetDecision { pub option: u8, pub suboption: u8, pub error: SetBlockError, pub effect: Option<Effect> }
pub fn decide_set(req: &SetRequest) -> Vec<SetDecision>;
pub fn reset_mode(qualifier: u16) -> u8;            // ((qualifier >> 1) & 0xF), docs/pdev-golden-frames.md §1.2
pub fn build_set_response(dst: MacAddr, src: MacAddr, xid: u32, results: &[(u8, u8, SetBlockError)]) -> Vec<u8>; // unchanged
// crate::testutil (test-only):
pub fn dcp_set_frame(dst: MacAddr, src: MacAddr, xid: u32, blocks: &[DcpBlock]) -> Vec<u8>;   // untagged DCP Set request
```

- `decide_set` no longer takes the current properties: the old "IP must equal configured" rule is gone (spec decision 2). The signature change is deliberate; `handle_dcp_frame` is the only caller until Task 12 moves the dispatch into `Device`.

- [ ] **Step 1: Write the failing tests** (replace the existing `decide_set` tests that assert `SetNotPossible` on a different IP; keep the response-building golden test `dcp_set_res`).

First the shared frame builder, in `crates/pnio/src/testutil.rs` (Task 12 reuses it):

```rust
/// An untagged DCP Set request frame: `EthHeader` (PROFINET EtherType) + FrameID
/// `0xFEFD` + `DcpHeader { Set, Request, xid, response_delay 0, data_length }` + the
/// blocks, whose `block_info` carries the request's `BlockQualifier`.
pub fn dcp_set_frame(
    dst: crate::eth::MacAddr,
    src: crate::eth::MacAddr,
    xid: u32,
    blocks: &[crate::dcp::DcpBlock],
) -> Vec<u8> {
    use crate::dcp::block::blocks_encoded_len;
    use crate::dcp::{write_blocks, DcpHeader, FrameId, ServiceId, ServiceType};
    use crate::eth::{EthHeader, ETHERTYPE_PROFINET};
    let mut f = Vec::with_capacity(64);
    EthHeader { dst, src, vlan: None, ethertype: ETHERTYPE_PROFINET }.write(&mut f);
    f.extend_from_slice(&FrameId::GetSet.to_u16().to_be_bytes());
    DcpHeader {
        service_id: ServiceId::Set,
        service_type: ServiceType::Request,
        xid,
        response_delay: 0,
        data_length: blocks_encoded_len(blocks),
    }
    .write(&mut f);
    write_blocks(blocks, &mut f);
    f
}
```

Then in `set.rs` tests (`use crate::eth::EthHeader;`, `use crate::dcp::{DcpBlock, DcpHeader};`):

```rust
    /// The DCP blocks of a Set request frame, tagged or not: Ethernet header (14 or
    /// 18 bytes) + FrameID 2 + DCP header 10, sliced to `DCPDataLength` so the
    /// Ethernet minimum-size padding of a captured frame is not read as blocks.
    fn blocks_of(frame: &[u8]) -> &[u8] {
        let (_, off) = EthHeader::parse(frame).unwrap();
        let (_, blocks) = DcpHeader::parse(&frame[off + 2..]).unwrap();
        blocks
    }

    // synthetic — replace by the Task 2 golden (`golden_in("dcp", name)`).
    // docs/pdev-golden-frames.md §1: NameOfStation (2/2) qualifier 0x0001 = permanent;
    // Signal (5/3) qualifier 0 + SignalValue 0x0100 "Flash Once"; ResetToFactory (5/6)
    // BlockQualifier = mode << 1 (mode 2 → 0x0004, mode 4 → 0x0008), no value.
    fn tia_set(name: &str) -> Vec<u8> {
        let block = |option, suboption, qualifier, value: &[u8]| DcpBlock {
            option,
            suboption,
            block_info: Some(qualifier),
            value: value.to_vec(),
        };
        let b = match name {
            "set_name_req" => block(2, 2, 0x0001, b"pnio-dev-b"),
            "signal_req" => block(5, 3, 0x0000, &[0x01, 0x00]),
            "reset_keep_im_req" => block(5, 6, 0x0004, &[]),
            "reset_all_req" => block(5, 6, 0x0008, &[]),
            other => panic!("no synthetic DCP request {other}"),
        };
        crate::testutil::dcp_set_frame(
            MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]),
            MacAddr([0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f]),
            0x0300_0200,
            &[b],
        )
    }

    #[test]
    fn parses_set_name_request() {
        let req = parse_set_request(blocks_of(&tia_set("set_name_req"))).unwrap();
        assert_eq!(
            req.blocks,
            vec![SetBlock::NameOfStation { qualifier: QUALIFIER_PERMANENT, name: "pnio-dev-b".into() }]
        );
    }

    #[test]
    fn parses_signal_and_reset_requests() {
        let s = parse_set_request(blocks_of(&tia_set("signal_req"))).unwrap();
        assert!(matches!(s.blocks[0], SetBlock::Signal { value: 0x0100, .. }));
        let keep = parse_set_request(blocks_of(&tia_set("reset_keep_im_req"))).unwrap();
        let all = parse_set_request(blocks_of(&tia_set("reset_all_req"))).unwrap();
        let SetBlock::ResetToFactory { qualifier: q_keep } = keep.blocks[0] else { panic!() };
        let SetBlock::ResetToFactory { qualifier: q_all } = all.blocks[0] else { panic!() };
        assert_eq!(reset_mode(q_keep), 2);
        assert_eq!(reset_mode(q_all), 4);
    }

    #[test]
    fn reset_mode_reads_bits_1_to_4() {
        assert_eq!(reset_mode(0x0004), 2);
        assert_eq!(reset_mode(0x0008), 4);
        assert_eq!(reset_mode(0x0010), 8); // mode 8 survives the decode (no truncation to 0)
        assert_eq!(reset_mode(0x0012), 9);
        assert_eq!(reset_mode(0x0005), 2); // bit 0 is not part of the mode
    }

    #[test]
    fn decides_every_block_kind() {
        let req = SetRequest {
            blocks: vec![
                SetBlock::NameOfStation { qualifier: 1, name: "ok-name".into() },
                SetBlock::NameOfStation { qualifier: 0, name: "Bad".into() },
                SetBlock::IpSuite { qualifier: 0, ip: [10, 0, 0, 2], subnet: [255, 255, 0, 0], gateway: [10, 0, 0, 1] },
                SetBlock::Signal { qualifier: 0, value: 0x0100 },
                SetBlock::Signal { qualifier: 0, value: 0x0200 },
                SetBlock::ResetToFactory { qualifier: 2 << 1 },
                SetBlock::ResetToFactory { qualifier: 8 << 1 },
                SetBlock::Other { option: 2, suboption: 9 },
            ],
        };
        let d = decide_set(&req);
        assert_eq!(d[0].error, SetBlockError::Ok);
        assert_eq!(d[0].effect, Some(Effect::SetName { name: "ok-name".into(), permanent: true }));
        assert_eq!((d[0].option, d[0].suboption), (2, 2));
        assert_eq!(d[1].error, SetBlockError::SuboptionNotSet);
        assert_eq!(d[1].effect, None);
        assert_eq!(d[2].effect, Some(Effect::SetIp { ip: [10, 0, 0, 2], subnet: [255, 255, 0, 0], gateway: [10, 0, 0, 1], permanent: false }));
        assert_eq!(d[3].effect, Some(Effect::Signal));
        assert_eq!(d[4].error, SetBlockError::SuboptionNotSet);
        assert_eq!(d[5].effect, Some(Effect::Reset { mode: 2 }));
        assert_eq!(reset_mode(8 << 1), 8, "refused because mode 8 is unsupported, not because it decoded to 0");
        assert_eq!(d[6].error, SetBlockError::SuboptionNotSupported);
        assert_eq!((d[7].option, d[7].suboption, d[7].error), (2, 9, SetBlockError::SuboptionNotSupported));
    }

    #[test]
    fn any_ip_suite_is_accepted_now() {
        let req = SetRequest { blocks: vec![SetBlock::IpSuite { qualifier: 1, ip: [0; 4], subnet: [0; 4], gateway: [0; 4] }] };
        assert_eq!(decide_set(&req)[0].error, SetBlockError::Ok);
    }

    #[test]
    fn net_params_helpers() {
        let p = NetParams { name: String::new(), ip: [0; 4], subnet: [0; 4], gateway: [0; 4] };
        assert!(p.is_blank());
        let q = NetParams { name: "x".into(), ip: [172, 16, 2, 10], subnet: [255, 255, 255, 0], gateway: [172, 16, 2, 10] };
        assert!(!q.is_blank());
        assert_eq!(q.prefix_len(), 24);
    }
```

The `BlockQualifier → mode` mapping is pinned in `docs/pdev-golden-frames.md` §1.2 (mode = `BlockQualifier >> 1`, field mask `0xFFFE`); Task 2 confirms which modes TIA sends for "Reset to factory settings" with / without "Retain I&M data".

- [ ] **Step 2: Run** `cargo test -p pnio dcp::set` → compile errors (new variants, `NetParams`, `decide_set` arity, `dcp_set_frame`).

- [ ] **Step 3: Implement** — in `dcp/mod.rs` add `NetParams` (doc every field; `is_blank` = name empty and ip all-zero; `prefix_len` = `subnet.iter().map(|b| b.count_ones() as u8).sum()`), re-export `set::{Effect, SetDecision, QUALIFIER_PERMANENT, reset_mode}`. In `set.rs`:

```rust
/// `BlockQualifier` bit 0: the change persists across restarts.
pub const QUALIFIER_PERMANENT: u16 = 0x0001;

/// The reset mode a ResetToFactory `BlockQualifier` carries: the mode sits above bit 0
/// (`docs/pdev-golden-frames.md` §1.2: mode = `BlockQualifier >> 1`; modes 1-9 are
/// defined, so bits 1-4 are kept). Mode 2 = `0x0004`, mode 4 = `0x0008`, mode 8 = `0x0010`.
pub fn reset_mode(qualifier: u16) -> u8 {
    ((qualifier >> 1) & 0xF) as u8
}
```

`parse_set_request` gains arms:

```rust
            (2, 2) => {
                let name = String::from_utf8(b.value.clone())
                    .map_err(|_| DcpError::Malformed("NameOfStation is not UTF-8"))?;
                out.push(SetBlock::NameOfStation { qualifier, name });
            }
            (5, 3) => {
                if b.value.len() != 2 {
                    return Err(DcpError::Malformed("Signal value must be 2 bytes"));
                }
                out.push(SetBlock::Signal { qualifier, value: u16::from_be_bytes([b.value[0], b.value[1]]) });
            }
            (5, 6) => out.push(SetBlock::ResetToFactory { qualifier }),
```

and `decide_set`:

```rust
/// Decide each block of a Set request. Pure: validation only — the effects are applied
/// (and the IP pushed to the interface, the file written, the AR aborted) by
/// `device::Device`, which also turns an application failure into `SetNotPossible`.
pub fn decide_set(req: &SetRequest) -> Vec<SetDecision> {
    req.blocks
        .iter()
        .map(|b| match b {
            SetBlock::NameOfStation { qualifier, name } => match validate_station_name(name) {
                Ok(()) => SetDecision::ok(2, 2, Effect::SetName { name: name.clone(), permanent: qualifier & QUALIFIER_PERMANENT != 0 }),
                Err(e) => {
                    log::warn!("DCP Set NameOfStation {name:?} refused: {e}");
                    SetDecision::err(2, 2, SetBlockError::SuboptionNotSet)
                }
            },
            SetBlock::IpSuite { qualifier, ip, subnet, gateway } => SetDecision::ok(1, 2, Effect::SetIp { ip: *ip, subnet: *subnet, gateway: *gateway, permanent: qualifier & QUALIFIER_PERMANENT != 0 }),
            SetBlock::Signal { value: 0x0100, .. } => SetDecision::ok(5, 3, Effect::Signal),
            SetBlock::Signal { .. } => SetDecision::err(5, 3, SetBlockError::SuboptionNotSet),
            SetBlock::ResetToFactory { qualifier } => match reset_mode(*qualifier) {
                mode @ (2 | 4) => SetDecision::ok(5, 6, Effect::Reset { mode }),
                _ => SetDecision::err(5, 6, SetBlockError::SuboptionNotSupported),
            },
            SetBlock::Other { option, suboption } => SetDecision::err(*option, *suboption, SetBlockError::SuboptionNotSupported),
        })
        .collect()
}

impl SetDecision {
    fn ok(option: u8, suboption: u8, effect: Effect) -> Self { SetDecision { option, suboption, error: SetBlockError::Ok, effect: Some(effect) } }
    fn err(option: u8, suboption: u8, error: SetBlockError) -> Self { SetDecision { option, suboption, error, effect: None } }
    /// The `(option, suboption, error)` tuple `build_set_response` consumes.
    pub fn tuple(&self) -> (u8, u8, SetBlockError) { (self.option, self.suboption, self.error) }
}
```

Update the module doc (`set.rs:1-5`) and the `SetBlockError::Ok`/`SetNotPossible` doc comments (they describe the old guard). Fix `handle_dcp_frame` in `mod.rs` minimally so the crate compiles: `let results: Vec<_> = decide_set(&req).iter().map(SetDecision::tuple).collect();` — effects are ignored there until Task 12 moves the dispatch into `Device`. In `set.rs::tests`, the old-guard tests go: delete `different_ip_is_refused_not_applied`, `matching_ip_but_other_subnet_is_refused` and `matching_ip_but_other_gateway_is_refused` (`any_ip_suite_is_accepted_now` replaces them; `set_ok_response_is_byte_exact_via_dispatch` still passes since p-net's request is answered `Ok`), and fix the arity in `unsupported_option_gets_suboption_not_supported`: `decide_set(&req).iter().map(SetDecision::tuple).collect::<Vec<_>>()` compared with `vec![(2, 2, SetBlockError::SuboptionNotSupported)]` (its `cfg` helpers stay, used by the remaining dispatch tests). Remove `use crate::dcp::identify::DeviceProperties;` from the top of `set.rs` (no longer used: `-D warnings`). `dcp/mod.rs`'s `dispatch_tests` hold only Identify tests and need no change.

- [ ] **Step 4: Run** `cargo test -p pnio dcp` → PASS; `cargo test -p pnio` → everything else still green (the `device` test `full_bring_up_through_the_loop` uses `dcp_set_req`/`dcp_set_res`: the response bytes are unchanged because p-net's request already matched).

- [ ] **Step 5: Commit** — `feat(dcp): Set-NameOfStation, Signal, ResetToFactory blocks; NetParams; unguarded Set-IP decisions`

---

### Task 5: `dcp::store` (`NetStore`) and `dcp::ipconf` (`IpConfigurator`)

**Files:**
- Create: `crates/pnio/src/dcp/store.rs`, `crates/pnio/src/dcp/ipconf.rs`
- Modify: `crates/pnio/src/dcp/mod.rs` (`pub mod store; pub mod ipconf; pub use store::NetStore; pub use ipconf::{IpConfigurator, MockIp, NoopIp};` and `#[cfg(target_os = "linux")] pub use ipconf::IpCommand;`), `crates/pnio/src/im.rs` (`ImStore::clear`), `crates/pnio/src/testutil.rs` (`im1_record`)

**Interfaces:**
- Produces:

```rust
pub struct NetStore { pub path: Option<PathBuf> }
impl NetStore {
    pub fn new(path: Option<PathBuf>) -> Self;
    pub fn load(&self) -> Option<NetParams>;        // None: no path, missing file, or malformed (warn)
    pub fn save(&self, p: &NetParams);              // no path = no-op; errors logged, never returned
}
pub trait IpConfigurator: Send {
    fn apply(&self, iface: &str, p: &NetParams) -> std::io::Result<()>;
}
pub struct NoopIp;                                  // apply = Ok(())
pub struct MockIp { pub calls: Arc<Mutex<Vec<NetParams>>>, pub fail: bool }
#[cfg(target_os = "linux")] pub struct IpCommand;   // runs `ip`
impl ImStore { pub fn clear(&mut self); }           // all three records back to spaces, persisted if `path`
pub(crate) fn dotted(a: [u8; 4]) -> String;         // dcp::store, reused by dcp::ipconf
// crate::testutil (test-only):
pub fn im1_record(function: &str, location: &str) -> Vec<u8>;   // a well-formed I&M1 record
```

- [ ] **Step 1: Failing tests** (`store.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::dcp::NetParams;

    fn params() -> NetParams {
        NetParams { name: "pnio-dev-b".into(), ip: [172, 16, 2, 10], subnet: [255, 255, 255, 0], gateway: [172, 16, 2, 1] }
    }

    #[test]
    fn round_trips_through_the_file() {
        let dir = std::env::temp_dir().join(format!("pnio-netstore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = NetStore::new(Some(dir.join("net")));
        assert_eq!(store.load(), None);
        store.save(&params());
        assert_eq!(store.load(), Some(params()));
        let text = std::fs::read_to_string(dir.join("net")).unwrap();
        assert_eq!(text, "name=pnio-dev-b\nip=172.16.2.10\nsubnet=255.255.255.0\ngateway=172.16.2.1\n");
        assert!(std::fs::read_dir(&dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with(".net-")), "no temp file left");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn blank_params_round_trip_and_crlf_loads() {
        let dir = std::env::temp_dir().join(format!("pnio-netstore-crlf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("net");
        let store = NetStore::new(Some(path.clone()));
        store.save(&NetParams::default());
        assert_eq!(store.load(), Some(NetParams::default()));
        std::fs::write(&path, "name=pnio-dev\r\nip=10.0.0.2\r\nsubnet=255.0.0.0\r\ngateway=0.0.0.0\r\n").unwrap();
        assert_eq!(store.load().unwrap().name, "pnio-dev");
        assert_eq!(store.load().unwrap().ip, [10, 0, 0, 2]);
        std::fs::write(&path, "name=pnio-dev\nip=not-an-ip\n").unwrap();
        assert_eq!(store.load(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_path_is_a_noop() {
        let store = NetStore::new(None);
        store.save(&params());
        assert_eq!(store.load(), None);
    }
}
```

`ipconf.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::dcp::NetParams;

    #[test]
    fn mock_records_calls_and_can_fail() {
        let ok = MockIp::default();
        ok.apply("eth0", &NetParams::default()).unwrap();
        assert_eq!(ok.calls.lock().unwrap().len(), 1);
        let bad = MockIp { fail: true, ..Default::default() };
        assert!(bad.apply("eth0", &NetParams::default()).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ip_command_lines_are_built_from_params() {
        let p = NetParams { name: String::new(), ip: [172, 16, 2, 10], subnet: [255, 255, 255, 0], gateway: [172, 16, 2, 1] };
        assert_eq!(
            IpCommand::commands("eno2", &p),
            vec![
                vec!["addr", "flush", "dev", "eno2"],
                vec!["addr", "add", "172.16.2.10/24", "dev", "eno2"],
                vec!["route", "replace", "default", "via", "172.16.2.1", "dev", "eno2"],
            ]
        );
        let blank = NetParams::default();
        assert_eq!(IpCommand::commands("eno2", &blank), vec![vec!["addr", "flush", "dev", "eno2"]]);
        let self_gw = NetParams { gateway: [172, 16, 2, 10], ..p.clone() };
        assert_eq!(IpCommand::commands("eno2", &self_gw).len(), 2);
    }
}
```

`testutil.rs` (shared by `im.rs` here and `device/mod.rs` in Task 12 — `im.rs`'s tests have no record builder today; `store_round_trips_records_and_persists` builds its record inline the same way):

```rust
/// A well-formed I&M1 record: `BlockHeader` type `0x0021`, version 1.0, body 54 bytes
/// (TagFunction 32 + TagLocation 22, space-padded).
pub fn im1_record(function: &str, location: &str) -> Vec<u8> {
    let mut rec = Vec::with_capacity(60);
    crate::cm::BlockHeader::write(&mut rec, 0x0021, 54);
    rec.extend_from_slice(format!("{function:<32}{location:<22}").as_bytes());
    rec
}
```

`im.rs` (add to the existing test module, with `use crate::testutil::im1_record;`):

```rust
    #[test]
    fn clear_blanks_everything_and_persists() {
        let dir = std::env::temp_dir().join(format!("pnio-imclear-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = ImStore::load(Some(dir.join("im")));
        s.write(INDEX_IM1, &im1_record("FUNCTION", "LOCATION")).unwrap();
        assert_eq!(s.tag_function().trim(), "FUNCTION");
        s.clear();
        assert_eq!(s.tag_function().trim(), "");
        let reloaded = ImStore::load(Some(dir.join("im")));
        assert_eq!(reloaded.tag_function().trim(), "");
        std::fs::remove_dir_all(&dir).unwrap();
    }
```

- [ ] **Step 2: Run** `cargo test -p pnio -- dcp::store dcp::ipconf im::` → compile errors.

- [ ] **Step 3: Implement `store.rs`**

```rust
//! Persistence of the DCP-assigned identity (`NetParams`): a four-line text file,
//! written atomically (temp file + rename), the same recipe as `ImStore`.

use crate::dcp::NetParams;
use std::path::PathBuf;

/// Backing file for [`NetParams`]; `path = None` means RAM only.
#[derive(Debug, Clone)]
pub struct NetStore {
    /// The file; `None` = nothing is ever read or written.
    pub path: Option<PathBuf>,
}

impl NetStore {
    /// A store on `path` (or RAM only).
    pub fn new(path: Option<PathBuf>) -> Self {
        NetStore { path }
    }

    /// Read the file. `None` when there is no path, no file, or the file is malformed
    /// (logged at warn — the caller falls back to its configured defaults).
    pub fn load(&self) -> Option<NetParams> {
        let path = self.path.as_ref()?;
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                log::warn!("net store {}: {e}", path.display());
                return None;
            }
        };
        match parse(&text) {
            Some(p) => Some(p),
            None => {
                log::warn!("net store {}: malformed, ignoring", path.display());
                None
            }
        }
    }

    /// Write `p` (temp file + rename). Failures are logged, never returned: the
    /// in-memory identity stands regardless.
    pub fn save(&self, p: &NetParams) {
        let Some(path) = &self.path else { return };
        let text = format!(
            "name={}\nip={}\nsubnet={}\ngateway={}\n",
            p.name, dotted(p.ip), dotted(p.subnet), dotted(p.gateway)
        );
        let tmp = path.with_file_name(format!(".net-{}.tmp", std::process::id()));
        let r = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, path));
        if let Err(e) = r {
            log::error!("net store {}: {e}", path.display());
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// `a.b.c.d` text form of an IPv4 address (also used by `dcp::ipconf`).
pub(crate) fn dotted(a: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
}

fn quad(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut n = 0;
    for part in s.split('.') {
        if n == 4 { return None; }
        out[n] = part.parse().ok()?;
        n += 1;
    }
    (n == 4).then_some(out)
}

fn parse(text: &str) -> Option<NetParams> {
    let mut p = NetParams::default();
    let mut seen = [false; 4];
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() { continue; }
        let (k, v) = line.split_once('=')?;
        match k {
            "name" => { p.name = v.to_string(); seen[0] = true; }
            "ip" => { p.ip = quad(v)?; seen[1] = true; }
            "subnet" => { p.subnet = quad(v)?; seen[2] = true; }
            "gateway" => { p.gateway = quad(v)?; seen[3] = true; }
            _ => return None,
        }
    }
    seen.iter().all(|&s| s).then_some(p)
}
```

`ipconf.rs`:

```rust
//! Pushing a DCP-assigned IP suite to the operating system.

use crate::dcp::NetParams;
use std::sync::{Arc, Mutex};

/// Applies an IP suite to a network interface. `Device` calls it before persisting a
/// DCP Set-IP; an `Err` turns the block's answer into `SetNotPossible`.
pub trait IpConfigurator: Send {
    /// Make `iface` carry `p.ip/p.subnet` with `p.gateway` as default route. A blank
    /// suite (`0.0.0.0`) removes every address from the interface.
    fn apply(&self, iface: &str, p: &NetParams) -> std::io::Result<()>;
}

/// Does nothing and succeeds (`StartOptions { apply_ip: false }`).
#[derive(Debug, Default)]
pub struct NoopIp;

impl IpConfigurator for NoopIp {
    fn apply(&self, _iface: &str, _p: &NetParams) -> std::io::Result<()> {
        Ok(())
    }
}

/// Records every call; fails on demand. Test double.
#[derive(Debug, Default)]
pub struct MockIp {
    /// Every `NetParams` passed to `apply`, in order.
    pub calls: Arc<Mutex<Vec<NetParams>>>,
    /// When true, `apply` returns an `Other` I/O error.
    pub fail: bool,
}

impl IpConfigurator for MockIp {
    fn apply(&self, _iface: &str, p: &NetParams) -> std::io::Result<()> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).push(p.clone());
        if self.fail {
            Err(std::io::Error::other("mock failure"))
        } else {
            Ok(())
        }
    }
}

/// Runs the iproute2 `ip` binary: `addr flush`, `addr add <ip>/<prefix>`, and
/// `route replace default via <gw>` when the gateway is neither blank nor the
/// device's own address. Needs `CAP_NET_ADMIN` (already required for `AF_PACKET`).
#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
pub struct IpCommand;

#[cfg(target_os = "linux")]
use crate::dcp::store::dotted;

#[cfg(target_os = "linux")]
impl IpCommand {
    /// The argument vectors `apply` runs, in order (exposed for tests).
    pub fn commands(iface: &str, p: &NetParams) -> Vec<Vec<String>> {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let mut out = vec![s(&["addr", "flush", "dev", iface])];
        if p.ip != [0; 4] {
            let cidr = format!("{}/{}", dotted(p.ip), p.prefix_len());
            out.push(s(&["addr", "add", &cidr, "dev", iface]));
            if p.gateway != [0; 4] && p.gateway != p.ip {
                let gw = dotted(p.gateway);
                out.push(s(&["route", "replace", "default", "via", &gw, "dev", iface]));
            }
        }
        out
    }
}

#[cfg(target_os = "linux")]
impl IpConfigurator for IpCommand {
    fn apply(&self, iface: &str, p: &NetParams) -> std::io::Result<()> {
        for args in Self::commands(iface, p) {
            let st = std::process::Command::new("ip").args(&args).status()?;
            if !st.success() {
                return Err(std::io::Error::other(format!("ip {} exited with {st}", args.join(" "))));
            }
        }
        Ok(())
    }
}
```

`ImStore::clear` in `im.rs`:

```rust
    /// Blank all three records (all spaces) and persist, for DCP ResetToFactory mode 4.
    pub fn clear(&mut self) {
        self.im1 = [b' '; IM1_LEN];
        self.im2 = [b' '; IM2_LEN];
        self.im3 = [b' '; IM3_LEN];
        self.persist();
    }
```

(`persist` is the existing private `fn persist(&self)` at `im.rs:315`, returning `()`.)

- [ ] **Step 4: Run** `cargo test -p pnio -- dcp:: im::` → PASS. `cargo check -p pnio --target x86_64-apple-darwin` → OK (`IpCommand` gated).

- [ ] **Step 5: Commit** — `feat(dcp): NetStore file persistence, IpConfigurator (ip command, mock, noop); ImStore::clear`

---

### Task 6: `eth` — `open_with(EthProto)` and per-transport accept

**Files:**
- Create: `crates/pnio/src/eth/proto.rs` (`EthProto`, all platforms)
- Modify: `crates/pnio/src/eth/afpacket.rs` (`open` → `open_with`, `is_profinet_frame` → `EthProto::accepts`), `crates/pnio/src/eth/header.rs` (`ETHERTYPE_LLDP`, `LLDP_MULTICAST_MAC`), `crates/pnio/src/eth/mod.rs` (`mod proto;`, exports)
- Test: inline in `eth/proto.rs` (the `is_profinet_frame` tests in `afpacket.rs` move there as `accepts` tests)

**Interfaces:**
- Produces:

```rust
// eth/header.rs, re-exported from `eth`:
pub const ETHERTYPE_LLDP: u16 = 0x88CC;
pub const LLDP_MULTICAST_MAC: MacAddr = MacAddr([0x01, 0x80, 0xc2, 0x00, 0x00, 0x0e]);
// eth/proto.rs:
/// What an `AfPacketTransport` is bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EthProto { pub ethertype: u16, pub multicast: Option<[u8; 6]> }
impl EthProto {
    pub const PROFINET: EthProto = EthProto { ethertype: ETHERTYPE_PROFINET, multicast: Some(DCP_MULTICAST_MAC.0) };
    pub const LLDP: EthProto = EthProto { ethertype: ETHERTYPE_LLDP, multicast: Some(LLDP_MULTICAST_MAC.0) };
    /// True for a frame of this EtherType, untagged or behind one 802.1Q tag.
    pub fn accepts(&self, frame: &[u8]) -> bool;
}
impl AfPacketTransport { pub fn open(ifname: &str) -> Result<Self, TransportError>; /* = open_with(ifname, EthProto::PROFINET) */ pub fn open_with(ifname: &str, proto: EthProto) -> Result<Self, TransportError>; }
```

- [ ] **Step 1: Failing test** (in the new `eth/proto.rs`, so `EthProto` and its test compile on every platform):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn frame(et: u16, tagged: bool) -> Vec<u8> {
        let mut f = vec![0u8; 12];
        if tagged {
            f.extend_from_slice(&[0x81, 0x00, 0xc0, 0x00]);
        }
        f.extend_from_slice(&et.to_be_bytes());
        f.extend_from_slice(&[0u8; 46]);
        f
    }

    #[test]
    fn accepts_matches_ethertype_tagged_or_not() {
        assert!(EthProto::PROFINET.accepts(&frame(0x8892, false)));
        assert!(EthProto::PROFINET.accepts(&frame(0x8892, true)));
        assert!(!EthProto::PROFINET.accepts(&frame(0x88CC, false)));
        assert!(EthProto::LLDP.accepts(&frame(0x88CC, false)));
        assert!(!EthProto::LLDP.accepts(&frame(0x8892, true)));
        assert!(!EthProto::LLDP.accepts(&[0u8; 13]));
    }
}
```

- [ ] **Step 2: Run** → compile error.

- [ ] **Step 3: Implement** `eth/proto.rs`:

```rust
//! Which EtherType (and multicast group) an [`AfPacketTransport`] is bound to.

use super::{ETHERTYPE_LLDP, ETHERTYPE_PROFINET, ETHERTYPE_VLAN, LLDP_MULTICAST_MAC};
use crate::dcp::DCP_MULTICAST_MAC;

/// What an `AF_PACKET` socket is bound to and what it accepts at receive time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EthProto {
    /// The EtherType the socket is bound to (`sll_protocol`) and filters on.
    pub ethertype: u16,
    /// A multicast group to join (`PACKET_ADD_MEMBERSHIP`), if any.
    pub multicast: Option<[u8; 6]>,
}

impl EthProto {
    /// PROFINET (`0x8892`) with the DCP multicast group.
    pub const PROFINET: EthProto = EthProto { ethertype: ETHERTYPE_PROFINET, multicast: Some(DCP_MULTICAST_MAC.0) };
    /// LLDP (`0x88CC`) with the nearest-bridge group `01:80:c2:00:00:0e`.
    pub const LLDP: EthProto = EthProto { ethertype: ETHERTYPE_LLDP, multicast: Some(LLDP_MULTICAST_MAC.0) };

    /// True for a frame of this EtherType, untagged or behind one 802.1Q tag.
    pub fn accepts(&self, buf: &[u8]) -> bool {
        if buf.len() < 14 {
            return false;
        }
        let et = u16::from_be_bytes([buf[12], buf[13]]);
        if et == self.ethertype {
            return true;
        }
        et == ETHERTYPE_VLAN && buf.len() >= 18 && u16::from_be_bytes([buf[16], buf[17]]) == self.ethertype
    }
}
```

In `afpacket.rs`: `pub struct AfPacketTransport { fd: OwnedFd, proto: EthProto }`; `open(ifname)` = `Self::open_with(ifname, EthProto::PROFINET)`; `open_with` replaces `ETHERTYPE_PROFINET` by `proto.ethertype` in the `socket`/`bind` calls and wraps the `PACKET_ADD_MEMBERSHIP` block in `if let Some(group) = proto.multicast { ... mreq.mr_address[..6].copy_from_slice(&group); ... }`; `recv_into`'s last check becomes `if self.proto.accepts(&buf[..n])`. Delete `is_profinet_frame` and its tests (moved). Move the doc paragraph about kernel VLAN stripping onto `EthProto::accepts`. Remove `use crate::dcp::frame::DCP_MULTICAST_MAC;` from `afpacket.rs` (the group now comes from `proto.multicast`), and drop `ETHERTYPE_PROFINET`/`ETHERTYPE_VLAN` from its `use super::{..}` once `proto.ethertype` and `accepts` replace their last uses (`-D warnings`). In `eth/header.rs`, next to `ETHERTYPE_PROFINET`/`ETHERTYPE_VLAN`:

```rust
/// EtherType of LLDP (IEEE 802.1AB).
pub const ETHERTYPE_LLDP: u16 = 0x88CC;
/// LLDP nearest-bridge multicast destination `01:80:c2:00:00:0e`.
pub const LLDP_MULTICAST_MAC: MacAddr = MacAddr([0x01, 0x80, 0xc2, 0x00, 0x00, 0x0e]);
```

`eth/mod.rs`: `mod proto;`, `pub use proto::EthProto;`, and the header re-export list becomes `pub use header::{EthError, EthHeader, MacAddr, ETHERTYPE_LLDP, ETHERTYPE_PROFINET, ETHERTYPE_VLAN, LLDP_MULTICAST_MAC};`. These two constants are the only definitions: `lldp` (Task 7) imports them from `crate::eth`.

- [ ] **Step 4: Run** `cargo test -p pnio eth` → PASS; `cargo clippy -p pnio --all-targets -- -D warnings` clean.

- [ ] **Step 5: Commit** — `feat(eth): AfPacketTransport::open_with(EthProto) — any EtherType + optional multicast group; LLDP constants`

---

### Task 7: `lldp` — LLDPDU codec (pure)

**Files:**
- Create: `crates/pnio/src/lldp/mod.rs`
- Modify: `crates/pnio/src/lib.rs` (`pub mod lldp;`)
- Test: inline; a synthetic CPU LLDPDU built from `docs/pdev-golden-frames.md` §3 (the Task 2 golden `testdata/lldp/cpu_x1.hex` gets a twin later)

**Interfaces:**
- Consumes: `eth::{EthHeader, MacAddr, ETHERTYPE_LLDP, LLDP_MULTICAST_MAC}` (Task 6; not redefined here).
- Produces:

```rust
pub const DEFAULT_TTL: u16 = 20;
pub const TX_PERIOD: Duration = Duration::from_secs(5);
pub const OUI_PROFINET: [u8; 3] = [0x00, 0x0e, 0xcf];
pub const OUI_IEEE8023: [u8; 3] = [0x00, 0x12, 0x0f];
#[derive(Debug, Clone, PartialEq, Eq)] pub struct TlvId { pub subtype: u8, pub value: Vec<u8> }
impl TlvId { pub fn local(s: &str) -> Self /* subtype 7 */; pub fn mac(m: [u8;6]) -> Self /* subtype 4 */; pub fn as_text(&self) -> String /* subtype 7 → UTF-8 lossy, subtype 4 → "aa:bb:cc:dd:ee:ff", else hex */ }
#[derive(Debug, Clone, PartialEq, Eq)] pub struct MacPhy { pub autoneg: u8, pub pmd: u16, pub mau_type: u16 }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lldpdu {
    pub chassis_id: TlvId, pub port_id: TlvId, pub ttl: u16,
    pub pnio_delay: Option<[u32; 5]>, pub pnio_port_status: Option<(u16, u16)>, pub pnio_chassis_mac: Option<[u8; 6]>,
    pub mac_phy: Option<MacPhy>, pub unknown_tlvs: usize,
}
impl Lldpdu {
    pub fn encode(&self) -> Vec<u8>;                                   // TLVs only (no Ethernet header)
    pub fn parse(payload: &[u8]) -> Result<Lldpdu, LldpError>;
    pub fn frame(&self, src: MacAddr) -> Vec<u8>;                      // dst 01:80:c2:00:00:0e, EtherType 0x88CC, min 60 bytes padded
}
#[derive(Debug, Error, PartialEq, Eq)] pub enum LldpError { Truncated, MissingMandatory(&'static str) }
/// The device's own PDU.
pub fn build_device_pdu(name: &str, mac: [u8; 6], port_id: &str, ttl: u16) -> Lldpdu;
/// `"port-001"` or `"port-001.<name>"` — the format pinned in docs/pdev-golden-frames.md.
pub fn port_id_string(name: &str) -> String;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer { pub chassis_id: String, pub port_id: String, pub mac: [u8; 6], pub mau_type: Option<u16>, pub seen_at: Instant, pub ttl: Duration }
impl Peer { pub fn from_pdu(pdu: &Lldpdu, src: [u8; 6], now: Instant) -> Peer; pub fn expired(&self, now: Instant) -> bool; }
pub fn parse_frame(frame: &[u8]) -> Result<([u8; 6], Lldpdu), LldpError>;   // src MAC + PDU; checks EtherType
```

- [ ] **Step 1: Failing tests** (bottom of `lldp/mod.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The 1515's X1 MAC (the controller MAC of the DCP and Connect goldens).
    const CPU_MAC: [u8; 6] = [0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f];

    // synthetic — replace by the Task 2 golden (`golden_in("lldp", "cpu_x1")`).
    // Hand-assembled (not through `encode`) from docs/pdev-golden-frames.md §3: Chassis ID
    // and Port ID subtype 7 "Locally assigned" (§3.1, §3.4: `<port>.<NameOfStation>`),
    // TTL 20, PROFINET OUI 00-0E-CF subtype 2 Port Status (RTClass2 0, RTClass3 0) and
    // subtype 5 Chassis MAC (§3.2), IEEE 802.3 OUI 00-12-0F subtype 1 MAC/PHY: autoneg
    // 0x03, PMD 0x6C00, MAU 0x0010 (§3.3), End.
    fn cpu_x1_frame() -> Vec<u8> {
        fn tlv(f: &mut Vec<u8>, ty: u16, body: &[u8]) {
            f.extend_from_slice(&((ty << 9) | body.len() as u16).to_be_bytes());
            f.extend_from_slice(body);
        }
        let mut f = vec![0x01, 0x80, 0xc2, 0x00, 0x00, 0x0e];
        f.extend_from_slice(&CPU_MAC);
        f.extend_from_slice(&[0x88, 0xcc]);
        tlv(&mut f, 1, &[&[7u8][..], b"plcxb1d0ed"].concat());
        tlv(&mut f, 2, &[&[7u8][..], b"port-001.plcxb1d0ed"].concat());
        tlv(&mut f, 3, &20u16.to_be_bytes());
        tlv(&mut f, 127, &[0x00, 0x0e, 0xcf, 2, 0x00, 0x00, 0x00, 0x00]);
        tlv(&mut f, 127, &[&[0x00, 0x0e, 0xcf, 5][..], &CPU_MAC[..]].concat());
        tlv(&mut f, 127, &[0x00, 0x12, 0x0f, 1, 0x03, 0x6c, 0x00, 0x00, 0x10]);
        tlv(&mut f, 0, &[]);
        f
    }

    #[test]
    fn parses_the_cpu_lldpdu() {
        let (src, pdu) = parse_frame(&cpu_x1_frame()).unwrap();
        assert_eq!(src, CPU_MAC);
        assert_eq!(pdu.chassis_id.subtype, 7);
        assert_eq!(pdu.chassis_id.as_text(), "plcxb1d0ed");
        assert!(pdu.port_id.as_text().starts_with("port-001"));
        assert_eq!(pdu.ttl, 20);
        assert_eq!(pdu.pnio_port_status, Some((0, 0)));
        assert_eq!(pdu.pnio_chassis_mac, Some(CPU_MAC));
        assert_eq!(pdu.mac_phy, Some(MacPhy { autoneg: 0x03, pmd: 0x6c00, mau_type: 0x0010 }));
        assert_eq!(pdu.unknown_tlvs, 0);
    }

    #[test]
    fn device_pdu_round_trips_and_has_the_pinned_tlv_order() {
        let pdu = build_device_pdu("pnio-dev", [2, 0, 0, 0, 0, 1], &port_id_string("pnio-dev"), DEFAULT_TTL);
        let bytes = pdu.encode();
        assert_eq!(&bytes[..2], &[0x02, 0x09]); // Chassis ID TLV: type 1, len 9 (subtype 1 byte + "pnio-dev" 8 bytes)
        assert_eq!(bytes[2], 7);
        assert_eq!(Lldpdu::parse(&bytes).unwrap(), pdu);
        assert_eq!(&bytes[bytes.len() - 2..], &[0, 0]); // End TLV
        let frame = pdu.frame(MacAddr([2, 0, 0, 0, 0, 1]));
        assert_eq!(&frame[..6], &[0x01, 0x80, 0xc2, 0x00, 0x00, 0x0e]);
        assert_eq!(&frame[12..14], &[0x88, 0xcc]);
        assert!(frame.len() >= 60);
    }

    #[test]
    fn blank_name_uses_the_mac_as_chassis_id() {
        let pdu = build_device_pdu("", [2, 0, 0, 0, 0, 1], &port_id_string(""), DEFAULT_TTL);
        assert_eq!(pdu.chassis_id, TlvId::mac([2, 0, 0, 0, 0, 1]));
        assert_eq!(pdu.chassis_id.as_text(), "02:00:00:00:00:01");
    }

    #[test]
    fn foreign_switch_pdu_with_unknown_tlvs_parses_into_a_peer() {
        // Chassis MAC, Port local "Gi1/0/3", TTL 120, System Name (type 5), End.
        let mut b = vec![0x02, 0x07, 0x04, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        b.extend_from_slice(&[0x04, 0x08, 0x07]);
        b.extend_from_slice(b"Gi1/0/3");
        b.extend_from_slice(&[0x06, 0x02, 0x00, 0x78]);
        b.extend_from_slice(&[0x0a, 0x03]);
        b.extend_from_slice(b"sw1");
        b.extend_from_slice(&[0, 0]);
        let pdu = Lldpdu::parse(&b).unwrap();
        assert_eq!(pdu.unknown_tlvs, 1);
        assert_eq!(pdu.ttl, 120);
        let now = Instant::now();
        let peer = Peer::from_pdu(&pdu, [1; 6], now);
        assert_eq!(peer.chassis_id, "aa:bb:cc:dd:ee:ff");
        assert_eq!(peer.port_id, "Gi1/0/3");
        assert_eq!(peer.mau_type, None);
        assert!(!peer.expired(now + Duration::from_secs(119)));
        assert!(peer.expired(now + Duration::from_secs(121)));
    }

    #[test]
    fn truncated_and_mandatory_missing_are_errors() {
        assert_eq!(Lldpdu::parse(&[0x02, 0x09, 0x07, b'a']), Err(LldpError::Truncated));
        assert_eq!(Lldpdu::parse(&[0x06, 0x02, 0x00, 0x14, 0, 0]), Err(LldpError::MissingMandatory("chassis id")));
    }

    #[test]
    fn ttl_zero_is_a_shutdown() {
        let mut pdu = build_device_pdu("pnio-dev", [2; 6], "port-001", 0);
        pdu.ttl = 0;
        let peer = Peer::from_pdu(&pdu, [2; 6], Instant::now());
        assert!(peer.expired(Instant::now()));
    }
}
```

- [ ] **Step 2: Run** `cargo test -p pnio lldp` → compile error.

- [ ] **Step 3: Implement.** TLV = 2-byte header: `type (7 bits) << 9 | length (9 bits)`, big-endian, then `length` bytes. Types: 0 End, 1 Chassis ID (`subtype, id`), 2 Port ID, 3 TTL (`u16`), 127 Org-specific (`oui[3], subtype, data`). Encode order per spec §4.2: Chassis, Port, TTL, PNIO delay (subtype 1, 20 bytes), PNIO port status (subtype 2, 4 bytes), PNIO chassis MAC (subtype 5, 6 bytes), IEEE 802.3 MAC/PHY (OUI `00-12-0f`, subtype 1: `autoneg u8, pmd u16, mau u16`), End. **If the Task 2 captures pin a different CPU TLV order or Port ID format, follow the pinned document** and mirror it in `build_device_pdu`/`port_id_string`. Every `pub` item gets a `///` doc comment (`missing_docs` + `-D warnings`); the code below shows the non-obvious ones.

```rust
//! LLDP (IEEE 802.1AB) codec with the PROFINET and IEEE 802.3 organisationally
//! specific TLVs a PROFINET device sends and reads. Pure: no I/O, no clock.

use crate::eth::{EthHeader, MacAddr, ETHERTYPE_LLDP, LLDP_MULTICAST_MAC};
use std::time::{Duration, Instant};
use thiserror::Error;

/// TTL (seconds) the device announces: 4 × [`TX_PERIOD`].
pub const DEFAULT_TTL: u16 = 20;
/// Transmit period of the device's LLDPDU.
pub const TX_PERIOD: Duration = Duration::from_secs(5);
/// PROFIBUS Nutzerorganisation OUI of the PROFINET TLVs (docs/pdev-golden-frames.md §3.2).
pub const OUI_PROFINET: [u8; 3] = [0x00, 0x0e, 0xcf];
/// IEEE 802.3 OUI of the MAC/PHY TLV (§3.3).
pub const OUI_IEEE8023: [u8; 3] = [0x00, 0x12, 0x0f];

/// Why an LLDPDU was not accepted.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LldpError {
    /// A TLV's declared length runs past the end of the frame.
    #[error("truncated TLV")]
    Truncated,
    /// A mandatory TLV (chassis id, port id, ttl) is absent.
    #[error("missing mandatory TLV: {0}")]
    MissingMandatory(&'static str),
}

fn tlv(out: &mut Vec<u8>, ty: u8, body: &[u8]) {
    let hdr = ((ty as u16) << 9) | (body.len() as u16 & 0x1ff);
    out.extend_from_slice(&hdr.to_be_bytes());
    out.extend_from_slice(body);
}

fn org(out: &mut Vec<u8>, oui: [u8; 3], subtype: u8, data: &[u8]) {
    let mut body = Vec::with_capacity(4 + data.len());
    body.extend_from_slice(&oui);
    body.push(subtype);
    body.extend_from_slice(data);
    tlv(out, 127, &body);
}

impl Lldpdu {
    /// The TLVs (no Ethernet header), End TLV included.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(128);
        let id = |t: &TlvId| { let mut b = vec![t.subtype]; b.extend_from_slice(&t.value); b };
        tlv(&mut out, 1, &id(&self.chassis_id));
        tlv(&mut out, 2, &id(&self.port_id));
        tlv(&mut out, 3, &self.ttl.to_be_bytes());
        if let Some(d) = &self.pnio_delay {
            let mut b = Vec::with_capacity(20);
            for v in d { b.extend_from_slice(&v.to_be_bytes()); }
            org(&mut out, OUI_PROFINET, 1, &b);
        }
        if let Some((c2, c3)) = self.pnio_port_status {
            let mut b = c2.to_be_bytes().to_vec();
            b.extend_from_slice(&c3.to_be_bytes());
            org(&mut out, OUI_PROFINET, 2, &b);
        }
        if let Some(m) = self.pnio_chassis_mac {
            org(&mut out, OUI_PROFINET, 5, &m);
        }
        if let Some(p) = &self.mac_phy {
            let mut b = vec![p.autoneg];
            b.extend_from_slice(&p.pmd.to_be_bytes());
            b.extend_from_slice(&p.mau_type.to_be_bytes());
            org(&mut out, OUI_IEEE8023, 1, &b);
        }
        tlv(&mut out, 0, &[]);
        out
    }

    /// Parse the TLVs of an LLDPDU (payload after the Ethernet header); unknown TLVs
    /// are counted, not rejected.
    pub fn parse(payload: &[u8]) -> Result<Lldpdu, LldpError> {
        let (mut chassis, mut port, mut ttl) = (None, None, None);
        let mut pdu = Lldpdu { chassis_id: TlvId { subtype: 0, value: vec![] }, port_id: TlvId { subtype: 0, value: vec![] }, ttl: 0, pnio_delay: None, pnio_port_status: None, pnio_chassis_mac: None, mac_phy: None, unknown_tlvs: 0 };
        let mut p = 0usize;
        while p + 2 <= payload.len() {
            let hdr = u16::from_be_bytes([payload[p], payload[p + 1]]);
            let (ty, len) = ((hdr >> 9) as u8, (hdr & 0x1ff) as usize);
            let body = payload.get(p + 2..p + 2 + len).ok_or(LldpError::Truncated)?;
            p += 2 + len;
            match ty {
                0 => break,
                1 | 2 if !body.is_empty() => {
                    let id = TlvId { subtype: body[0], value: body[1..].to_vec() };
                    if ty == 1 { chassis = Some(id) } else { port = Some(id) }
                }
                3 if body.len() >= 2 => ttl = Some(u16::from_be_bytes([body[0], body[1]])),
                127 if body.len() >= 4 => {
                    let (oui, sub, data) = ([body[0], body[1], body[2]], body[3], &body[4..]);
                    match (oui, sub, data.len()) {
                        (OUI_PROFINET, 1, 20) => {
                            let mut d = [0u32; 5];
                            for (i, v) in d.iter_mut().enumerate() { *v = u32::from_be_bytes([data[4*i], data[4*i+1], data[4*i+2], data[4*i+3]]); }
                            pdu.pnio_delay = Some(d);
                        }
                        (OUI_PROFINET, 2, 4) => pdu.pnio_port_status = Some((u16::from_be_bytes([data[0], data[1]]), u16::from_be_bytes([data[2], data[3]]))),
                        (OUI_PROFINET, 5, 6) => { let mut m = [0u8; 6]; m.copy_from_slice(data); pdu.pnio_chassis_mac = Some(m); }
                        (OUI_IEEE8023, 1, n) if n >= 5 => pdu.mac_phy = Some(MacPhy { autoneg: data[0], pmd: u16::from_be_bytes([data[1], data[2]]), mau_type: u16::from_be_bytes([data[3], data[4]]) }),
                        _ => pdu.unknown_tlvs += 1,
                    }
                }
                _ => pdu.unknown_tlvs += 1,
            }
        }
        pdu.chassis_id = chassis.ok_or(LldpError::MissingMandatory("chassis id"))?;
        pdu.port_id = port.ok_or(LldpError::MissingMandatory("port id"))?;
        pdu.ttl = ttl.ok_or(LldpError::MissingMandatory("ttl"))?;
        Ok(pdu)
    }

    /// The full Ethernet frame (to `LLDP_MULTICAST_MAC`, EtherType `0x88CC`), padded
    /// to the 60-byte minimum.
    pub fn frame(&self, src: MacAddr) -> Vec<u8> {
        let mut f = Vec::with_capacity(128);
        EthHeader { dst: LLDP_MULTICAST_MAC, src, vlan: None, ethertype: ETHERTYPE_LLDP }.write(&mut f);
        f.extend_from_slice(&self.encode());
        if f.len() < 60 { f.resize(60, 0); }
        f
    }
}

/// The device's own PDU: chassis id = NameOfStation (subtype 7), or the MAC (subtype 4)
/// while the name is blank.
pub fn build_device_pdu(name: &str, mac: [u8; 6], port_id: &str, ttl: u16) -> Lldpdu {
    Lldpdu {
        chassis_id: if name.is_empty() { TlvId::mac(mac) } else { TlvId::local(name) },
        port_id: TlvId::local(port_id),
        ttl,
        pnio_delay: Some([0; 5]),
        pnio_port_status: Some((0, 0)),
        pnio_chassis_mac: Some(mac),
        mac_phy: Some(MacPhy { autoneg: 0x03, pmd: 0x6c00, mau_type: 0x0010 }), // pmd: pinned in docs/pdev-golden-frames.md §3.3 (10/100 HD+FD)
        unknown_tlvs: 0,
    }
}

/// The device's LLDP Port ID: `port-001.<name>` (docs/pdev-golden-frames.md §2.7/§3.4:
/// the 1515 sets MultipleInterfaceMode.NameOfDevice = 1), bare `port-001` while unnamed.
pub fn port_id_string(name: &str) -> String {
    // Form provisional (§3.4, "capture only" for the port literal): Task 2 confirms.
    if name.is_empty() { "port-001".to_string() } else { format!("port-001.{name}") }
}
```

`Peer::from_pdu`: `chassis_id = pdu.chassis_id.as_text()`, `port_id = pdu.port_id.as_text()`, `mac = pdu.pnio_chassis_mac.unwrap_or(src)`, `mau_type = pdu.mac_phy.as_ref().map(|m| m.mau_type)`, `ttl = Duration::from_secs(pdu.ttl as u64)`; `expired(now) = now.saturating_duration_since(self.seen_at) >= self.ttl` (so TTL 0 is expired immediately). `parse_frame`: `EthHeader::parse`, require `ethertype == ETHERTYPE_LLDP`, return `(eth.src.0, Lldpdu::parse(payload)?)`. If `port_id_string` is pinned to the bare form, make it return `"port-001"` always and fix the test.

- [ ] **Step 4: Run** `cargo test -p pnio lldp` → 6 PASS. `cargo doc -p pnio --no-deps` → 0 warnings (every pub item documented).

- [ ] **Step 5: Commit** — `feat(lldp): LLDPDU codec with PROFINET and IEEE 802.3 TLVs, device PDU builder, Peer`

---

### Task 8: `pdev` — port state, peer tracking, evaluation, tick

**Files:**
- Create: `crates/pnio/src/pdev/mod.rs`
- Modify: `crates/pnio/src/lib.rs` (`pub mod pdev;`)

**Interfaces:**
- Consumes: `lldp::{Lldpdu, Peer, build_device_pdu, port_id_string, parse_frame, TX_PERIOD, DEFAULT_TTL}`, `dcp::NetParams`.
- Produces:

```rust
pub const PORT_SUBSLOT: u16 = 0x8001;
pub const IFACE_SUBSLOT: u16 = 0x8000;
pub trait LinkProbe: Send { fn link_up(&self) -> bool; }
#[cfg(target_os = "linux")] pub struct SysfsLink { pub iface: String }   // reads /sys/class/net/<iface>/carrier == "1"
pub struct MockLink(pub std::sync::atomic::AtomicBool);
/// One expected peer from CheckPeers.
#[derive(Debug, Clone, PartialEq, Eq)] pub struct ExpectedPeer { pub port_id: String, pub chassis_id: String }
#[derive(Debug, Clone, PartialEq, Eq, Default)] pub struct CheckRecord { pub peers: Vec<ExpectedPeer>, pub mau_type: Option<u16>, pub raw: Vec<u8> }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Mismatch { NoPeer, Chassis, Port }
impl Mismatch { pub fn ext_error_type(self) -> u16 /* under ChannelErrorType 0x8001 "Remote mismatch" (docs/pdev-golden-frames.md §2.10): Chassis 0x8000 "Peer name of station mismatch", Port 0x8001 "Peer name of port mismatch", NoPeer 0x8005 "No peer detected" */ }
#[derive(Debug, Clone, PartialEq, Eq)] pub enum PdevAction { Send(Vec<u8>) /* full frame */, Diag(Option<Mismatch>) }
pub struct Pdev { /* private */ }
impl Pdev {
    pub fn new(mac: [u8; 6], link: Box<dyn LinkProbe>, now: Instant) -> Pdev;
    pub fn set_identity(&mut self, net: &NetParams, now: Instant);   // name/ip changed → next_tx = now (immediate LLDPDU)
    pub fn on_rx(&mut self, frame: &[u8], now: Instant) -> Vec<PdevAction>;
    pub fn tick(&mut self, now: Instant) -> Vec<PdevAction>;
    pub fn next_deadline(&self) -> Option<Instant>;
    pub fn shutdown_frame(&self) -> Vec<u8>;                           // TTL 0; only Chassis ID, Port ID, TTL, End (docs/pdev-golden-frames.md §3.1)
    pub fn set_check(&mut self, c: Option<CheckRecord>, now: Instant) -> Vec<PdevAction>;
    pub fn set_adjust(&mut self, raw: Option<Vec<u8>>);
    pub fn set_iface_adjust(&mut self, raw: Option<Vec<u8>>);
    pub fn clear_engineering(&mut self) -> Vec<PdevAction>;
    pub fn peer(&self) -> Option<&Peer>;
    pub fn check(&self) -> Option<&CheckRecord>;
    pub fn adjust(&self) -> Option<&[u8]>;
    pub fn iface_adjust(&self) -> Option<&[u8]>;
    pub fn link_up(&self) -> bool;
    pub fn mismatch(&self) -> Option<Mismatch>;
    pub fn own_port_id(&self) -> &str;
    pub fn mac(&self) -> [u8; 6];
    pub fn net(&self) -> &NetParams;
}
```

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::dcp::NetParams;
    use crate::lldp::{build_device_pdu, parse_frame};
    use std::time::Duration;

    const MAC: [u8; 6] = [2, 0, 0, 0, 0, 1];

    fn net() -> NetParams {
        NetParams { name: "pnio-dev".into(), ip: [172, 16, 2, 10], subnet: [255, 255, 255, 0], gateway: [172, 16, 2, 10] }
    }

    fn pdev(now: Instant) -> Pdev {
        let mut p = Pdev::new(MAC, Box::new(MockLink(true.into())), now);
        p.set_identity(&net(), now);
        p
    }

    fn cpu_frame(chassis: &str, port: &str, ttl: u16) -> Vec<u8> {
        let mut pdu = build_device_pdu(chassis, [0xec, 0, 0, 0, 0, 1], port, ttl);
        pdu.ttl = ttl;
        pdu.frame(crate::eth::MacAddr([0xec, 0, 0, 0, 0, 1]))
    }

    fn check(chassis: &str, port: &str) -> CheckRecord {
        CheckRecord { peers: vec![ExpectedPeer { port_id: port.into(), chassis_id: chassis.into() }], mau_type: None, raw: vec![] }
    }

    fn sends(a: &[PdevAction]) -> usize { a.iter().filter(|x| matches!(x, PdevAction::Send(_))).count() }
    fn diag(a: &[PdevAction]) -> Option<Option<Mismatch>> { a.iter().find_map(|x| if let PdevAction::Diag(m) = x { Some(*m) } else { None }) }

    #[test]
    fn sends_immediately_then_every_period() {
        let t0 = Instant::now();
        let mut p = pdev(t0);
        let a = p.tick(t0);
        assert_eq!(sends(&a), 1);
        let (src, pdu) = parse_frame(match &a[0] { PdevAction::Send(f) => f, _ => panic!() }).unwrap();
        assert_eq!(src, MAC);
        assert_eq!(pdu.chassis_id.as_text(), "pnio-dev");
        assert_eq!(pdu.ttl, 20);
        assert_eq!(p.next_deadline(), Some(t0 + Duration::from_secs(5)));
        assert_eq!(sends(&p.tick(t0 + Duration::from_secs(4))), 0);
        assert_eq!(sends(&p.tick(t0 + Duration::from_secs(5))), 1);
    }

    #[test]
    fn identity_change_sends_at_once_and_blank_name_uses_mac() {
        let t0 = Instant::now();
        let mut p = pdev(t0);
        p.tick(t0);
        p.set_identity(&NetParams::default(), t0 + Duration::from_secs(1));
        let a = p.tick(t0 + Duration::from_secs(1));
        assert_eq!(sends(&a), 1);
        let (_, pdu) = parse_frame(match &a[0] { PdevAction::Send(f) => f, _ => panic!() }).unwrap();
        assert_eq!(pdu.chassis_id.subtype, 4);
    }

    #[test]
    fn peer_is_learned_served_and_expires() {
        let t0 = Instant::now();
        let mut p = pdev(t0);
        p.on_rx(&cpu_frame("plc", "port-001.plc", 20), t0);
        assert_eq!(p.peer().unwrap().chassis_id, "plc");
        assert!(p.next_deadline().unwrap() <= t0 + Duration::from_secs(5));
        p.tick(t0 + Duration::from_secs(19));
        assert!(p.peer().is_some());
        p.tick(t0 + Duration::from_secs(20));
        assert!(p.peer().is_none());
        p.on_rx(&cpu_frame("plc", "port-001.plc", 20), t0 + Duration::from_secs(21));
        p.on_rx(&cpu_frame("plc", "port-001.plc", 0), t0 + Duration::from_secs(22)); // shutdown PDU
        assert!(p.peer().is_none());
    }

    #[test]
    fn evaluation_matrix() {
        let t0 = Instant::now();
        let mut p = pdev(t0);
        // No check → never a diag, whatever the peer does.
        assert_eq!(diag(&p.on_rx(&cpu_frame("plc", "port-001.plc", 20), t0)), None);
        // Check matching the peer → explicit "no mismatch" is not emitted (nothing changed).
        assert_eq!(diag(&p.set_check(Some(check("plc", "port-001.plc")), t0)), None);
        assert_eq!(p.mismatch(), None);
        // Port differs → Port.
        assert_eq!(diag(&p.set_check(Some(check("plc", "port-002.plc")), t0)), Some(Some(Mismatch::Port)));
        // Chassis differs → Chassis (takes precedence over port).
        assert_eq!(diag(&p.set_check(Some(check("other", "port-002.plc")), t0)), Some(Some(Mismatch::Chassis)));
        // Peer changes to match → cleared.
        assert_eq!(diag(&p.on_rx(&cpu_frame("other", "port-002.plc", 20), t0)), Some(None));
        // Peer expires → NoPeer (we had seen one).
        assert_eq!(diag(&p.tick(t0 + Duration::from_secs(20))), Some(Some(Mismatch::NoPeer)));
        // Clearing engineering clears the diag.
        assert_eq!(diag(&p.clear_engineering()), Some(None));
        assert_eq!(p.check(), None);
    }

    #[test]
    fn no_peer_waits_two_periods_after_the_check_when_none_was_ever_seen() {
        let t0 = Instant::now();
        let mut p = pdev(t0);
        assert_eq!(diag(&p.set_check(Some(check("plc", "port-001.plc")), t0)), None);
        assert_eq!(diag(&p.tick(t0 + Duration::from_secs(9))), None);
        assert_eq!(diag(&p.tick(t0 + Duration::from_secs(10))), Some(Some(Mismatch::NoPeer)));
        // The grace deadline (check written at t0, + 2 × TX_PERIOD) is visible to the loop:
        // move the transmit schedule past it without letting the grace expire (sends at
        // t0 and t0+7 s → next_tx = t0+12 s > t0+10 s); the grace term is then the minimum.
        let mut q = pdev(t0);
        q.tick(t0);
        q.set_check(Some(check("plc", "port-001.plc")), t0);
        assert_eq!(sends(&q.tick(t0 + Duration::from_secs(7))), 1);
        assert_eq!(q.next_deadline(), Some(t0 + Duration::from_secs(10)));
    }

    #[test]
    fn two_expected_peers_match_if_any_matches() {
        let t0 = Instant::now();
        let mut p = pdev(t0);
        p.on_rx(&cpu_frame("plc", "port-002.plc", 20), t0);
        let c = CheckRecord { peers: vec![ExpectedPeer { port_id: "port-001.plc".into(), chassis_id: "plc".into() }, ExpectedPeer { port_id: "port-002.plc".into(), chassis_id: "plc".into() }], mau_type: None, raw: vec![] };
        assert_eq!(diag(&p.set_check(Some(c), t0)), None);
        assert_eq!(p.mismatch(), None);
        let empty = CheckRecord::default();
        assert_eq!(diag(&p.set_check(Some(empty), t0)), None);
    }

    #[test]
    fn shutdown_frame_has_ttl_zero_and_own_port_id_is_pinned_format() {
        let p = pdev(Instant::now());
        let (_, pdu) = parse_frame(&p.shutdown_frame()).unwrap();
        assert_eq!(pdu.ttl, 0);
        // Shutdown PDU = Chassis ID, Port ID, TTL, End only (docs/pdev-golden-frames.md §3.1).
        assert_eq!((pdu.pnio_delay, pdu.pnio_port_status, pdu.pnio_chassis_mac, pdu.mac_phy.clone()), (None, None, None, None));
        assert_eq!(p.own_port_id(), crate::lldp::port_id_string("pnio-dev"));
        assert_eq!((p.mac(), p.net().name.as_str()), (MAC, "pnio-dev"));
    }
}
```

- [ ] **Step 2: Run** `cargo test -p pnio pdev` → compile error.

- [ ] **Step 3: Implement**

```rust
//! PDEV: the physical-device view of our one port and interface — LLDP neighbour
//! tracking, the engineering records TIA writes (`PDPortDataCheck`/`Adjust`,
//! `PDInterfaceAdjust`), and the expected-vs-actual peer evaluation that drives the
//! *remote mismatch* diagnosis. Pure: `tick`/`on_rx` return actions, the device loop
//! performs them.

use crate::dcp::NetParams;
use crate::eth::MacAddr;
use crate::lldp::{build_device_pdu, parse_frame, port_id_string, Peer, DEFAULT_TTL, TX_PERIOD};
use std::time::Instant;

/// Our one port and interface: LLDP schedule, neighbour, engineering records, mismatch.
pub struct Pdev {
    mac: [u8; 6],
    net: NetParams,
    own_port_id: String,
    link: Box<dyn LinkProbe>,
    peer: Option<Peer>,
    ever_seen_peer: bool,
    check: Option<CheckRecord>,
    check_written_at: Option<Instant>,
    adjust: Option<Vec<u8>>,
    iface_adjust: Option<Vec<u8>>,
    next_tx: Instant,
    mismatch: Option<Mismatch>,
}

impl Pdev {
    /// A port with no identity yet (blank name), first LLDPDU due at `now`.
    pub fn new(mac: [u8; 6], link: Box<dyn LinkProbe>, now: Instant) -> Pdev {
        Pdev { mac, net: NetParams::default(), own_port_id: port_id_string(""), link, peer: None, ever_seen_peer: false, check: None, check_written_at: None, adjust: None, iface_adjust: None, next_tx: now, mismatch: None }
    }

    /// Adopt a new name/IP suite; the next LLDPDU goes out at `now`.
    pub fn set_identity(&mut self, net: &NetParams, now: Instant) {
        self.net = net.clone();
        self.own_port_id = port_id_string(&net.name);
        self.next_tx = now;
    }

    /// The TTL-0 LLDPDU announcing our departure: Chassis ID, Port ID, TTL = 0 and End
    /// only (docs/pdev-golden-frames.md §3.1 — any other TLV is flagged in a shutdown PDU).
    pub fn shutdown_frame(&self) -> Vec<u8> {
        let mut pdu = build_device_pdu(&self.net.name, self.mac, &self.own_port_id, 0);
        pdu.pnio_delay = None;
        pdu.pnio_port_status = None;
        pdu.pnio_chassis_mac = None;
        pdu.mac_phy = None;
        pdu.frame(MacAddr(self.mac))
    }

    /// Feed one received LLDP frame: learn (or forget, on TTL 0) the neighbour and
    /// re-evaluate the expected-peer check.
    pub fn on_rx(&mut self, frame: &[u8], now: Instant) -> Vec<PdevAction> {
        match parse_frame(frame) {
            Ok((src, pdu)) => {
                let peer = Peer::from_pdu(&pdu, src, now);
                if peer.ttl.is_zero() {
                    log::info!("LLDP shutdown from {}", peer.chassis_id);
                    self.peer = None;
                } else {
                    self.ever_seen_peer = true;
                    self.peer = Some(peer);
                }
                self.reevaluate(now)
            }
            Err(e) => {
                log::debug!("dropping LLDP frame: {e}");
                Vec::new()
            }
        }
    }

    /// Timers: expire the neighbour, send the periodic LLDPDU, re-evaluate.
    pub fn tick(&mut self, now: Instant) -> Vec<PdevAction> {
        let mut out = Vec::new();
        if let Some(p) = &self.peer {
            if p.expired(now) {
                log::info!("LLDP peer {} expired", p.chassis_id);
                self.peer = None;
            }
        }
        if now >= self.next_tx {
            let pdu = build_device_pdu(&self.net.name, self.mac, &self.own_port_id, DEFAULT_TTL);
            out.push(PdevAction::Send(pdu.frame(MacAddr(self.mac))));
            self.next_tx = now + TX_PERIOD;
        }
        out.extend(self.reevaluate(now));
        out
    }

    /// The earliest instant `tick` has something to do: next transmit, neighbour
    /// expiry, or the end of the no-peer grace after a Check write.
    pub fn next_deadline(&self) -> Option<Instant> {
        let mut d = Some(self.next_tx);
        if let Some(p) = &self.peer { d = Some(d.unwrap().min(p.seen_at + p.ttl)); }
        if let (Some(t), None, false) = (self.check_written_at, &self.peer, self.ever_seen_peer) {
            if self.check.as_ref().is_some_and(|c| !c.peers.is_empty()) { d = Some(d.unwrap().min(t + 2 * TX_PERIOD)); }
        }
        d
    }

    fn evaluate(&self, now: Instant) -> Option<Mismatch> {
        let check = self.check.as_ref()?;
        if check.peers.is_empty() { return None; }
        match &self.peer {
            Some(p) => {
                if check.peers.iter().any(|e| e.chassis_id == p.chassis_id && e.port_id == p.port_id) { None }
                else if check.peers.iter().any(|e| e.chassis_id == p.chassis_id) { Some(Mismatch::Port) }
                else { Some(Mismatch::Chassis) }
            }
            None => {
                let grace_over = self.check_written_at.is_some_and(|t| now.saturating_duration_since(t) >= 2 * TX_PERIOD);
                (self.ever_seen_peer || grace_over).then_some(Mismatch::NoPeer)
            }
        }
    }

    fn reevaluate(&mut self, now: Instant) -> Vec<PdevAction> {
        let m = self.evaluate(now);
        if m != self.mismatch {
            self.mismatch = m;
            vec![PdevAction::Diag(m)]
        } else {
            Vec::new()
        }
    }

    /// Store (or drop) the expected-peer record TIA wrote; returns the diag change.
    pub fn set_check(&mut self, c: Option<CheckRecord>, now: Instant) -> Vec<PdevAction> {
        self.check = c;
        self.check_written_at = self.check.as_ref().map(|_| now);
        self.reevaluate(now)
    }
    /// Store (or drop) the raw `PDPortDataAdjust` record.
    pub fn set_adjust(&mut self, raw: Option<Vec<u8>>) { self.adjust = raw; }
    /// Store (or drop) the raw `PDInterfaceAdjust` record.
    pub fn set_iface_adjust(&mut self, raw: Option<Vec<u8>>) { self.iface_adjust = raw; }
    /// Forget every engineering record (AR loss, ResetToFactory); clears the diag.
    pub fn clear_engineering(&mut self) -> Vec<PdevAction> {
        self.adjust = None;
        self.iface_adjust = None;
        self.check = None;
        self.check_written_at = None;
        let was = self.mismatch.take();
        if was.is_some() { vec![PdevAction::Diag(None)] } else { Vec::new() }
    }
    /// The current LLDP neighbour.
    pub fn peer(&self) -> Option<&Peer> { self.peer.as_ref() }
    /// The stored `PDPortDataCheck`.
    pub fn check(&self) -> Option<&CheckRecord> { self.check.as_ref() }
    /// The stored `PDPortDataAdjust` record.
    pub fn adjust(&self) -> Option<&[u8]> { self.adjust.as_deref() }
    /// The stored `PDInterfaceAdjust` record.
    pub fn iface_adjust(&self) -> Option<&[u8]> { self.iface_adjust.as_deref() }
    /// Link state from the [`LinkProbe`].
    pub fn link_up(&self) -> bool { self.link.link_up() }
    /// The current remote mismatch, if any.
    pub fn mismatch(&self) -> Option<Mismatch> { self.mismatch }
    /// Our LLDP Port ID.
    pub fn own_port_id(&self) -> &str { &self.own_port_id }
    /// The interface MAC (chassis MAC TLV, PDInterfaceDataReal).
    pub fn mac(&self) -> [u8; 6] { self.mac }
    /// The identity last given to [`Pdev::set_identity`].
    pub fn net(&self) -> &NetParams { &self.net }
}
```

Every `pub` item gets a `///` doc comment (`missing_docs` + `-D warnings`) — the types of the Interfaces block (`PORT_SUBSLOT`, `LinkProbe`, `SysfsLink`, `MockLink`, `ExpectedPeer`, `CheckRecord`, `Mismatch`, `PdevAction`) included. `Mismatch::ext_error_type`: the values pinned in `docs/pdev-golden-frames.md` §2.10 — Chassis `0x8000`, Port `0x8001`, NoPeer `0x8005`, all under ChannelErrorType `0x8001` "Remote mismatch" (`diag::PORT_ERROR_TYPE`, Task 11). `SysfsLink::link_up` = `std::fs::read_to_string(format!("/sys/class/net/{}/carrier", self.iface)).map(|s| s.trim() == "1").unwrap_or(true)` (unknown = assume up, so a missing sysfs never reports a false link-down). `MockLink(AtomicBool)`: `link_up` loads it. No `pub mod records;` yet: Task 9 adds it together with `pdev/records.rs`.

- [ ] **Step 4: Run** `cargo test -p pnio pdev` → 7 PASS; clippy clean.

- [ ] **Step 5: Commit** — `feat(pdev): port state, LLDP peer tracking with TTL, expected-peer evaluation, transmit schedule`

---

### Task 9: `pdev::records` — PDEV record builders (read) and Check/Adjust parsing (write)

**Files:**
- Create: `crates/pnio/src/pdev/records.rs`
- Modify: `crates/pnio/src/pdev/mod.rs` (`pub mod records;`), `crates/pnio/src/testutil.rs` (synthetic PDEV request builders)
- Test: inline; synthetic records/requests built from `docs/pdev-golden-frames.md` §2 (the Task 2 goldens `testdata/pdev/connect_write_check.hex`, `read_*_req.hex` get twins later)

**Interfaces:**
- Consumes: `crate::cm::{BlockHeader, Cursor, BlockError}` (all three already `pub` and re-exported from `cm`; `BlockError` is defined in `cm/mod.rs`, so the `cm::block::` path would not resolve it), `Pdev` getters incl. `mac()`/`net()` (Task 8), `NetParams`.
- Produces:

```rust
pub mod index { pub const PD_PORT_DATA_REAL: u16 = 0x802A; pub const PD_PORT_DATA_CHECK: u16 = 0x802B; pub const PD_PORT_DATA_ADJUST: u16 = 0x802F; pub const PD_INTERFACE_DATA_REAL: u16 = 0x8080; pub const PD_INTERFACE_ADJUST: u16 = 0x8071; pub const PD_REAL_DATA: u16 = 0xF841; pub const PD_EXPECTED_DATA: u16 = 0xF842; }
/// BlockTypes, docs/pdev-golden-frames.md §2.2 (all confirmed by the dissector's table).
pub mod ty {
    pub const PD_PORT_DATA_CHECK: u16 = 0x0200;
    pub const PD_PORT_DATA_ADJUST: u16 = 0x0202;
    pub const CHECK_PEERS: u16 = 0x020A;
    pub const CHECK_MAU_TYPE: u16 = 0x020C;
    pub const PD_PORT_DATA_REAL: u16 = 0x020F;
    pub const PD_INTERFACE_DATA_REAL: u16 = 0x0240;
    pub const PD_INTERFACE_ADJUST: u16 = 0x0250;
    pub const MULTIPLE_BLOCK_HEADER: u16 = 0x0400;
}
pub fn parse_check(record: &[u8]) -> Result<CheckRecord, BlockError>;      // whole record data (PDPortDataCheck block incl. header)
pub fn port_data_real(p: &Pdev, slot: u16, subslot: u16) -> Vec<u8>;
pub fn interface_data_real(p: &Pdev) -> Vec<u8>;                          // the block has no slot/subslot fields (§2.4)
pub fn pd_real_data(p: &Pdev) -> Vec<u8>;       // MultipleBlockHeader(api 0, slot 0, 0x8000){iface real} + MultipleBlockHeader(0, 0, 0x8001){port real}
pub fn pd_expected_data(p: &Pdev) -> Vec<u8>;   // MultipleBlockHeader(0, 0, 0x8001){check raw + adjust raw} + MultipleBlockHeader(0, 0, 0x8000){iface adjust raw}; each only if something is stored; empty when nothing written
pub fn read(p: &Pdev, idx: u16, slot: u16, subslot: u16) -> Option<Vec<u8>>;   // dispatch; None = not ours
// crate::testutil (test-only), shared with Task 10:
pub fn synthetic_check_record(port_id: &str, chassis_id: &str) -> Vec<u8>;
pub fn synthetic_write_record(ar_uuid: Uuid, slot: u16, subslot: u16, index: u16, data: &[u8]) -> Vec<u8>;
pub fn synthetic_read_req(ar_uuid: Uuid, slot: u16, subslot: u16, index: u16) -> Vec<u8>;
```

Every PDEV block is BlockVersion **1.0** (docs/pdev-golden-frames.md §2.1/§2.3: the dissector decodes only 1.0 for these blocks; there is no 1.2 PDPortDataReal), so `BlockHeader::write`/`parse` (1.0 only) are used unchanged — no codec change.

- [ ] **Step 1: Failing tests**

First the synthetic builders, in `crates/pnio/src/testutil.rs` (Task 10 reuses them):

```rust
// synthetic — replace by the Task 2 golden. PDEV record and request builders from
// docs/pdev-golden-frames.md §2 and the IODWriteReqHeader / IODReadReqHeader layouts
// that `cm::WriteReq::parse` / `cm::ReadReq::parse` read.

/// A `PDPortDataCheck` record (index `0x802B`, slot 0, subslot `0x8001`) with one expected
/// peer (§2.5): header, padding 2, SlotNumber, SubslotNumber, a `CheckPeers` sub-block
/// (NumberOfPeers, LengthPeerPortID + PeerPortID, LengthPeerChassisID + PeerChassisID, no
/// padding) and a `Checking MAUType` sub-block (`0x0010`).
pub fn synthetic_check_record(port_id: &str, chassis_id: &str) -> Vec<u8> {
    use crate::cm::BlockHeader;
    use crate::pdev::records::ty;
    let mut peers = vec![1u8, port_id.len() as u8];
    peers.extend_from_slice(port_id.as_bytes());
    peers.push(chassis_id.len() as u8);
    peers.extend_from_slice(chassis_id.as_bytes());
    let mut body = vec![0, 0]; // padding
    body.extend_from_slice(&0u16.to_be_bytes()); // SlotNumber
    body.extend_from_slice(&0x8001u16.to_be_bytes()); // SubslotNumber
    BlockHeader::write(&mut body, ty::CHECK_PEERS, peers.len() as u16);
    body.extend_from_slice(&peers);
    BlockHeader::write(&mut body, ty::CHECK_MAU_TYPE, 2);
    body.extend_from_slice(&0x0010u16.to_be_bytes());
    let mut out = Vec::new();
    BlockHeader::write(&mut out, ty::PD_PORT_DATA_CHECK, body.len() as u16);
    out.extend_from_slice(&body);
    out
}

/// One `IODWriteReqHeader` carrying `data` (a single-record Write, no MultipleWrite
/// container): seq, ARUUID, API 0, slot, subslot, padding, index, RecordDataLength,
/// 24 bytes padding, then the data.
pub fn synthetic_write_record(
    ar_uuid: crate::rpc::Uuid,
    slot: u16,
    subslot: u16,
    index: u16,
    data: &[u8],
) -> Vec<u8> {
    use crate::cm::{ty, BlockHeader};
    let mut b = Vec::with_capacity(64 + data.len());
    BlockHeader::write(&mut b, ty::IOD_WRITE_REQ_HEADER, 58);
    b.extend_from_slice(&1u16.to_be_bytes());
    ar_uuid.write(&mut b, crate::rpc::Drep::BIG);
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&slot.to_be_bytes());
    b.extend_from_slice(&subslot.to_be_bytes());
    b.extend_from_slice(&[0, 0]);
    b.extend_from_slice(&index.to_be_bytes());
    b.extend_from_slice(&(data.len() as u32).to_be_bytes());
    b.extend_from_slice(&[0u8; 24]);
    b.extend_from_slice(data);
    b
}

/// One `IODReadReqHeader`: seq, ARUUID, API 0, slot, subslot, padding, index,
/// RecordDataLength `0x8000`, TargetARUUID (= `ar_uuid`), 8 bytes padding.
pub fn synthetic_read_req(ar_uuid: crate::rpc::Uuid, slot: u16, subslot: u16, index: u16) -> Vec<u8> {
    use crate::cm::{ty, BlockHeader};
    let mut b = Vec::with_capacity(64);
    BlockHeader::write(&mut b, ty::IOD_READ_REQ_HEADER, 58);
    b.extend_from_slice(&2u16.to_be_bytes());
    ar_uuid.write(&mut b, crate::rpc::Drep::BIG);
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&slot.to_be_bytes());
    b.extend_from_slice(&subslot.to_be_bytes());
    b.extend_from_slice(&[0, 0]);
    b.extend_from_slice(&index.to_be_bytes());
    b.extend_from_slice(&0x8000u32.to_be_bytes());
    ar_uuid.write(&mut b, crate::rpc::Drep::BIG);
    b.extend_from_slice(&[0u8; 8]);
    b
}
```

Then `pdev/records.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cm::{BlockHeader, ReadReq, WriteReq};
    use crate::dcp::NetParams;
    use crate::pdev::{MockLink, Pdev, IFACE_SUBSLOT, PORT_SUBSLOT};
    use crate::rpc::Uuid;
    use crate::testutil::{synthetic_check_record, synthetic_read_req, synthetic_write_record};
    use std::time::Instant;

    fn pdev() -> Pdev {
        let mut p = Pdev::new([2, 0, 0, 0, 0, 1], Box::new(MockLink(true.into())), Instant::now());
        p.set_identity(&NetParams { name: "pnio-dev".into(), ip: [172, 16, 2, 10], subnet: [255, 255, 255, 0], gateway: [172, 16, 2, 10] }, Instant::now());
        p
    }

    /// `(subslot, nested BlockTypes)` of a MultipleBlockHeader body: padding 2, API 4,
    /// slot 2, subslot 2, then the nested blocks (docs/pdev-golden-frames.md §2.8).
    fn mbh(body: &[u8]) -> (u16, Vec<u16>) {
        let subslot = u16::from_be_bytes([body[8], body[9]]);
        let nested = BlockHeader::read_all(&body[10..]).unwrap().iter().map(|(h, _)| h.block_type).collect();
        (subslot, nested)
    }

    #[test]
    fn parses_a_check_peers_record() {
        // synthetic — replace by the Task 2 golden (`golden_in("pdev", "connect_write_check")`).
        let check = synthetic_check_record("port-001.plcxb1d0ed", "plcxb1d0ed");
        let w = WriteReq::parse(&synthetic_write_record(Uuid([0; 16]), 0, PORT_SUBSLOT, index::PD_PORT_DATA_CHECK, &check)).unwrap();
        let rec = w.records.iter().find(|r| r.index == index::PD_PORT_DATA_CHECK).unwrap();
        assert_eq!((rec.slot, rec.subslot), (0, PORT_SUBSLOT));
        let c = parse_check(&rec.data).unwrap();
        assert_eq!(c.peers.len(), 1);
        assert_eq!(c.peers[0].chassis_id, "plcxb1d0ed");
        assert!(c.peers[0].port_id.starts_with("port-001"));
        assert_eq!(c.mau_type, Some(0x0010));
        assert_eq!(c.raw, rec.data);
    }

    #[test]
    fn check_sub_block_list_stops_at_a_zero_block_type() {
        // docs/pdev-golden-frames.md §2.1: a block list ends early at a 0x0000 BlockType.
        let mut rec = synthetic_check_record("port-001.plc", "plc");
        rec.extend_from_slice(&[0, 0, 0, 0]);
        let len = u16::from_be_bytes([rec[2], rec[3]]) + 4;
        rec[2..4].copy_from_slice(&len.to_be_bytes());
        assert_eq!(parse_check(&rec).unwrap().peers.len(), 1);
    }

    #[test]
    fn check_with_garbage_is_an_error() {
        assert!(parse_check(&[0x02, 0x00, 0x00, 0x03, 0x01, 0x00, 0x00]).is_err());
    }

    #[test]
    fn port_data_real_without_peer_then_with_peer() {
        let mut p = pdev();
        let b = port_data_real(&p, 0, PORT_SUBSLOT);
        let (h, body) = BlockHeader::parse(&b).unwrap();
        assert_eq!(h.block_type, ty::PD_PORT_DATA_REAL);
        assert_eq!(h.block_length as usize + 4, b.len()); // BlockLength counts the version bytes, not type/length
        assert_eq!(b.len() % 4, 0);
        // Padding 2, SlotNumber, SubslotNumber, LengthOwnPortID, OwnPortID, NumberOfPeers = 0 (§2.3)
        assert_eq!(&body[0..2], &[0, 0]);
        assert_eq!(&body[2..6], &[0, 0, 0x80, 0x01]);
        let own = p.own_port_id().as_bytes();
        assert_eq!(body[6] as usize, own.len());
        assert_eq!(&body[7..7 + own.len()], own);
        assert_eq!(body[7 + own.len()], 0);
        let frame = crate::lldp::build_device_pdu("plc", [0xec; 6], "port-001.plc", 20).frame(crate::eth::MacAddr([0xec; 6]));
        p.on_rx(&frame, Instant::now());
        let b2 = port_data_real(&p, 0, PORT_SUBSLOT);
        assert!(b2.len() > b.len());
        assert_eq!(b2.len() % 4, 0);
        assert_eq!(b2[BlockHeader::LEN + 7 + own.len()], 1, "NumberOfPeers");
        let s = String::from_utf8_lossy(&b2);
        assert!(s.contains("port-001.plc") && s.contains("plc"));
        assert!(b2.windows(6).any(|w| w == [0xec; 6]), "peer MAC present");
    }

    #[test]
    fn interface_data_real_carries_chassis_mac_and_ip() {
        let p = pdev();
        let b = interface_data_real(&p);
        let (h, body) = BlockHeader::parse(&b).unwrap();
        assert_eq!(h.block_type, ty::PD_INTERFACE_DATA_REAL);
        assert_eq!(b.len() % 4, 0);
        // LengthOwnStationName right after the header, no padding before it (§2.4).
        assert_eq!(body[0] as usize, "pnio-dev".len());
        assert_eq!(&body[1..9], b"pnio-dev");
        assert!(body.windows(6).any(|w| w == [2, 0, 0, 0, 0, 1]));
        assert_eq!(&body[body.len() - 12..], &[172, 16, 2, 10, 255, 255, 255, 0, 172, 16, 2, 10]);
    }

    #[test]
    fn aggregates_and_dispatch() {
        let mut p = pdev();
        assert!(pd_expected_data(&p).is_empty());
        // MultipleBlockHeader is nested: its BlockLength covers the blocks it wraps (§2.8),
        // so the top level holds two MBHs, each wrapping one data block.
        let real = pd_real_data(&p);
        let top = BlockHeader::read_all(&real).unwrap();
        assert_eq!(top.len(), 2);
        assert!(top.iter().all(|(h, _)| h.block_type == ty::MULTIPLE_BLOCK_HEADER));
        assert_eq!(mbh(top[0].1), (IFACE_SUBSLOT, vec![ty::PD_INTERFACE_DATA_REAL]));
        assert_eq!(mbh(top[1].1), (PORT_SUBSLOT, vec![ty::PD_PORT_DATA_REAL]));
        // synthetic — replace by the Task 2 golden (`connect_write_check`'s 0x802B record).
        p.set_check(Some(parse_check(&synthetic_check_record("port-001.plcxb1d0ed", "plcxb1d0ed")).unwrap()), Instant::now());
        let exp = pd_expected_data(&p);
        let top = BlockHeader::read_all(&exp).unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].0.block_type, ty::MULTIPLE_BLOCK_HEADER);
        assert_eq!(mbh(top[0].1), (PORT_SUBSLOT, vec![ty::PD_PORT_DATA_CHECK]));
        assert!(read(&p, 0x802A, 0, PORT_SUBSLOT).is_some());
        assert!(read(&p, 0x802A, 0, IFACE_SUBSLOT).is_none());
        assert!(read(&p, 0x8080, 0, IFACE_SUBSLOT).is_some());
        assert!(read(&p, 0x802B, 0, PORT_SUBSLOT).is_some());
        assert!(read(&p, 0x802F, 0, PORT_SUBSLOT).unwrap().is_empty());
        assert!(read(&p, 0xF841, 0, 0).is_some());
        assert!(read(&p, 0xAFF0, 0, 1).is_none());
    }

    #[test]
    fn read_requests_target_the_indices_we_serve() {
        // synthetic — replace by the Task 2 goldens (`golden_in("pdev", "read_802a_req" |
        // "read_8080_req" | "read_f841_req" | "read_f842_req")`): indices per
        // docs/pdev-golden-frames.md §2.9; the (slot, subslot) TIA uses is capture-only.
        let p = pdev();
        for (idx, subslot) in [(0x802Au16, PORT_SUBSLOT), (0x8080, IFACE_SUBSLOT), (0xF841, IFACE_SUBSLOT), (0xF842, IFACE_SUBSLOT)] {
            let r = ReadReq::parse(&synthetic_read_req(Uuid([0; 16]), 0, subslot, idx)).unwrap();
            assert_eq!((r.index, r.slot, r.subslot), (idx, 0, subslot));
            assert!(read(&p, r.index, r.slot, r.subslot).is_some(), "{idx:#06x}");
        }
    }
}
```

- [ ] **Step 2: Run** → compile error.

- [ ] **Step 3: Implement.** Add `pub mod records;` to `pdev/mod.rs`. Layouts are `docs/pdev-golden-frames.md` §2.3 (PDPortDataReal), §2.4 (PDInterfaceDataReal), §2.5 (PDPortDataCheck), §2.8 (MultipleBlockHeader), all BlockVersion 1.0. Padding is "align to 4 relative to the block start" (§2.1: every block we build starts 4-aligned and keeps `4 + BlockLength` a multiple of 4, which decodes identically under both of the dissector's alignment readings). Every `pub` item gets a `///` doc comment (`missing_docs` + `-D warnings`), the `index`/`ty` constants included.

```rust
//! PDEV records: the read-side builders (`PDPortDataReal`, `PDInterfaceDataReal`,
//! `PDRealData`, `PDExpectedData`) and the write-side `PDPortDataCheck` parser.
//! Wire layouts: docs/pdev-golden-frames.md §2.

use super::{CheckRecord, ExpectedPeer, Pdev, IFACE_SUBSLOT, PORT_SUBSLOT};
use crate::cm::{BlockError, BlockHeader, Cursor};

/// Start a block: 6-byte header (version 1.0) with a placeholder BlockLength.
fn begin(block_type: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(96);
    BlockHeader::write(&mut b, block_type, 0);
    b
}

/// Pad with zeros to a multiple of 4 counted from the block's first header byte.
fn align4(b: &mut Vec<u8>) {
    while b.len() % 4 != 0 {
        b.push(0);
    }
}

/// Patch the BlockLength of a block built with [`begin`]: everything after the
/// type/length fields (so the 2 version bytes count).
fn finish(mut b: Vec<u8>) -> Vec<u8> {
    let len = (b.len() - 4) as u16;
    b[2..4].copy_from_slice(&len.to_be_bytes());
    b
}

/// `PDPortDataReal` (`0x020F`) for our port, §2.3.
pub fn port_data_real(p: &Pdev, slot: u16, subslot: u16) -> Vec<u8> {
    let mut b = begin(ty::PD_PORT_DATA_REAL);
    b.extend_from_slice(&[0, 0]); // padding: header is 6 bytes
    b.extend_from_slice(&slot.to_be_bytes());
    b.extend_from_slice(&subslot.to_be_bytes());
    let own = p.own_port_id().as_bytes();
    b.push(own.len() as u8); // LengthOwnPortID
    b.extend_from_slice(own); // OwnPortID (no padding before NumberOfPeers)
    let peer = p.peer();
    b.push(u8::from(peer.is_some())); // NumberOfPeers
    align4(&mut b);
    if let Some(peer) = peer {
        b.push(peer.port_id.len() as u8);
        b.extend_from_slice(peer.port_id.as_bytes());
        b.push(peer.chassis_id.len() as u8);
        b.extend_from_slice(peer.chassis_id.as_bytes());
        align4(&mut b);
        b.extend_from_slice(&0u32.to_be_bytes()); // LineDelay: format 0, value 0 = unknown
        b.extend_from_slice(&peer.mac); // PeerMACAddress
        align4(&mut b);
    }
    b.extend_from_slice(&0x0010u16.to_be_bytes()); // MAUType 100BASETXFD
    align4(&mut b);
    b.extend_from_slice(&0u32.to_be_bytes()); // DomainBoundary
    b.extend_from_slice(&0u32.to_be_bytes()); // MulticastBoundary
    b.push(0); // LinkState.Port: 0 unknown (what the 1515 puts here is capture-only)
    b.push(if p.link_up() { 1 } else { 2 }); // LinkState.Link: 1 up, 2 down
    align4(&mut b);
    b.extend_from_slice(&1u32.to_be_bytes()); // MediaType: copper
    finish(b)
}

/// `PDInterfaceDataReal` (`0x0240`), §2.4: no slot/subslot fields, no padding before
/// the name length.
pub fn interface_data_real(p: &Pdev) -> Vec<u8> {
    let net = p.net();
    let mut b = begin(ty::PD_INTERFACE_DATA_REAL);
    b.push(net.name.len() as u8); // LengthOwnStationName
    b.extend_from_slice(net.name.as_bytes());
    align4(&mut b);
    b.extend_from_slice(&p.mac()); // MACAddressValue
    align4(&mut b);
    b.extend_from_slice(&net.ip);
    b.extend_from_slice(&net.subnet);
    b.extend_from_slice(&net.gateway);
    finish(b)
}

/// `MultipleBlockHeader` (`0x0400`), §2.8: padding 2, API 0, slot, subslot, then the
/// nested blocks, which its BlockLength covers.
fn multiple_block_header(slot: u16, subslot: u16, nested: &[u8]) -> Vec<u8> {
    let mut b = begin(ty::MULTIPLE_BLOCK_HEADER);
    b.extend_from_slice(&[0, 0]);
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&slot.to_be_bytes());
    b.extend_from_slice(&subslot.to_be_bytes());
    b.extend_from_slice(nested);
    finish(b)
}

/// `PDRealData` (`0xF841`): the interface block, then the port block, each wrapped in a
/// MultipleBlockHeader (wrapping order capture-only, §2.9).
pub fn pd_real_data(p: &Pdev) -> Vec<u8> {
    let mut out = multiple_block_header(0, IFACE_SUBSLOT, &interface_data_real(p));
    out.extend(multiple_block_header(0, PORT_SUBSLOT, &port_data_real(p, 0, PORT_SUBSLOT)));
    out
}

/// `PDExpectedData` (`0xF842`): what TIA wrote, wrapped per subslot; empty when nothing
/// was written.
pub fn pd_expected_data(p: &Pdev) -> Vec<u8> {
    let mut port = Vec::new();
    if let Some(c) = p.check() {
        port.extend_from_slice(&c.raw);
    }
    if let Some(a) = p.adjust() {
        port.extend_from_slice(a);
    }
    let mut out = Vec::new();
    if !port.is_empty() {
        out.extend(multiple_block_header(0, PORT_SUBSLOT, &port));
    }
    if let Some(a) = p.iface_adjust() {
        out.extend(multiple_block_header(0, IFACE_SUBSLOT, a));
    }
    out
}

/// Parse a `PDPortDataCheck` record (the whole record data, block header included), §2.5:
/// padding 2, slot, subslot, then sub-blocks until the end or a `0x0000` BlockType (§2.1).
/// `CheckPeers` and `Checking MAUType` are read; other sub-blocks are skipped.
pub fn parse_check(record: &[u8]) -> Result<CheckRecord, BlockError> {
    let (h, body) = BlockHeader::parse(record)?;
    if h.block_type != ty::PD_PORT_DATA_CHECK {
        return Err(BlockError::UnexpectedType { expected: ty::PD_PORT_DATA_CHECK, got: h.block_type });
    }
    let mut c = Cursor::new(body);
    let _padding = c.u16()?;
    let _slot = c.u16()?;
    let _subslot = c.u16()?;
    let mut out = CheckRecord { peers: Vec::new(), mau_type: None, raw: record.to_vec() };
    let mut rest = &body[6..];
    while rest.len() >= BlockHeader::LEN && rest[..2] != [0, 0] {
        let (sh, sb) = BlockHeader::parse(rest)?;
        rest = &rest[BlockHeader::LEN + sb.len()..];
        let mut s = Cursor::new(sb);
        match sh.block_type {
            ty::CHECK_PEERS => {
                for _ in 0..s.u8()? {
                    let n = s.u8()? as usize;
                    let port_id = String::from_utf8_lossy(s.bytes(n)?).into_owned();
                    let n = s.u8()? as usize;
                    let chassis_id = String::from_utf8_lossy(s.bytes(n)?).into_owned();
                    out.peers.push(ExpectedPeer { port_id, chassis_id });
                }
            }
            ty::CHECK_MAU_TYPE => out.mau_type = Some(s.u16()?),
            _ => {}
        }
    }
    Ok(out)
}

/// Serve a PDEV read: `Some(record)` (possibly empty) for an index/subslot this module
/// owns, `None` otherwise (the caller falls through to I&M, then "invalid index").
pub fn read(p: &Pdev, idx: u16, slot: u16, subslot: u16) -> Option<Vec<u8>> {
    match (idx, slot, subslot) {
        (index::PD_PORT_DATA_REAL, 0, PORT_SUBSLOT) => Some(port_data_real(p, slot, subslot)),
        (index::PD_PORT_DATA_CHECK, 0, PORT_SUBSLOT) => Some(p.check().map(|c| c.raw.clone()).unwrap_or_default()),
        (index::PD_PORT_DATA_ADJUST, 0, PORT_SUBSLOT) => Some(p.adjust().map(<[u8]>::to_vec).unwrap_or_default()),
        (index::PD_INTERFACE_DATA_REAL, 0, IFACE_SUBSLOT) => Some(interface_data_real(p)),
        (index::PD_REAL_DATA, _, _) => Some(pd_real_data(p)),
        (index::PD_EXPECTED_DATA, _, _) => Some(pd_expected_data(p)),
        _ => None,
    }
}
```

plus the `index` and `ty` modules of the Interfaces block (each constant documented). An empty `pd_expected_data` is an empty record: `build_read_res` already handles `data.len() == 0`.

- [ ] **Step 4: Run** `cargo test -p pnio pdev::records` → PASS.

- [ ] **Step 5: Commit** — `feat(pdev): PDPortDataReal/PDInterfaceDataReal/PDRealData/PDExpectedData builders, PDPortDataCheck parsing`

---

### Task 10: `cm` — record dispatch, per-record Write status, station-name setter, abort reasons

**Files:**
- Modify: `crates/pnio/src/cm/write.rs:118-140` (`build_write_res`), `crates/pnio/src/cm/ar.rs:51-80` (`AbortReason`), `:122-145` (`Action::WriteAccepted`), `:322-357` (`handle_write`), new `Ar::set_station_name`, `crates/pnio/src/cm/records.rs:96-131` (`RecordCtx`, `read_record`, `write_pdev_record`), `crates/pnio/src/cm/mod.rs:327-358` (Write handling), `:505-522` (`handle_read`), new `set_station_name`/`pdev` accessors, `crates/pnio/src/device/mod.rs:129-145` (`err_rta_code2` arms for the two new reasons)

**Interfaces:**
- Produces:

```rust
pub fn build_write_res(req: &WriteReq, statuses: &[PnioStatus]) -> Vec<u8>;   // statuses.len() == req.records.len(); records[0] (the MultipleWrite container) keeps OK
pub enum AbortReason { …, DcpSet, ResetToFactory }
pub enum Action { …, WriteAccepted { req: WriteReq } }       // replaces Respond for an accepted Write
impl Ar { pub fn set_station_name(&mut self, name: &str); }   // the model the ARServerBlock is built from
pub struct RecordCtx<'a> { pub model: &'a DeviceModel, pub im0: &'a Im0, pub im: &'a ImStore, pub pdev: Option<&'a Pdev> }
pub fn write_pdev_record(p: &mut Pdev, r: &Record, now: Instant, actions: &mut Vec<PdevAction>) -> PnioStatus;   // cm::records
impl Cm {
    pub fn set_station_name(&mut self, name: &str);           // Cm's model copy + Ar::set_station_name
    pub fn set_pdev(&mut self, pdev: Option<Pdev>);          // Cm owns the Pdev so Read can serve it; Device borrows through these:
    pub fn pdev(&self) -> Option<&Pdev>;
    pub fn pdev_mut(&mut self) -> Option<&mut Pdev>;
}
/// Statuses for a Write's records, computed by `Cm` after the AR accepted the Write.
fn write_statuses(&mut self, records: &[Record], now: Instant) -> (Vec<PnioStatus>, Vec<PdevAction>);
pub struct CmOutput { pub send, pub notify, pub pdev: Vec<PdevAction> }   // new field: actions the device loop must apply (diag changes from a Check write)
```

Design note: `Pdev` lives in `Cm` (not `Device`) because `Read`/`Write` are served inside `Cm::handle_datagram` and need it synchronously; the device loop reaches it through `pdev_mut()` for `on_rx`/`tick`. `Ar::handle_write` must answer with the statuses, but it runs *before* `Cm` sees the records — so the AR machine hands the accepted request back and `Cm` builds the response: `handle_write` returns a new action `Action::WriteAccepted { req: WriteReq }` instead of `Respond` (plus the activity-timer side effect as today); `Cm::apply` turns it into `Respond { status: OK, blocks: build_write_res(&req, &statuses) }` after computing the statuses.

- [ ] **Step 1: Failing tests**

`write.rs`:

```rust
    #[test]
    fn write_res_carries_one_status_per_record() {
        let w = WriteReq::parse(&golden("write_req")[BLOCKS..]).unwrap();
        let all_ok = vec![PnioStatus::OK; w.records.len()];
        assert_eq!(build_write_res(&w, &all_ok), golden("write_res")[BLOCKS..].to_vec());
        let mut st = all_ok.clone();
        st[2] = PnioStatus::write_invalid_parameter();
        let out = build_write_res(&w, &st);
        // record 2 = third 64-byte response block; status at offset 6+2+16+4+2+2+2+2+4+2+2 = 44
        let off = 2 * 64 + 44;
        assert_eq!(&out[off..off + 4], &PnioStatus::write_invalid_parameter().to_u32().to_be_bytes());
        assert_eq!(&out[44..48], &[0, 0, 0, 0]);
    }
```

`cm/mod.rs` tests — the module already has `MAC`, `cpu()`, `cpu_cm()`, `cm()`, `pdu(name)` (`golden(name)[RPC_OFF..]`) and imports `golden`, `golden_alarm`, `RPC_OFF`; add the helpers below and the two tests (`use crate::testutil::{synthetic_check_record, synthetic_read_req, synthetic_write_record};`):

```rust
    /// `ar_uuid` of the AR the cm goldens establish (see `cm_in_data_with_the_alarm_capture_ar`).
    fn golden_ar() -> Uuid {
        Uuid::parse_str("e5e1aecc-b133-4b4d-b187-cc68b0211ed2").unwrap()
    }

    /// `template` (an RPC request PDU, from `RPC_OFF`) with its PNIO blocks replaced by
    /// `blocks`: same RPC header except a fresh `seq_num` (so `Cm`'s response cache does
    /// not answer it) and a recomputed `frag_len`; NDR header rebuilt for the new length.
    fn rpc_request(template: &[u8], blocks: &[u8], seq_num: u32) -> Vec<u8> {
        let mut h = RpcHeader::parse(template).unwrap();
        let (ndr, _) = NdrRequest::parse(&template[RpcHeader::LEN..], h.drep).unwrap();
        h.seq_num = seq_num;
        h.frag_len = (NdrRequest::LEN + blocks.len()) as u16;
        let mut out = Vec::with_capacity(RpcHeader::LEN + NdrRequest::LEN + blocks.len());
        h.write(&mut out);
        NdrRequest::for_blocks(ndr.args_max, blocks.len() as u32).write(&mut out, h.drep);
        out.extend_from_slice(blocks);
        out
    }

    /// `cm()` with a `Pdev` installed (as `Device::new_with` does when LLDP is on), AR
    /// brought to `Data` by the four cm goldens. `write_req` carries `0x8071` on subslot
    /// `0x8000`, which the `Pdev` now stores.
    fn cm_with_pdev_in_data(now: Instant) -> Cm {
        let mut cm = cm();
        let link = Box::new(crate::pdev::MockLink(true.into()));
        cm.set_pdev(Some(crate::pdev::Pdev::new(MAC.0, link, now)));
        for name in ["connect_req", "write_req", "prmend_req"] {
            cm.handle_datagram(&pdu(name), cpu(), now).unwrap();
        }
        cm.handle_datagram(&pdu("appready_res"), cpu_cm(), now).unwrap();
        assert_eq!(cm.state(), ArState::Data);
        cm
    }

    #[test]
    fn pdev_check_write_is_stored_and_read_back_and_f841_is_served() {
        let now = Instant::now();
        let mut cm = cm_with_pdev_in_data(now);
        assert!(cm.pdev().unwrap().iface_adjust().is_some(), "write_req's 0x8071 is stored");
        // synthetic — replace by the Task 2 golden (`connect_write_check`).
        let check = synthetic_check_record("port-001.plcxb1d0ed", "plcxb1d0ed");
        let blocks = synthetic_write_record(golden_ar(), 0, 0x8001, 0x802B, &check);
        let out = cm.handle_datagram(&rpc_request(&pdu("write_req"), &blocks, 0x200), cpu(), now).unwrap();
        assert_eq!(out.send.len(), 1);
        let (n, res) = NdrResponse::parse(&out.send[0].bytes[RpcHeader::LEN..], Drep::BIG).unwrap();
        assert!(PnioStatus(n.status).is_ok());
        assert_eq!(res.len(), 64, "one IODWriteResHeader");
        assert_eq!(&res[44..48], &[0, 0, 0, 0], "record status OK");
        assert_eq!(cm.pdev().unwrap().check().unwrap().peers[0].chassis_id, "plcxb1d0ed");
        // synthetic — replace by the Task 2 golden (`read_f841_req`); the Read envelope is
        // the alarm capture's I&M0 Read (opnum 2) with new blocks.
        let read = rpc_request(&golden_alarm("im0_read_req")[RPC_OFF..], &synthetic_read_req(golden_ar(), 0, 1, 0xF841), 0x201);
        let out = cm.handle_datagram(&read, cpu(), now).unwrap();
        let (n, res) = NdrResponse::parse(&out.send[0].bytes[RpcHeader::LEN..], Drep::BIG).unwrap();
        assert!(PnioStatus(n.status).is_ok());
        assert_eq!(u16::from_be_bytes([res[0], res[1]]), ty::IOD_READ_RES_HEADER);
        let record = BlockHeader::read_all(&res[64..]).unwrap();
        assert_eq!(record.len(), 2, "two MultipleBlockHeaders");
        assert!(record.iter().all(|(h, _)| h.block_type == crate::pdev::records::ty::MULTIPLE_BLOCK_HEADER));
    }

    #[test]
    fn set_station_name_reaches_the_ar_server_block_of_the_next_connect() {
        let mut cm = cm();
        cm.set_station_name("renamed");
        let o = cm.handle_datagram(&pdu("connect_req"), cpu(), Instant::now()).unwrap();
        let res = &o.send[0].bytes;
        assert!(res.windows(7).any(|w| w == b"renamed"), "ARServerBlock carries the new name");
        assert!(!res.windows(11).any(|w| w == b"rt-labs-dev"), "the old name is gone");
    }
```

`records.rs` (tests add `use crate::eth::MacAddr; use crate::pdev::{MockLink, Pdev}; use std::time::Instant;`):

```rust
    #[test]
    fn read_dispatches_pdev_indices_when_a_pdev_is_present() {
        let model = DeviceModel::pnet_sample(MacAddr([0; 6]));
        let (im0, store) = (Im0::default(), ImStore::new());
        let pdev = Pdev::new([0; 6], Box::new(MockLink(true.into())), Instant::now());
        let ctx = RecordCtx { model: &model, im0: &im0, im: &store, pdev: Some(&pdev) };
        let req = |index, subslot| ReadReq {
            seq: 0,
            ar_uuid: Uuid([0; 16]),
            api: 0,
            slot: 0,
            subslot,
            index,
            record_data_length: 0x8000,
            target_ar_uuid: Uuid([0; 16]),
        };
        assert!(read_record(&req(0x802A, 0x8001), &ctx).is_some());
        assert!(read_record(&req(0x8080, 0x8000), &ctx).is_some());
        assert!(read_record(&req(0xF841, 0x0001), &ctx).is_some());
        let none = RecordCtx { pdev: None, ..ctx };
        assert!(read_record(&req(0x802A, 0x8001), &none).is_none());
    }
```

- [ ] **Step 2: Run** → compile errors.

- [ ] **Step 3: Implement**

`build_write_res(req, statuses)`: same loop, `statuses[i].to_u32().to_be_bytes()` in the status field (`assert_eq!(statuses.len(), req.records.len())` — a debug assert is fine; production callers always build it from the records). All existing callers pass `&vec![PnioStatus::OK; n]`.

`ar.rs`: add `Action::WriteAccepted { req: WriteReq }` (doc: "the AR accepted this Write; `Cm` computes per-record statuses and builds the response"); `handle_write` returns it instead of `Respond`, after the same checks and `ctx.records.extend(req.records.clone())` (its `build_write_res(&req)` call goes: `Cm` builds the response). Add `AbortReason::DcpSet` ("a DCP Set changed the station name or IP suite") and `AbortReason::ResetToFactory`. Map both to ERR-RTA `RTA_ABORT_AR_REMOVED` in `device/mod.rs`'s exhaustive `err_rta_code2` (the arm that lists `AbortReason::Shutdown`). Add `Ar::set_station_name(&mut self, name: &str)` (doc: "the name answered in the ARServerBlock of the next Connect"): `self.model.station_name = name.to_string()` — `build_connect_res(&params, &self.model)` (`ar.rs:263`, `connect.rs:354`) writes it.

`cm/mod.rs`: field `pdev: Option<Pdev>`; `set_pdev`, `pdev`, `pdev_mut`, `set_station_name` (doc: "rename the device for the PNIO side: I&M/record model and the next Connect's ARServerBlock"): `self.model.station_name = name.to_string(); self.ar.set_station_name(name);`. In `apply`, handle `Action::WriteAccepted { req }`:

```rust
Action::WriteAccepted { req } => {
    let (statuses, pdev_actions) = self.write_statuses(&req.records, now);
    output.pdev.extend(pdev_actions);
    let blocks = build_write_res(&req, &statuses);
    // then exactly what `Respond { status: OK, blocks }` does today (build + cache the response PDU)
}
```

(`apply` needs `now`; thread it through — `handle_datagram` and `tick` have it.) `write_statuses`:

```rust
fn write_statuses(&mut self, records: &[Record], now: Instant) -> (Vec<PnioStatus>, Vec<PdevAction>) {
    let mut actions = Vec::new();
    let statuses = records.iter().map(|r| {
        match r.index {
            INDEX_MULTIPLE_WRITE => PnioStatus::OK,
            INDEX_IM1..=INDEX_IM3 => write_im_record(r, &self.model, &mut self.im),
            index::PD_PORT_DATA_CHECK | index::PD_PORT_DATA_ADJUST | index::PD_INTERFACE_ADJUST => {
                match self.pdev.as_mut() {
                    None => PnioStatus::write_index_unsupported(),
                    Some(p) => write_pdev_record(p, r, now, &mut actions),
                }
            }
            _ => PnioStatus::OK, // spec decision 8: lenient on unknown indices
        }
    }).collect();
    (statuses, actions)
}
```

with, in `records.rs`:

```rust
/// Store a PDEV engineering record. `write_invalid_parameter` on the wrong subslot or a
/// malformed `PDPortDataCheck`; Adjust records are stored raw.
pub fn write_pdev_record(p: &mut Pdev, r: &Record, now: Instant, actions: &mut Vec<PdevAction>) -> PnioStatus {
    use crate::pdev::records::{index, parse_check};
    use crate::pdev::{IFACE_SUBSLOT, PORT_SUBSLOT};
    match (r.index, r.subslot) {
        (index::PD_PORT_DATA_CHECK, PORT_SUBSLOT) => match parse_check(&r.data) {
            Ok(c) => { actions.extend(p.set_check(Some(c), now)); PnioStatus::OK }
            Err(e) => { log::warn!("PDPortDataCheck rejected: {e}"); PnioStatus::write_invalid_parameter() }
        },
        (index::PD_PORT_DATA_ADJUST, PORT_SUBSLOT) => { p.set_adjust(Some(r.data.clone())); PnioStatus::OK }
        (index::PD_INTERFACE_ADJUST, IFACE_SUBSLOT) => { p.set_iface_adjust(Some(r.data.clone())); PnioStatus::OK }
        _ => PnioStatus::write_invalid_parameter(),
    }
}
```

`read_record`: after the model check, `if let Some(p) = ctx.pdev { if let Some(d) = crate::pdev::records::read(p, req.index, req.slot, req.subslot) { return Some(d); } }` before the I&M `match`. **Golden guard:** `testdata/cm/write_req.hex` carries `0x8071` on subslot `0x8000` — with a `Pdev` present it is now stored; the response bytes stay all-OK (`write_res` golden still matches). The I&M path change: a non-OK `write_im_record` now reaches the wire (was log-only) — `records.rs` doc comment at `:124-131` must be updated.

Remove the now-dead "I&M write rejected" log loop in `handle_datagram` (statuses carry it). `CmOutput` gets `pub pdev: Vec<PdevAction>`, default empty; `Device::dispatch` (Task 12) applies them.

- [ ] **Step 4: Run** `cargo test -p pnio` → all green (the `ar.rs` unit tests asserting `Action::Respond` on Write must be updated to `Action::WriteAccepted`).

- [ ] **Step 5: Commit** — `feat(cm): per-record Write statuses, PDEV record dispatch (read + write), set_station_name, DcpSet/ResetToFactory abort reasons`

---

### Task 11: `diag` — port remote-mismatch entry (ExtChannelDiagnosis)

**Files:**
- Modify: `crates/pnio/src/diag.rs` (`DiagStore`: port entry, `set_port_mismatch`, included in `problem_indicator` and `replay`; `active()` excludes it)

**Interfaces:**
- Consumes: `pdev::Mismatch`, `alarm::{ExtChannelDiagnosis, UserData, USI_EXT_CHANNEL_DIAG}`.
- Produces:

```rust
/// `ChannelErrorType` for the port's remote-mismatch diagnoses ("Remote mismatch", pinned in docs/pdev-golden-frames.md §2.10).
pub const PORT_ERROR_TYPE: u16 = 0x8001;
impl DiagStore {
    /// Register the DAP port submodule so port diagnoses can be built (module/submodule idents from the model's slot 0 / subslot 0x8001).
    pub fn with_port(self, model: &DeviceModel) -> Self;
    /// Set the port's mismatch state; returns the notification(s) implied: Appears on a new/changed mismatch, Disappears on `None`.
    pub fn set_port_mismatch(&mut self, m: Option<Mismatch>) -> Vec<AlarmNotification>;
    pub fn port_mismatch(&self) -> Option<Mismatch>;
}
```

`active()` (application-level `Diagnosis` list) does not include the port entry (it is not a `ChannelError`); `IoDevice` exposes it through `peer()`/a new `port_mismatch()` instead. `problem_indicator()` is true while a mismatch is set (severity Fault). `replay()` includes the port Appears notification.

- [ ] **Step 1: Failing tests** (`diag.rs` tests; the module imports `super::*` and `crate::alarm::*` but no `MacAddr`: add `use crate::eth::MacAddr;` to its imports):

```rust
    #[test]
    fn port_mismatch_appears_changes_and_disappears_as_ext_channel_diag() {
        use crate::pdev::Mismatch;
        let model = DeviceModel::pnet_sample(MacAddr([0; 6]));
        let mut s = DiagStore::from_model(&model).with_port(&model);
        assert!(s.set_port_mismatch(None).is_empty());
        let n = s.set_port_mismatch(Some(Mismatch::Port));
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].alarm_type, AlarmType::Diagnosis);
        assert_eq!((n[0].slot, n[0].subslot), (0, 0x8001));
        assert_eq!(n[0].usi, USI_EXT_CHANNEL_DIAG);
        let UserData::ExtChannel(d) = &n[0].data else { panic!() };
        assert_eq!(d.channel, WHOLE_SUBMODULE);
        assert_eq!(d.error_type, PORT_ERROR_TYPE);
        assert_eq!(d.ext_error_type, Mismatch::Port.ext_error_type());
        assert!(s.problem_indicator());
        assert!(s.set_port_mismatch(Some(Mismatch::Port)).is_empty(), "no churn");
        let n = s.set_port_mismatch(Some(Mismatch::Chassis));
        assert_eq!(n.len(), 2, "old disappears, new appears");
        assert_eq!(n[0].alarm_type, AlarmType::DiagnosisDisappears);
        assert_eq!(n[1].alarm_type, AlarmType::Diagnosis);
        assert_eq!(s.replay().len(), 1);
        let n = s.set_port_mismatch(None);
        assert_eq!(n[0].alarm_type, AlarmType::DiagnosisDisappears);
        assert!(!s.problem_indicator());
        assert!(s.replay().is_empty());
        assert!(s.active().is_empty(), "the port entry is not an application ChannelError");
    }

    #[test]
    fn port_mismatch_without_with_port_is_ignored() {
        let model = DeviceModel::pnet_sample(MacAddr([0; 6]));
        let mut s = DiagStore::from_model(&model);
        assert!(s.set_port_mismatch(Some(crate::pdev::Mismatch::NoPeer)).is_empty());
    }
```

- [ ] **Step 2: Run** → compile error.

- [ ] **Step 3: Implement.** `DiagStore` gains `port: Option<(SubmoduleInfo, Option<Mismatch>)>`. `with_port`: find slot 0, subslot `0x8001` in `model.slots` → `SubmoduleInfo { slot: Slot(0), subslot: 0x8001, module_ident, submodule_ident, direction: Direction::InputOutput }`. `set_port_mismatch`: compare with current; build with a new private `build_port_notification(info, m, alarm_type, specifier)` producing `AlarmNotification { usi: USI_EXT_CHANNEL_DIAG, data: UserData::ExtChannel(ExtChannelDiagnosis { channel: WHOLE_SUBMODULE, properties: ChannelProperties { type_: 0, accumulative: false, maintenance: Maintenance::Fault, specifier, direction: 0 }, error_type: PORT_ERROR_TYPE, ext_error_type: m.ext_error_type(), ext_add_value: 0 }), specifier: AlarmSpecifier { channel_diag: true, submodule_diag: true, ar_diag: true, manufacturer_diag: false, sequence: 0 }, … }`. Check `ChannelProperties.direction` for the "0 = manufacturer specific" value and `Specifier` variants in `alarm::rta`. `problem_indicator`: `|| self.port.as_ref().is_some_and(|(_, m)| m.is_some())`. `replay`: append the port Appears when set. The `ar_diag`/`channel_diag` flags in `build_notification` should also consider the port entry (`ar_diag = !self.active.is_empty() || port set`).

- [ ] **Step 4: Run** `cargo test -p pnio diag` → PASS.

- [ ] **Step 5: Commit** — `feat(diag): port remote-mismatch diagnosis as ExtChannelDiagnosis (USI 0x8002)`

---

### Task 12: `device` — identity, DCP effects, LLDP socket, pdev tick, hooks, shutdown PDU

**Files:**
- Modify: `crates/pnio/src/device/mod.rs` (struct `Device`, `DeviceSetup`, `DeviceHooks`, `DeviceShared`, `new`/`new_with`, `step`, `dispatch`, `shutdown`, DCP handling), `crates/pnio/src/dcp/mod.rs` (`handle_dcp_frame` split into `classify`), `crates/pnio/src/cm/mod.rs` (`Cm::im_store_mut`)
- Modify, every `DeviceSetup` literal (two new fields, `net_store: None, iface: String::new()` — `iface` only matters to an `IpConfigurator`, and `Device::new` installs `NoopIp`; `api::start_inner` overrides both in Task 13): `crates/pnio/src/config.rs:383` (`DeviceConfig::setup`), `crates/pnio/src/device/mod.rs:858` (test `setup()`), `crates/pnio/tests/ar_replay.rs:19`, `crates/pnio/tests/alarm_replay.rs:64`, `crates/pnio/tests/rt_replay.rs:23`, `crates/pnio/examples/linux/ar_bringup.rs:57`, `crates/pnio/examples/linux/rt_bringup.rs:132`

**Interfaces:**
- Consumes: everything from Tasks 4-11; `testutil::{dcp_set_frame, im1_record}` (Tasks 4, 5).
- Produces:

```rust
pub struct DeviceSetup { …, pub im_store: Option<PathBuf>, pub net_store: Option<PathBuf>, pub iface: String }
/// Injectable side effects (defaults: no-op IP, always-up link, no LLDP transport, no hook).
pub struct DeviceHooks {
    pub ip: Box<dyn IpConfigurator>,
    pub link: Box<dyn LinkProbe>,
    pub lldp: Option<Box<dyn EthTransport>>,
    pub on_signal: Option<Arc<dyn Fn() + Send + Sync>>,
}
impl Default for DeviceHooks;   // NoopIp + MockLink(true) + no lldp + no hook (the facade installs the real ones)
#[derive(Debug, Default)]
pub struct DeviceShared { pub net: Mutex<NetParams>, pub signal_count: AtomicU32, pub peer: Mutex<Option<Peer>>, pub port_mismatch: Mutex<Option<Mismatch>> }
impl Device {
    pub fn new(setup: DeviceSetup, eth: E, rpc: R) -> Device<E, R>;                 // = new_with(…, DeviceHooks::default(), Instant::now())
    pub fn new_with(setup: DeviceSetup, eth: E, rpc: R, hooks: DeviceHooks, now: Instant) -> Device<E, R>;
    pub fn net_params(&self) -> NetParams;
    pub fn signal_count(&self) -> u32;
    pub fn peer(&self) -> Option<Peer>;
    pub fn port_mismatch(&self) -> Option<Mismatch>;
    pub fn device_shared(&self) -> Arc<DeviceShared>;
    #[doc(hidden)] pub fn cm(&self) -> &Cm;
    #[doc(hidden)] pub fn cm_mut(&mut self) -> &mut Cm;
    pub(crate) fn apply_pdev_actions(&mut self, actions: Vec<PdevAction>, now: Instant) -> Result<(), DeviceError>;
    fn apply_pdev_actions_in(&mut self, actions: Vec<PdevAction>, now: Instant, pending: &mut Vec<(u64, Vec<AlarmAction>)>);
    fn apply_net(&mut self, net: NetParams, now: Instant);
    fn handle_set(&mut self, src: MacAddr, xid: u32, decisions: Vec<SetDecision>, now: Instant, pending: &mut Vec<(u64, Vec<AlarmAction>)>, report: &mut StepReport) -> Result<(), DeviceError>;
}
impl Cm { pub fn im_store_mut(&mut self) -> &mut ImStore; }
/// `dcp::handle_dcp_frame` is split: `dcp::classify(frame, cfg) -> Result<Option<DcpRequest>, DcpError>` where
/// `enum DcpRequest { Identify { reply: Vec<u8> }, Set { src: MacAddr, xid: u32, decisions: Vec<SetDecision> } }`;
/// `Device` answers Identify as before and runs the Set effects itself.
```

`DeviceShared` (like `DiagShared`) is published by the loop and read by `IoDevice` (Task 13) through `Device::device_shared()`.

- [ ] **Step 1: Failing tests** (in `device/mod.rs`'s test module; reuse `setup()` (gains `net_store: None, iface: String::new()`), `MAC`, `CPU_MAC`, `SharedEth`, `feed_bring_up`, `cpu_alarm`; add the imports `use crate::dcp::{DcpBlock, MockIp, NetParams, SetBlockError};`, `use crate::im::INDEX_IM1;`, `use crate::pdev::{CheckRecord, ExpectedPeer, Mismatch, MockLink};`, `use crate::testutil::{dcp_set_frame, im1_record};`, `use std::sync::atomic::AtomicU32;`):

```rust
    fn hooks() -> (DeviceHooks, SharedEth, Arc<Mutex<Vec<NetParams>>>) {
        let lldp = SharedEth::new();
        let ip = MockIp::default();
        let calls = ip.calls.clone();
        let h = DeviceHooks { ip: Box::new(ip), link: Box::new(MockLink(true.into())), lldp: Some(Box::new(lldp.clone())), on_signal: None };
        (h, lldp, calls)
    }

    /// An untagged DCP Set request from the CPU to this device, xid `0x1234`: Ethernet
    /// header + FrameID `0xFEFD` + `DcpHeader { Set, Request, xid, 0, data_length }` +
    /// `write_blocks(&blocks, ..)`, each block's `block_info` = its `BlockQualifier`
    /// (`testutil::dcp_set_frame`, written out in Task 4).
    fn set_frame(blocks: Vec<DcpBlock>) -> Vec<u8> {
        dcp_set_frame(MAC, CPU_MAC, 0x1234, &blocks)
    }

    /// The Control/Response blocks of the last DCP Set response sent (untagged, built by
    /// `build_set_response`: blocks start at 14 + 2 + 10 = 26); each value is
    /// `[option, suboption, BlockError]`.
    fn set_response_blocks(eth: &SharedEth) -> Vec<DcpBlock> {
        let resp = eth
            .sent()
            .into_iter()
            .rev()
            .find(|f| f.len() > 16 && f[12..14] == [0x88, 0x92] && f[14..16] == [0xfe, 0xfd])
            .expect("a DCP Set response");
        crate::dcp::parse_blocks(&resp[26..], false).unwrap()
    }

    /// An Identify request from the CPU filtering on `name` (NameOfStation, 2/2), or an
    /// AllSelector (0xff/0xff) for `None` — the frame shape of `dcp::dispatch_tests::req_frame`
    /// (private to that module, so rebuilt here from the public codec).
    fn identify_frame(name: Option<&str>) -> Vec<u8> {
        use crate::dcp::{write_blocks, DcpHeader, FrameId, ServiceId, ServiceType, DCP_MULTICAST_MAC};
        use crate::eth::{EthHeader, ETHERTYPE_PROFINET};
        let block = match name {
            Some(n) => DcpBlock { option: 2, suboption: 2, block_info: None, value: n.as_bytes().to_vec() },
            None => DcpBlock { option: 0xff, suboption: 0xff, block_info: None, value: vec![] },
        };
        let mut blocks = Vec::new();
        write_blocks(&[block], &mut blocks);
        let mut f = Vec::new();
        EthHeader { dst: DCP_MULTICAST_MAC, src: CPU_MAC, vlan: None, ethertype: ETHERTYPE_PROFINET }.write(&mut f);
        f.extend_from_slice(&FrameId::IdentifyRequest.to_u16().to_be_bytes());
        DcpHeader { service_id: ServiceId::Identify, service_type: ServiceType::Request, xid: 0x0300_0152, response_delay: 1, data_length: blocks.len() as u16 }.write(&mut f);
        f.extend_from_slice(&blocks);
        f
    }

    /// Identify responses (FrameID `0xFEFF`) sent so far, oldest first.
    fn identify_responses(eth: &SharedEth) -> Vec<Vec<u8>> {
        eth.sent().into_iter().filter(|f| f.len() > 16 && f[12..14] == [0x88, 0x92] && f[14..16] == [0xfe, 0xff]).collect()
    }

    #[test]
    fn lldp_is_sent_at_start_and_the_peer_is_published() {
        let (h, lldp, _) = hooks();
        let t0 = Instant::now();
        let mut dev = Device::new_with(setup(), SharedEth::new(), MockRpcTransport::new(), h, t0);
        dev.step(t0, Some(Duration::ZERO)).unwrap();
        assert_eq!(lldp.sent().len(), 1);
        let frame = crate::lldp::build_device_pdu("plc", [0xec; 6], "port-001.plc", 20).frame(CPU_MAC);
        lldp.push_rx(frame);
        dev.step(t0 + Duration::from_millis(10), Some(Duration::ZERO)).unwrap();
        assert_eq!(dev.peer().unwrap().chassis_id, "plc");
    }

    #[test]
    fn check_mismatch_raises_then_clears_a_diagnosis_alarm() {
        let (h, lldp, _) = hooks();
        let eth = SharedEth::new();
        let t0 = Instant::now();
        let mut dev = Device::new_with(setup(), eth.clone(), MockRpcTransport::new(), h, t0);
        feed_bring_up(&dev, None, None);
        dev.step(t0, Some(Duration::ZERO)).unwrap();
        assert_eq!(dev.state(), ArState::Data);
        // Peer "plc"/port-002, then a Check expecting port-001 → Port mismatch → Diagnosis alarm on 0x8001.
        lldp.push_rx(crate::lldp::build_device_pdu("plc", [0xec; 6], "port-002.plc", 20).frame(CPU_MAC));
        let t1 = t0 + Duration::from_millis(1);
        dev.step(t1, Some(Duration::ZERO)).unwrap();
        let check = CheckRecord { peers: vec![ExpectedPeer { port_id: "port-001.plc".into(), chassis_id: "plc".into() }], mau_type: None, raw: vec![] };
        let acts = dev.cm_mut().pdev_mut().unwrap().set_check(Some(check), t1);
        dev.apply_pdev_actions(acts, t1).unwrap();
        let alarm = eth.sent().into_iter().rev().find(|f| is_alarm_frame(f)).expect("a Diagnosis alarm was sent");
        let pdu = parse_frame(&alarm).unwrap();
        let RtaBody::Data(RtaData::Notification(n)) = pdu.body else {
            panic!("expected an AlarmNotification, got {:?}", pdu.body)
        };
        assert_eq!(n.alarm_type, AlarmType::Diagnosis);
        assert_eq!((n.slot, n.subslot), (0, 0x8001));
        assert_eq!(n.usi, 0x8002);
        let crate::alarm::UserData::ExtChannel(d) = &n.data else { panic!("expected ExtChannelDiagnosis") };
        assert_eq!((d.error_type, d.ext_error_type), (0x8001, Mismatch::Port.ext_error_type()));
        assert!(dev.problem_indicator());
        assert_eq!(dev.port_mismatch(), Some(Mismatch::Port));
        lldp.push_rx(crate::lldp::build_device_pdu("plc", [0xec; 6], "port-001.plc", 20).frame(CPU_MAC));
        dev.step(t0 + Duration::from_millis(3), Some(Duration::ZERO)).unwrap();
        assert_eq!(dev.port_mismatch(), None);
        assert!(!dev.problem_indicator());
    }

    #[test]
    fn set_name_applies_persists_aborts_and_updates_identify() {
        let dir = std::env::temp_dir().join(format!("pnio-dev-net-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = setup();
        s.net_store = Some(dir.join("net"));
        let (h, lldp, calls) = hooks();
        let eth = SharedEth::new();
        let t0 = Instant::now();
        let mut dev = Device::new_with(s, eth.clone(), MockRpcTransport::new(), h, t0);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        dev.on_state_change(move |st, why| s2.lock().unwrap().push((st, why)));
        feed_bring_up(&dev, None, None);
        dev.step(t0, Some(Duration::ZERO)).unwrap();
        let before = lldp.sent().len();
        eth.push_rx(set_frame(vec![DcpBlock { option: 2, suboption: 2, block_info: Some(1), value: b"renamed".to_vec() }]));
        dev.step(t0 + Duration::from_millis(1), Some(Duration::ZERO)).unwrap();
        assert_eq!(dev.net_params().name, "renamed");
        assert_eq!(dev.state(), ArState::Idle);
        assert_eq!(seen.lock().unwrap().last(), Some(&(ArState::Idle, Some(AbortReason::DcpSet))));
        assert!(calls.lock().unwrap().is_empty(), "name-only set does not touch the interface");
        assert!(std::fs::read_to_string(dir.join("net")).unwrap().contains("name=renamed"));
        assert!(lldp.sent().len() > before, "immediate LLDPDU after rename");
        let blocks = set_response_blocks(&eth);
        assert_eq!(blocks.len(), 1, "one Control/Response block");
        assert_eq!(blocks[0].value, vec![2, 2, SetBlockError::Ok as u8]);
        // Identify answers the new name, and no longer the old one.
        let n = identify_responses(&eth).len();
        eth.push_rx(identify_frame(Some("renamed")));
        dev.step(t0 + Duration::from_millis(2), Some(Duration::ZERO)).unwrap();
        assert_eq!(identify_responses(&eth).len(), n + 1);
        eth.push_rx(identify_frame(Some("rt-labs-dev")));
        dev.step(t0 + Duration::from_millis(3), Some(Duration::ZERO)).unwrap();
        assert_eq!(identify_responses(&eth).len(), n + 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn set_name_and_ip_in_one_request_applies_both_with_one_abort() {
        let (h, _, calls) = hooks();
        let eth = SharedEth::new();
        let t0 = Instant::now();
        let mut dev = Device::new_with(setup(), eth.clone(), MockRpcTransport::new(), h, t0);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        dev.on_state_change(move |st, why| s2.lock().unwrap().push((st, why)));
        feed_bring_up(&dev, None, None);
        dev.step(t0, Some(Duration::ZERO)).unwrap();
        eth.push_rx(set_frame(vec![
            DcpBlock { option: 2, suboption: 2, block_info: Some(1), value: b"both".to_vec() },
            DcpBlock { option: 1, suboption: 2, block_info: Some(1), value: vec![10, 0, 0, 5, 255, 0, 0, 0, 10, 0, 0, 1] },
        ]));
        dev.step(t0 + Duration::from_millis(1), Some(Duration::ZERO)).unwrap();
        assert_eq!(dev.net_params().name, "both");
        assert_eq!(dev.net_params().ip, [10, 0, 0, 5]);
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert_eq!(dev.state(), ArState::Idle);
        assert_eq!(seen.lock().unwrap().iter().filter(|(st, _)| *st == ArState::Idle).count(), 1, "one abort");
        let blocks = set_response_blocks(&eth);
        assert_eq!(blocks.len(), 2, "two Control/Response blocks");
        assert_eq!(blocks[0].value, vec![2, 2, SetBlockError::Ok as u8]);
        assert_eq!(blocks[1].value, vec![1, 2, SetBlockError::Ok as u8]);
    }

    #[test]
    fn ip_apply_failure_answers_set_not_possible_and_keeps_the_old_ip() {
        let lldp = SharedEth::new();
        let h = DeviceHooks { ip: Box::new(MockIp { fail: true, ..Default::default() }), link: Box::new(MockLink(true.into())), lldp: Some(Box::new(lldp)), on_signal: None };
        let eth = SharedEth::new();
        let t0 = Instant::now();
        let mut dev = Device::new_with(setup(), eth.clone(), MockRpcTransport::new(), h, t0);
        let old = dev.net_params();
        eth.push_rx(set_frame(vec![DcpBlock { option: 1, suboption: 2, block_info: Some(1), value: vec![10, 0, 0, 5, 255, 0, 0, 0, 0, 0, 0, 0] }]));
        dev.step(t0, Some(Duration::ZERO)).unwrap();
        assert_eq!(dev.net_params(), old);
        assert_eq!(set_response_blocks(&eth)[0].value, vec![1, 2, SetBlockError::SetNotPossible as u8]);
    }

    #[test]
    fn signal_counts_and_calls_the_hook() {
        let hit = Arc::new(AtomicU32::new(0));
        let h2 = hit.clone();
        let h = DeviceHooks { on_signal: Some(Arc::new(move || { h2.fetch_add(1, Ordering::Relaxed); })), ..DeviceHooks::default() };
        let eth = SharedEth::new();
        let t0 = Instant::now();
        let mut dev = Device::new_with(setup(), eth.clone(), MockRpcTransport::new(), h, t0);
        eth.push_rx(set_frame(vec![DcpBlock { option: 5, suboption: 3, block_info: Some(0), value: vec![0x01, 0x00] }]));
        dev.step(t0, Some(Duration::ZERO)).unwrap();
        assert_eq!(dev.signal_count(), 1);
        assert_eq!(hit.load(Ordering::Relaxed), 1);
        assert_eq!(set_response_blocks(&eth)[0].value, vec![5, 3, SetBlockError::Ok as u8]);
    }

    #[test]
    fn reset_mode_4_blanks_identity_and_im_and_aborts() {
        let dir = std::env::temp_dir().join(format!("pnio-dev-reset-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = setup();
        s.net_store = Some(dir.join("net"));
        s.im_store = Some(dir.join("im"));
        let (h, _, calls) = hooks();
        let eth = SharedEth::new();
        let t0 = Instant::now();
        let mut dev = Device::new_with(s, eth.clone(), MockRpcTransport::new(), h, t0);
        feed_bring_up(&dev, None, None);
        dev.step(t0, Some(Duration::ZERO)).unwrap();
        dev.cm_mut().im_store_mut().write(INDEX_IM1, &im1_record("F", "L")).unwrap();
        eth.push_rx(set_frame(vec![DcpBlock { option: 5, suboption: 6, block_info: Some(4 << 1), value: vec![] }]));
        dev.step(t0 + Duration::from_millis(1), Some(Duration::ZERO)).unwrap();
        assert!(dev.net_params().is_blank());
        assert_eq!(dev.state(), ArState::Idle);
        assert_eq!(calls.lock().unwrap().last().unwrap().ip, [0; 4]);
        assert_eq!(dev.cm().im_store().tag_function().trim(), "");
        assert!(std::fs::read_to_string(dir.join("net")).unwrap().contains("name=\n"));
        assert_eq!(set_response_blocks(&eth)[0].value, vec![5, 6, SetBlockError::Ok as u8]);
        // Identify-all is answered, with a blank NameOfStation.
        eth.push_rx(identify_frame(None));
        dev.step(t0 + Duration::from_millis(2), Some(Duration::ZERO)).unwrap();
        let resp = identify_responses(&eth).pop().expect("Identify-all is answered");
        let (_, blocks) = crate::dcp::DcpHeader::parse(&resp[16..]).unwrap();
        let blocks = crate::dcp::parse_blocks(blocks, true).unwrap();
        assert!(blocks.iter().filter(|b| (b.option, b.suboption) == (2, 2)).all(|b| b.value.is_empty()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reset_with_a_failing_ip_configurator_answers_set_not_possible_and_resets_nothing() {
        let lldp = SharedEth::new();
        let h = DeviceHooks { ip: Box::new(MockIp { fail: true, ..Default::default() }), link: Box::new(MockLink(true.into())), lldp: Some(Box::new(lldp)), on_signal: None };
        let eth = SharedEth::new();
        let t0 = Instant::now();
        let mut dev = Device::new_with(setup(), eth.clone(), MockRpcTransport::new(), h, t0);
        feed_bring_up(&dev, None, None);
        dev.step(t0, Some(Duration::ZERO)).unwrap();
        let old = dev.net_params();
        eth.push_rx(set_frame(vec![DcpBlock { option: 5, suboption: 6, block_info: Some(4 << 1), value: vec![] }]));
        dev.step(t0 + Duration::from_millis(1), Some(Duration::ZERO)).unwrap();
        assert_eq!(dev.net_params(), old);
        assert_eq!(dev.state(), ArState::Data, "nothing applied, so no abort");
        assert_eq!(set_response_blocks(&eth)[0].value, vec![5, 6, SetBlockError::SetNotPossible as u8]);
    }

    #[test]
    fn ar_abort_clears_engineering() {
        let (h, _, _) = hooks();
        let eth = SharedEth::new();
        let t0 = Instant::now();
        let mut dev = Device::new_with(setup(), eth.clone(), MockRpcTransport::new(), h, t0);
        feed_bring_up(&dev, None, None);
        dev.step(t0, Some(Duration::ZERO)).unwrap();
        let check = CheckRecord { peers: vec![ExpectedPeer { port_id: "port-001.plc".into(), chassis_id: "plc".into() }], mau_type: None, raw: vec![] };
        let p = dev.cm_mut().pdev_mut().unwrap();
        p.set_adjust(Some(vec![1, 2, 3]));
        let _ = p.set_check(Some(check), t0);
        // The controller aborts (Plan 5 pattern: its ERR-RTA on the alarm channel).
        eth.push_rx(cpu_alarm("alarm_err_rta_cpu_removed"));
        dev.step(t0 + Duration::from_millis(1), Some(Duration::ZERO)).unwrap();
        assert_eq!(dev.state(), ArState::Idle);
        let p = dev.cm().pdev().unwrap();
        assert!(p.adjust().is_none());
        assert!(p.check().is_none());
    }

    #[test]
    fn shutdown_sends_ttl_zero_and_clears_engineering_with_or_without_an_ar() {
        let (h, lldp, _) = hooks();
        let t0 = Instant::now();
        let mut dev = Device::new_with(setup(), SharedEth::new(), MockRpcTransport::new(), h, t0);
        feed_bring_up(&dev, None, None);
        dev.step(t0, Some(Duration::ZERO)).unwrap();
        dev.cm_mut().pdev_mut().unwrap().set_adjust(Some(vec![1, 2, 3]));
        dev.shutdown(t0 + Duration::from_secs(1)).unwrap();
        assert_eq!(dev.state(), ArState::Idle);
        assert!(dev.cm().pdev().unwrap().adjust().is_none());
        fn ttl(f: &[u8]) -> u16 {
            crate::lldp::parse_frame(f).unwrap().1.ttl
        }
        assert_eq!(ttl(lldp.sent().last().unwrap()), 0);
        // From Idle (no AR) the shutdown PDU still goes out.
        let n = lldp.sent().len();
        dev.shutdown(t0 + Duration::from_secs(2)).unwrap();
        assert_eq!(lldp.sent().len(), n + 1);
        assert_eq!(ttl(lldp.sent().last().unwrap()), 0);
    }
```

- [ ] **Step 2: Run** → compile errors.

- [ ] **Step 3: Implement.** Every `pub` item gets a `///` doc comment (`missing_docs` + `-D warnings`), the `DeviceSetup`/`DeviceHooks`/`DeviceShared` fields and `DcpRequest` variants included.

`dcp/mod.rs`: replace `handle_dcp_frame`'s body by `classify`, and keep `handle_dcp_frame` as a thin wrapper (used by the `dcp` tests and the crate-level doc example):

```rust
/// What a received frame asks of the device, once classified against `cfg`.
pub enum DcpRequest {
    /// An Identify this device must answer with this ready-made frame.
    Identify { reply: Vec<u8> },
    /// A Set request: the decisions per block; the caller applies the effects and answers.
    Set { src: MacAddr, xid: u32, decisions: Vec<SetDecision> },
}

/// Classify one received frame: today's `handle_dcp_frame` logic, except that a Set
/// returns `DcpRequest::Set { src: eth.src, xid: header.xid, decisions: decide_set(&req) }`
/// instead of a response frame.
pub fn classify(frame: &[u8], cfg: &DcpConfig) -> Result<Option<DcpRequest>, DcpError>;

/// Handle one received frame without side effects: Identify → its reply; Set → the
/// response built from the decisions (effects not applied).
pub fn handle_dcp_frame(frame: &[u8], cfg: &DcpConfig) -> Result<Option<Vec<u8>>, DcpError> {
    Ok(match classify(frame, cfg)? {
        Some(DcpRequest::Identify { reply }) => Some(reply),
        Some(DcpRequest::Set { src, xid, decisions }) => {
            let results: Vec<_> = decisions.iter().map(SetDecision::tuple).collect();
            Some(build_set_response(src, cfg.mac, xid, &results))
        }
        None => None,
    })
}
```

`cm/mod.rs`: `pub fn im_store_mut(&mut self) -> &mut ImStore { &mut self.im }` (doc: "the I&M1-3 store, for ResetToFactory").

`device/mod.rs`:

- Imports: `use crate::dcp::{handle_dcp_frame, DcpConfig};` becomes `use crate::dcp::{build_set_response, classify, DcpConfig, DcpRequest, Effect, IpConfigurator, NetParams, NetStore, NoopIp, SetBlockError, SetDecision};`, plus `use crate::eth::MacAddr;`, `use crate::lldp::Peer;`, `use crate::pdev::{LinkProbe, Mismatch, MockLink, Pdev, PdevAction};`, and `AtomicU32` added to the `std::sync::atomic` import. The struct doc of `Device` names `classify` instead of `handle_dcp_frame`.
- `DeviceSetup` gains `pub net_store: Option<PathBuf>` ("backing file of the DCP-assigned identity; `None` = RAM only") and `pub iface: String` ("the interface an `IpConfigurator` reconfigures; empty when none applies"); the literal sites are in **Files**.
- `DeviceHooks` (Interfaces) and its `Default`: `DeviceHooks { ip: Box::new(NoopIp), link: Box::new(MockLink(AtomicBool::new(true))), lldp: None, on_signal: None }`. `DeviceShared` as in Interfaces.
- `Device` fields — the hooks are stored individually (`DeviceHooks` is destructured in `new_with`; `link` moves into the `Pdev`): `net: NetParams`, `net_store: NetStore`, `ip: Box<dyn IpConfigurator>`, `on_signal: Option<Arc<dyn Fn() + Send + Sync>>`, `lldp_eth: Option<Box<dyn EthTransport>>`, `shared: Arc<DeviceShared>`. The interface name is read from `self.setup.iface`.
- `new` = `Self::new_with(setup, eth, rpc, DeviceHooks::default(), Instant::now())`; `new_with`:

```rust
    pub fn new_with(setup: DeviceSetup, eth: E, rpc: R, hooks: DeviceHooks, now: Instant) -> Device<E, R> {
        let DeviceHooks { ip, link, lldp, on_signal } = hooks;
        let props = &setup.dcp.properties;
        let net = NetStore::new(setup.net_store.clone()).load().unwrap_or_else(|| NetParams {
            name: props.name_of_station.clone(),
            ip: props.ip,
            subnet: props.subnet,
            gateway: props.gateway,
        });
        let mut cm = Cm::new(setup.model.clone(), setup.activity_seed);
        cm.set_im(setup.im0.clone(), ImStore::load(setup.im_store.clone()));
        if lldp.is_some() {
            // No LLDP socket = no PDEV: PDEV records answer "invalid index" as before,
            // which keeps the pre-Plan 8 device tests byte-identical.
            cm.set_pdev(Some(Pdev::new(setup.dcp.mac.0, link, now)));
        }
        let diag = DiagStore::from_model(&setup.model).with_port(&setup.model);
        let net_store = NetStore::new(setup.net_store.clone());
        let mut dev = Device {
            setup,
            eth,
            rpc,
            cm,
            on_state_change: None,
            image: Arc::new(IoImage::empty()),
            stats: Arc::new(RtStats::default()),
            problem: Arc::new(AtomicBool::new(false)),
            diag,
            diag_shared: Arc::new(DiagShared::default()),
            alarm: None,
            next_alarm_id: 0,
            alarm_epoch: 0,
            net: NetParams::default(),
            net_store,
            ip,
            on_signal,
            lldp_eth: lldp,
            shared: Arc::new(DeviceShared::default()),
            #[cfg(target_os = "linux")]
            runner: None,
            #[cfg(target_os = "linux")]
            runner_factory: Box::new(RtRunner::spawn),
        };
        dev.apply_net(net, now);
        dev
    }
```

- `apply_net` — one arity, by value:

```rust
    /// Make `net` the identity everywhere it is answered: DCP Identify/Set, the next
    /// Connect's ARServerBlock, LLDP (an immediate LLDPDU), `DeviceShared`.
    fn apply_net(&mut self, net: NetParams, now: Instant) {
        let p = &mut self.setup.dcp.properties;
        p.name_of_station = net.name.clone();
        p.ip = net.ip;
        p.subnet = net.subnet;
        p.gateway = net.gateway;
        p.ip_block_info = if net.ip == [0; 4] { 0 } else { 1 };
        self.cm.set_station_name(&net.name);
        if let Some(pdev) = self.cm.pdev_mut() {
            pdev.set_identity(&net, now);
        }
        *self.shared.net.lock().unwrap_or_else(|e| e.into_inner()) = net.clone();
        self.net = net;
    }
```

- `step`: the deadline becomes `[self.cm.next_deadline(), alarm_deadline, self.cm.pdev().and_then(|p| p.next_deadline())].into_iter().flatten().min()`; the poll set gets `if let Some(fd) = self.lldp_eth.as_ref().and_then(|t| t.raw_fd()) { fds.push(fd); }`. Stage 1's DCP branch becomes

```rust
            match classify(&frame, &self.setup.dcp) {
                Ok(Some(DcpRequest::Identify { reply })) => self.eth.send(&reply)?,
                Ok(Some(DcpRequest::Set { src, xid, decisions })) => {
                    self.handle_set(src, xid, decisions, now, &mut pending, &mut report)?
                }
                Ok(None) => {}
                Err(e) => log::warn!("dropping unparsable DCP frame: {e}"),
            }
```

  new stage 1b (after stage 1):

```rust
        // (1b) LLDP: frames first (the transport borrow), then the PDEV actions.
        let mut lldp_frames = Vec::new();
        if let Some(t) = &self.lldp_eth {
            while let Some(f) = t.recv(Some(Duration::ZERO))? {
                lldp_frames.push(f);
            }
        }
        for f in lldp_frames {
            let actions = self.cm.pdev_mut().map(|p| p.on_rx(&f, now)).unwrap_or_default();
            self.apply_pdev_actions_in(actions, now, &mut pending);
        }
```

  and stage 5b, right after stage 5's `dispatch`: `let actions = self.cm.pdev_mut().map(|p| p.tick(now)).unwrap_or_default(); self.apply_pdev_actions_in(actions, now, &mut pending);`.
- PDEV actions — `_in` collects alarm batches into the step's `pending` (applied at stage 7, like every other alarm action); the `pub(crate)` wrapper applies them at once (same pattern as `abort_with_err_rta` / `abort_with_err_rta_in`):

```rust
    /// Apply PDEV actions outside a `step` (dispatch, shutdown, tests): LLDP frames go
    /// out, diag changes become alarms sent immediately.
    pub(crate) fn apply_pdev_actions(&mut self, actions: Vec<PdevAction>, now: Instant) -> Result<(), DeviceError> {
        let mut pending = Vec::new();
        self.apply_pdev_actions_in(actions, now, &mut pending);
        let mut report = StepReport::default();
        for (epoch, batch) in pending {
            self.apply_alarm_actions(batch, epoch, now, &mut report)?;
        }
        Ok(())
    }

    fn apply_pdev_actions_in(&mut self, actions: Vec<PdevAction>, now: Instant, pending: &mut Vec<(u64, Vec<AlarmAction>)>) {
        for action in actions {
            match action {
                PdevAction::Send(frame) => {
                    if let Some(t) = &self.lldp_eth {
                        if let Err(e) = t.send(&frame) {
                            log::warn!("LLDP send failed: {e}"); // never aborts the loop
                        }
                    }
                }
                PdevAction::Diag(m) => {
                    for n in self.diag.set_port_mismatch(m) {
                        let a = self.queue_notification(n, now);
                        if !a.is_empty() {
                            pending.push((self.alarm_epoch, a));
                        }
                    }
                    self.publish_diag_state();
                    *self.shared.port_mismatch.lock().unwrap_or_else(|e| e.into_inner()) = m;
                }
            }
        }
        *self.shared.peer.lock().unwrap_or_else(|e| e.into_inner()) = self.cm.pdev().and_then(|p| p.peer().cloned());
    }
```

- `handle_set` — effect order per spec §5.1: apply → persist → respond → abort → LLDPDU (the LLDPDU follows from `apply_net` → `Pdev::set_identity`, sent at stage 5b of the same step):

```rust
    fn handle_set(
        &mut self,
        src: MacAddr,
        xid: u32,
        decisions: Vec<SetDecision>,
        now: Instant,
        pending: &mut Vec<(u64, Vec<AlarmAction>)>,
        report: &mut StepReport,
    ) -> Result<(), DeviceError> {
        let mut results = Vec::with_capacity(decisions.len());
        let mut new_net = self.net.clone();
        let mut persist = false;
        let mut identity_changed = false;
        let mut reset: Option<(usize, u8)> = None; // (index in `results`, mode)
        for d in &decisions {
            let mut err = d.error;
            if let (SetBlockError::Ok, Some(effect)) = (d.error, &d.effect) {
                match effect {
                    Effect::SetName { name, permanent } => {
                        if *name != new_net.name {
                            new_net.name = name.clone();
                            identity_changed = true;
                        }
                        persist |= *permanent;
                    }
                    Effect::SetIp { ip, subnet, gateway, permanent } => {
                        let candidate = NetParams { name: new_net.name.clone(), ip: *ip, subnet: *subnet, gateway: *gateway };
                        match self.ip.apply(&self.setup.iface, &candidate) {
                            Ok(()) => {
                                if (candidate.ip, candidate.subnet, candidate.gateway) != (new_net.ip, new_net.subnet, new_net.gateway) {
                                    identity_changed = true;
                                }
                                new_net = candidate;
                                persist |= *permanent;
                            }
                            Err(e) => {
                                log::error!("applying IP suite: {e}");
                                err = SetBlockError::SetNotPossible;
                            }
                        }
                    }
                    Effect::Signal => {
                        self.shared.signal_count.fetch_add(1, Ordering::Relaxed);
                        log::info!("DCP Signal: flash requested");
                        if let Some(h) = &self.on_signal {
                            h();
                        }
                    }
                    Effect::Reset { mode } => reset = Some((results.len(), *mode)),
                }
            }
            results.push((d.option, d.suboption, err));
        }
        if let Some((i, mode)) = reset {
            let blank = NetParams::default();
            match self.ip.apply(&self.setup.iface, &blank) {
                Ok(()) => {
                    new_net = blank;
                    identity_changed = true;
                    persist = true;
                    let a = self.cm.pdev_mut().map(|p| p.clear_engineering()).unwrap_or_default();
                    self.apply_pdev_actions_in(a, now, pending);
                    if mode == 4 {
                        self.cm.im_store_mut().clear();
                    }
                }
                Err(e) => {
                    // spec §9: the reset is refused whole — nothing applied, nothing persisted.
                    log::error!("ResetToFactory: clearing the IP suite failed: {e}");
                    results[i].2 = SetBlockError::SetNotPossible;
                    reset = None;
                }
            }
        }
        if identity_changed {
            self.apply_net(new_net.clone(), now);
            if persist {
                self.net_store.save(&new_net);
            }
        }
        self.eth.send(&build_set_response(src, self.setup.dcp.mac, xid, &results))?;
        if identity_changed && self.cm.state() != ArState::Idle {
            let reason = if reset.is_some() { AbortReason::ResetToFactory } else { AbortReason::DcpSet };
            self.abort_with_err_rta_in(reason, now, report)?;
        }
        Ok(())
    }
```

  (A same-name Set leaves `identity_changed` false: no persist, no abort — spec §9.)
- `dispatch`: the `(ArState::Idle, Some(_))` arm also clears the engineering records: `let a = self.cm.pdev_mut().map(|p| p.clear_engineering()).unwrap_or_default(); self.apply_pdev_actions(a, now)?;` (`out.notify` is owned by the loop, so `self` is free); after the notify loop, `self.apply_pdev_actions(out.pdev, now)?;` applies the actions of a Check write (Task 10's `CmOutput.pdev`).
- `shutdown` — the TTL-0 LLDPDU and the engineering clear come **before** the `Idle` early return, so a stop without an AR still announces it (spec §4.2):

```rust
    pub fn shutdown(&mut self, now: Instant) -> Result<(), DeviceError> {
        if let (Some(t), Some(p)) = (&self.lldp_eth, self.cm.pdev()) {
            if let Err(e) = t.send(&p.shutdown_frame()) {
                log::warn!("LLDP shutdown PDU not sent: {e}");
            }
        }
        let a = self.cm.pdev_mut().map(|p| p.clear_engineering()).unwrap_or_default();
        self.apply_pdev_actions(a, now)?;
        if self.cm.state() == ArState::Idle {
            return Ok(());
        }
        self.abort_with_err_rta(AbortReason::Shutdown, now)
    }
```

- Accessors: `net_params()` = `self.net.clone()`; `signal_count()` = `self.shared.signal_count.load(Ordering::Relaxed)`; `peer()` = `self.cm.pdev().and_then(|p| p.peer().cloned())`; `port_mismatch()` = `self.diag.port_mismatch()`; `device_shared()` = `self.shared.clone()`; `cm()`/`cm_mut()` (`#[doc(hidden)]`, test/embedding hooks) return `&self.cm`/`&mut self.cm`.

- [ ] **Step 4: Run** `cargo test -p pnio device` → PASS; full `cargo test -p pnio` green; `cargo clippy --all-targets -- -D warnings` clean.

- [ ] **Step 5: Commit** — `feat(device): DCP Set effects (name/IP/signal/reset), NetStore, LLDP socket + pdev tick, remote-mismatch diag, shutdown LLDPDU`

---

### Task 13: `api` — `StartOptions`, hooks, accessors, example

**Files:**
- Modify: `crates/pnio/src/api.rs` (`StartOptions`, `start`, `start_with`, `start_inner`, `IoDevice` accessors; tests), `crates/pnio/tests/typed_replay.rs:156` (`start_with` call), `crates/pnio/examples/linux/typed_bringup.rs:201` and `crates/pnio/examples/linux/latency_probe.rs:100` (the only `StartOptions` literals). `config.rs` needs nothing here: `DeviceConfig::setup` already got `net_store: None, iface: String::new()` in Task 12, and `start_inner` overrides both.

**Interfaces:**
- Produces:

```rust
pub struct StartOptions { …, pub net_store: Option<PathBuf>, pub apply_ip: bool, pub lldp: bool, pub on_signal: Option<Arc<dyn Fn() + Send + Sync>> }
// `StartOptions` loses `derive(Debug, Clone)` unless `on_signal` is wrapped: implement Debug manually (print "<fn>" for the hook) and keep Clone (Arc clones).
impl IoDevice {
    pub fn net_params(&self) -> NetParams;
    pub fn signal_count(&self) -> u32;
    pub fn peer(&self) -> Option<Peer>;
    pub fn port_mismatch(&self) -> Option<Mismatch>;
    pub fn start_with<E, R>(cfg, mac, ip, rt, eth, rpc, runner, hooks: DeviceHooks) -> Result<IoDevice, ApiError>;   // signature change: one more parameter
}
```

- [ ] **Step 1: Failing test** (`api.rs` tests already have `sample()`, `const DEV: MacAddr`, and the FrameID-partitioned `SharedMock` with `SharedMock::new_pair() -> (acyclic, rt)`; the LLDP transport is a plain `MockTransport`, which the test never needs to read back, so no new newtype; add `use crate::dcp::MockIp; use crate::device::DeviceHooks; use crate::pdev::MockLink;`):

```rust
    #[test]
    fn facade_exposes_identity_signal_and_peer() {
        let (eth, _) = SharedMock::new_pair();
        let hooks = DeviceHooks {
            ip: Box::new(MockIp::default()),
            link: Box::new(MockLink(true.into())),
            lldp: Some(Box::new(MockTransport::new())),
            on_signal: None,
        };
        let dev = IoDevice::start_with(
            sample(),
            DEV,
            [172, 16, 2, 10],
            None,
            eth,
            MockRpcTransport::new(),
            |c| RtRunner::spawn_with_transport(c, MockTransport::new()),
            hooks,
        )
        .unwrap();
        assert_eq!(dev.net_params().name, sample().station_name());
        assert_eq!(dev.signal_count(), 0);
        assert_eq!(dev.peer(), None);
        assert_eq!(dev.port_mismatch(), None);
        dev.stop().unwrap();
    }
```

Update every existing `start_with` call site to pass `DeviceHooks::default()` as the new last argument: `api.rs:1033` and `api.rs:1077` (tests), `tests/typed_replay.rs:156`.

- [ ] **Step 2: Run** → compile errors.

- [ ] **Step 3: Implement.** `start`: after opening `eth`, `let lldp = if opts.lldp { Some(Box::new(AfPacketTransport::open_with(&opts.iface, EthProto::LLDP).map_err(io_err)?) as Box<dyn EthTransport>) } else { None };` build `DeviceHooks { ip: if opts.apply_ip { Box::new(IpCommand) } else { Box::new(NoopIp) }, link: Box::new(SysfsLink { iface: opts.iface.clone() }), lldp, on_signal: opts.on_signal.clone() }`. `start_inner` takes `hooks: DeviceHooks`, `net_store: Option<PathBuf>` and `iface: &str`, sets `setup.net_store = net_store` and `setup.iface = iface.to_string()` (`start` passes `&opts.iface` and `opts.net_store.clone()`, `start_with` passes `""` and `None`), and calls `Device::new_with(setup, eth, rpc, hooks, Instant::now())`. Keep `shared = dev.device_shared()` in `IoDevice` for the accessors. Doc every new field/method (`StartOptions` docs: `net_store` "None = RAM only, a warning is logged at start"; `apply_ip` "default true"; `lldp` "default true; false also disables PDEV record service"). No `impl Default for StartOptions` (`iface` has no default): construction stays explicit, and the two `StartOptions` literals — `examples/linux/latency_probe.rs:100` and `examples/linux/typed_bringup.rs:201` — get the four new fields (`latency_probe`: `net_store: None, apply_ip: false, lldp: false, on_signal: None`; `typed_bringup`: from its flags below). `rt_bringup.rs` builds a `DeviceSetup`, handled in Task 12.

`typed_bringup.rs`: `--net-store <PATH>` (`Option<PathBuf>`), `--no-apply-ip`, `--no-lldp` flags; status line adds `name=<net_params().name> peer=<chassis>/<port>|none mismatch=<…>`; on SIGINT nothing new. `gen_gsdml.rs` unchanged.

- [ ] **Step 4: Run** `cargo test -p pnio` (all), `cargo build -p pnio --examples`, `cargo check -p pnio --target x86_64-apple-darwin --all-targets`, `cargo doc -p pnio --no-deps` (0 warnings), `cargo clippy --all-targets -- -D warnings`.

- [ ] **Step 5: Commit** — `feat(api): StartOptions net_store/apply_ip/lldp/on_signal, DeviceHooks injection, net_params/signal_count/peer/port_mismatch; typed_bringup flags`

---

### Task 14: Integration replay, docs, follow-ups, changelog

**Prerequisite: Task 2 goldens; without them do only the docs steps** (Step 3, then commit the docs alone; Steps 1-2 run once Task 2 has produced `testdata/{lldp,pdev,dcp}/*.hex`).

**Files:**
- Create: `crates/pnio/tests/pdev_replay.rs`
- Modify: `crates/pnio/tests/common/mod.rs` (`golden_in` twin), `README.md` (Limitations, Status), `CHANGELOG.md` (Unreleased), `FOLLOWUPS.md` (Plan 8 section), `docs/gsdml.md` (H1 placeholder → filled at HIL), `docs/dcp-golden-frames.md` (link to the new document)

- [ ] **Step 1: Write `tests/pdev_replay.rs`** — drives `Device` (public API: `Device::new_with(.., now)`, `MockTransport`, `MockRpcTransport`, `DeviceHooks` with `MockIp`/`MockLink`) through: the four Plan 3 goldens to `Data`, then the Plan 8 `connect_write_check` golden wrapped in the RPC envelope → asserts the Write response has one `IODWriteResHeader` per record and all statuses `0`; then the CPU LLDPDU golden on the LLDP mock → `dev.peer()` is the CPU; then the `read_802a_req`/`read_f841_req` goldens → responses parse with `BlockHeader::read_all` and contain the CPU's chassis id; then the DCP `set_name_req` golden → `dev.net_params().name == "pnio-dev-b"`, AR `Idle`, one Set response with `Ok`; then `signal_req` → `signal_count() == 1`; then `reset_all_req` → `net_params().is_blank()`. One `#[test]` per phase, sharing helpers written in `tests/pdev_replay.rs` itself (`tests/ar_replay.rs` has no reusable helper — one test fn whose body inlines the setup — and `tests/common` has no RPC envelope builder):
  - `fn setup() -> DeviceSetup`: the `DeviceSetup` literal of `tests/ar_replay.rs:19` (p-net identity, `DeviceModel::pnet_sample(MAC)`, same activity seed), plus `net_store: None, iface: String::new()`.
  - `#[derive(Clone)] struct Shared(Arc<MockTransport>)` implementing `EthTransport` by delegating `send`/`recv_into` (the integration-test twin of `device/mod.rs`'s `SharedEth`: the test reads the transports back after `new_with` has taken them).
  - `fn device_in_data() -> (Device<Shared, MockRpcTransport>, Shared, Shared)` — `(device, PROFINET eth, LLDP eth)`, built with `DeviceHooks { ip: Box::new(MockIp::default()), link: Box::new(MockLink(true.into())), lldp: Some(Box::new(lldp.clone())), on_signal: None }`; push `connect_req`, `write_req`, `prmend_req` (from `172.16.2.100:54766`) and `appready_res` (from `172.16.2.100:34964`) — `common::golden(name)[RPC_OFF..]` — on the `MockRpcTransport` through `dev.rpc().push_rx(..)`, exactly as `device/mod.rs`'s `feed_bring_up` does, then one `step`, and assert `ArState::Data`.
  - `fn rpc_request(template: &[u8], blocks: &[u8], seq_num: u32) -> Vec<u8>`: the Task 10 `cm` test helper of the same name, rewritten against the public `pnio::rpc::{RpcHeader, NdrRequest}` API (parse the template header, set `seq_num` and `frag_len = NdrRequest::LEN + blocks.len()`, write header + `NdrRequest::for_blocks(args_max, len)` + blocks); a Write uses `common::golden("write_req")[RPC_OFF..]` as template, a Read `common::golden_alarm("im0_read_req")[RPC_OFF..]`.

- [ ] **Step 2: Run** `cargo test -p pnio --test pdev_replay` → PASS (fix any envelope/offset detail against `tests/common`).

- [ ] **Step 3: Docs**
  - `README.md`: Status gains "LLDP neighbourhood, PDEV records (`0x802A/B/F`, `0x8080`, `0xF841/2`), remote-mismatch diagnosis, DCP Set-Name/IP/Signal/ResetToFactory, GSDML V2.31"; Limitations: remove the DCP and `PNIO_Version V2.3` lines, add "no SNMP, no MRP, no PTP/IRT, single port".
  - `CHANGELOG.md` `[Unreleased]` → the Added/Changed/Fixed lists (Changed: `decide_set` signature, `build_write_res` signature, `start_with` signature, `Action::WriteAccepted`, non-OK I&M writes now reach the wire; Fixed: TIA's `0xF841/F842` probes no longer answer invalid index).
  - `FOLLOWUPS.md`: new "From Plan 8" section with the spec §2 Out list and anything found during implementation.
  - `docs/gsdml.md`: V2.31 section (Task 1) gains an "H1 outcome" line to fill at HIL.
  - `docs/dcp-golden-frames.md`: one line pointing to `docs/pdev-golden-frames.md` for the Set-Name/Signal/Reset goldens.

- [ ] **Step 4: Commit** — `test: pdev_replay integration; docs: README/CHANGELOG/FOLLOWUPS for Plan 8`

---

### Task 15: HIL §6j (bench, manual)

**Files:**
- Modify: `docs/bench-pnet-device.md` (new §6j), `docs/gsdml.md` (H1 outcome), `docs/pdev-golden-frames.md` (any value corrected on the bench)

- [ ] **Step 1: Build and deploy** — musl build as for Plan 5 (`cargo build --release --target x86_64-unknown-linux-musl --example typed_bringup`), `scp` to `~/bench/typed_bringup.plan8`, `setcap cap_net_raw,cap_net_admin,cap_sys_nice,cap_ipc_lock+ep`, run with `--net-store ~/bench/pnio-net --im-store ~/bench/pnio-im` plus the usual RT flags. GSD from Task 1 already installed; topology configured.

- [ ] **Step 2: Run the nine checks of spec §6** in order, `tcpdump` per check into `captures/plan8-hil-<date>/`. For each: expected observation (spec §6), actual, PASS/FAIL. Check 3 (I&M1-3) decides H1 — if FAIL, apply the fallback (fields set? `V2.35`? capture) and record.

- [ ] **Step 3: Write §6j** in `docs/bench-pnet-device.md` (same structure as §6i: setup, table of checks, findings, captures list), update `docs/gsdml.md` with the H1 outcome, correct `docs/pdev-golden-frames.md` if the bench contradicted a pinned value (and fix the code/test, with its own commit).

- [ ] **Step 4: Commit** — `docs(bench): §6j HIL Plan 8 — PDEV/LLDP/DCP on the 1515 X1 (<n>/9 checks)`

If a check fails for a code reason, fix it in its own task-sized commit (test first) before closing the HIL.

---

### Task 16: Release 0.2.0

**Files:**
- Modify: `crates/pnio/src/lib.rs` (remove `version()`, `doc(hidden)` on internal modules), `crates/pnio/src/api.rs` (`Device::run_with` use), `crates/pnio/src/device/mod.rs` (`run_with`), `crates/pnio/src/config.rs` tests (`TooLongTotal` mixed case), `crates/pnio/Cargo.toml` (version), `CHANGELOG.md`, `README.md`

- [ ] **Step 1: `Device::run_with`**

Test (device tests): `run_with(stop, |dev| …)` is invoked once per step with `&Device`; a stop set from inside the callback ends the loop.

```rust
    /// Like [`Device::run`], calling `after_step(self)` after every iteration (the
    /// facade uses it to mirror `ar_params()` without duplicating the loop).
    pub fn run_with(&mut self, stop: &AtomicBool, mut after_step: impl FnMut(&Device<E, R>)) -> Result<(), DeviceError> {
        while !stop.load(Ordering::Relaxed) {
            let wait = self.poll_interval(Duration::from_millis(200));
            self.step(Instant::now(), Some(wait))?;
            after_step(self);
        }
        self.shutdown(Instant::now())
    }
```

(`run` becomes `self.run_with(stop, |_| {})`.) `api.rs`: delete `run_publishing_params`, call `dev.run_with(&stop, |d| { *params.lock()… = d.ar_params(); })` — read the current `run_publishing_params` first and keep its exact publishing semantics (it may publish only on state change).

- [ ] **Step 2: `TooLongTotal` mixed test** in `config.rs`: a config with one submodule having both directions whose summed C-SDU exceeds the cap → `check_total_csdu` returns `Err(ConfigError::TooLongTotal { .. })`. Read `check_total_csdu` and `cr_lengths` to pick lengths that only overflow through the `has_in && has_out` branch.

- [ ] **Step 3: API surface** — `#[doc(hidden)]` on the implementation-detail modules of `lib.rs`. Default (the `pub mod` list of `lib.rs` after Plan 8: `alarm`, `api`, `capture`, `cm`, `config`, `data`, `dcp`, `device`, `diag`, `eth`, `gsdml`, `im`, `lldp`, `pdev`, `rpc`, `rt`): everything is hidden except `api`, `config`, `data`, `gsdml`, `im`, `diag`, `dcp`, `lldp`, `pdev`, `capture` — i.e. `#[doc(hidden)]` on `pub mod alarm`, `cm`, `device`, `eth`, `rpc`, `rt` (they stay `pub`: integration tests and examples use them). This is the default; the final call is made here, after `cargo doc -p pnio --no-deps` shows what the visible modules' signatures link to. Remove `pub fn version()` and its test (`CARGO_PKG_VERSION` is available to users through Cargo). Note each removal in CHANGELOG "Removed".

- [ ] **Step 4: Version and changelog** — `Cargo.toml` `version = "0.2.0"`, CHANGELOG `## [0.2.0] — <date>` with the Unreleased content, README badges unchanged. `cargo publish --dry-run -p pnio` passes. `cargo test`, clippy, doc, darwin check all green.

- [ ] **Step 5: Merge and tag**

```bash
git checkout main && git merge --ff-only feat/pdev-lldp-dcp && git push origin main
git tag -a v0.2.0 -m "pnio 0.2.0: PDEV/LLDP, DCP Set/Signal/Reset, GSDML V2.31" && git push origin v0.2.0
```

`cargo publish -p pnio` is the user's call (ask before running it). Then `git worktree remove .worktrees/feat-pdev-lldp-dcp`, delete the local branch, and update `FOLLOWUPS.md`'s head note.

---

## Self-review notes (done while writing)

- Spec coverage: §5.1 → Tasks 3-5, 12; §5.2 → 6; §5.3 → 7; §5.4 → 8-9; §5.5 → 10; §5.6 → 11; §5.7 → 12; §5.8 → 13; §5.9 → 1; §6 → 15; §7 → tests in each task + 14; §8 → 2; §9 edge cases: IP apply failure (12), NetStore failure (5), same-name set (12: `identity_changed` false → no abort, no persist), peer flapping (8: last PDU wins, by construction), `NumberOfPeers = 0` (8), unknown subslot (10), `0xF841` before engineering (9); §11 → 16.
- Type consistency: `SetDecision`/`Effect` (4) used in 12; `NetParams` (4) in 5, 8, 9, 12, 13; `PdevAction`/`Mismatch`/`CheckRecord`/`ExpectedPeer` (8) in 9-12; `Peer` (7) in 8, 12, 13; `ETHERTYPE_LLDP`/`LLDP_MULTICAST_MAC` (6) in 7; `Pdev::mac()`/`net()` (8) in 9; `DeviceHooks`, `Device::new_with(.., now)` (12) in 13, 14; `build_write_res(req, statuses)` (10) in 14; `Action::WriteAccepted { req }` (10) only inside `cm`; test helpers `testutil::{dcp_set_frame (4), im1_record (5), synthetic_check_record / synthetic_write_record / synthetic_read_req (9)}` in 9, 10, 12.
- Placeholders: none left in plan code — Task 12's `set_frame` delegates to `testutil::dcp_set_frame`, written out in Task 4. Wire values (BlockTypes, padding, ResetToFactory qualifier, PMD bytes, ext error types, Port ID form) come from the provisional `docs/pdev-golden-frames.md` (commit `6c2ba3a`); Task 2 confirms or overrides them, and the synthetic test frames built from them get golden twins.
- Pre-flight scan (2026-10-10, 50 findings) applied per the controller's rulings; see "Deviations from spec (ruled 2026-10-10)".
- Review Focus items: #1 → Task 12 `set_name_and_ip_in_one_request…`; #2 → Task 8 `two_expected_peers…`; #3 → Task 7 `foreign_switch_pdu…`; #4 → Task 5 `…crlf_loads`; #5 → Task 3 (240/241).

# PROFINET-RT Close-out & OSS Polish Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Close the remaining non-capture review follow-ups and make the now-public repository more professional: typed transport errors + documented `recv` contract, an end-to-end DCP replay test, and crates.io/OSS metadata.

**Architecture:** Three independent tasks touching disjoint areas — `eth` error type, a new integration test, and project metadata/docs. No protocol behavior changes.

**Tech Stack:** Rust (stable), `thiserror`, `nix` 0.27, `pcap-file` 2 (a normal dependency, available to integration tests).

## Global Constraints

- Pure Rust, no new dependencies. Big-endian preserved. rustfmt `max_width = 100`; `cargo clippy --all-targets -- -D warnings` clean.
- All cargo commands prefixed: `. "$HOME/.cargo/env" && cargo ...`.
- No behavior regression: the existing 44 unit + 1 integration tests must stay green; golden-frame byte-exact tests are sacred.
- English only (the repo is now English).
- Branch: `feat/oss-polish`, base `main`.

---

### Task 1: Typed `TransportError` + documented `recv` contract

**Files:**
- Modify: `crates/profinet-rt/src/eth/transport.rs`
- Modify: `crates/profinet-rt/src/eth/afpacket.rs`

**Goal:** `TransportError::Io` wraps a typed `std::io::Error` (consistency with `CaptureError`), and the `EthTransport::recv` trait doc enumerates the legitimate `Ok(None)` cases.

> **nix API note (verify against installed nix 0.27, do not guess):** the AF_PACKET backend wraps `nix::errno::Errno` (from `socket`/`if_nametoindex`/`send`/`recv`) and one `std::io::Error` (from `bind`). Confirm whether `std::io::Error: From<nix::errno::Errno>` exists in nix 0.27 (it does in recent nix). If yes, convert via `std::io::Error::from(errno)`. If not, use `std::io::Error::from_raw_os_error(errno as i32)`. Adapt the code below to what compiles.

- [ ] **Step 1: Update `transport.rs` — typed error + recv doc**

Replace the `TransportError` enum and the trait's `recv` doc:
```rust
#[derive(Debug, Error)]
pub enum TransportError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Raw Ethernet frame I/O abstraction (L2 header included).
pub trait EthTransport: Send + Sync {
    fn send(&self, frame: &[u8]) -> Result<(), TransportError>;

    /// Receive the next frame.
    ///
    /// Returns `Ok(None)` in three legitimate, non-error cases:
    /// - the queue is empty (e.g. `MockTransport` with nothing pushed);
    /// - no frame arrived before `timeout` elapsed;
    /// - the backend filters non-PROFINET traffic and the next frame on the wire
    ///   was not PROFINET (e.g. `AfPacketTransport`).
    ///
    /// A receive loop should treat `Ok(None)` as "nothing for me right now" and
    /// continue, distinct from `Err(_)` which is a real I/O failure.
    fn recv(&self, timeout: Option<Duration>) -> Result<Option<Vec<u8>>, TransportError>;
}
```

- [ ] **Step 2: Update `afpacket.rs` — convert nix/io errors into the typed variant**

Replace the `io_err` helper with `From` conversions and use `?`. Add (near the top, after the `use` lines):
```rust
impl From<nix::errno::Errno> for TransportError {
    fn from(e: nix::errno::Errno) -> Self {
        TransportError::Io(std::io::Error::from(e))
    }
}
```
Then remove `fn io_err(...)` and change the call sites:
- `socket(...).map_err(io_err)?` → `socket(...)?`
- `nix::net::if_::if_nametoindex(ifname).map_err(io_err)?` → `...if_nametoindex(ifname)?`
- in `send`: `send(...).map_err(io_err)?` → `send(...)?`
- in `recv`: `recv(...).map_err(io_err)?` → `recv(...)?`
- the `bind` failure path already builds a `std::io::Error`:
  `return Err(io_err(std::io::Error::last_os_error()));` →
  `return Err(TransportError::Io(std::io::Error::last_os_error()));`

(If `std::io::Error: From<nix::errno::Errno>` is NOT available in this nix, implement the `From<Errno>` body with `std::io::Error::from_raw_os_error(e as i32)` instead.)

- [ ] **Step 3: Run tests + lint**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt eth 2>&1 | tail -20 && cargo clippy --all-targets -- -D warnings 2>&1 | tail -5`
Expected: all eth tests pass (incl. `open_unknown_interface_errors`, which only checks `is_err()`), no warnings. Run the full suite too: `cargo test -p profinet-rt 2>&1 | tail -6`.

- [ ] **Step 4: Commit**

```bash
git add crates/profinet-rt/src/eth/transport.rs crates/profinet-rt/src/eth/afpacket.rs
git commit -m "refactor(eth): typed TransportError::Io(io::Error) + document recv Ok(None) contract"
```

---

### Task 2: End-to-end DCP replay integration test

**Files:**
- Create: `crates/profinet-rt/tests/dcp_replay.rs`

**Goal:** Prove the whole device pipeline works on a capture: build an in-memory **pcapng** holding the real golden Identify REQUEST, read it via `PcapFrames`, feed each frame to `dcp::handle_dcp_frame`, and assert the produced frame equals the golden Identify RESPONSE. Exercises `capture` (pcapng) + `eth` parsing + `dcp` dispatch together.

> Reuse the **exact working pcapng writer pattern** already proven in `src/capture.rs`'s `#[cfg(test)]` module (`make_pcapng`: SectionHeader + InterfaceDescription with Ethernet link type + one EnhancedPacket per frame). Read that module first and mirror it; `pcap-file` is a normal dependency, so it is usable from this integration test.

- [ ] **Step 1: Write the integration test**

Create `crates/profinet-rt/tests/dcp_replay.rs`:
```rust
//! End-to-end: replay a pcapng holding a real DCP Identify request through the
//! capture -> eth -> dcp pipeline and check the device produces the golden response.

use profinet_rt::capture::PcapFrames;
use profinet_rt::dcp::{handle_dcp_frame, DeviceConfig, DeviceProperties};
use profinet_rt::eth::MacAddr;
use std::io::Cursor;

// Golden Identify REQUEST frame (56 bytes), docs/dcp-golden-frames.md.
const REQ_FRAME: &[u8] = &[
    0x01, 0x0e, 0xcf, 0x00, 0x00, 0x00, 0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f, 0x88, 0x92, 0xfe,
    0xfe, 0x05, 0x00, 0x03, 0x00, 0x01, 0x52, 0x00, 0x01, 0x00, 0x0c, 0x02, 0x02, 0x00, 0x08,
    0x69, 0x2d, 0x64, 0x65, 0x76, 0x69, 0x63, 0x65, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

// Golden Identify RESPONSE frame (114 bytes), docs/dcp-golden-frames.md.
const RESP_FRAME: &[u8] = &[
    0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f, 0x02, 0xc0, 0xa8, 0x01, 0x0f, 0x02, 0x88, 0x92, 0xfe,
    0xff, 0x05, 0x01, 0x03, 0x00, 0x01, 0x52, 0x00, 0x00, 0x00, 0x58, 0x02, 0x02, 0x00, 0x0a,
    0x00, 0x00, 0x69, 0x2d, 0x64, 0x65, 0x76, 0x69, 0x63, 0x65, 0x02, 0x05, 0x00, 0x04, 0x00,
    0x00, 0x02, 0x07, 0x02, 0x01, 0x00, 0x12, 0x00, 0x00, 0x53, 0x37, 0x2d, 0x31, 0x35, 0x30,
    0x30, 0x20, 0x28, 0x50, 0x4c, 0x43, 0x53, 0x49, 0x4d, 0x29, 0x02, 0x03, 0x00, 0x06, 0x00,
    0x00, 0x00, 0x2a, 0x01, 0x0e, 0x02, 0x04, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x02, 0x07,
    0x00, 0x04, 0x00, 0x00, 0x10, 0x64, 0x01, 0x02, 0x00, 0x0e, 0x00, 0x01, 0xc0, 0xa8, 0x01,
    0x3d, 0xff, 0xff, 0xff, 0x00, 0xc0, 0xa8, 0x01, 0x3d,
];

// Build an in-memory pcapng holding the given Ethernet frames.
// MIRROR the working helper in src/capture.rs's test module (PcapNgWriter:
// SectionHeader + InterfaceDescription{ DataLink::ETHERNET } + EnhancedPacket per frame).
fn make_pcapng(frames: &[&[u8]]) -> Vec<u8> {
    // ... implement using pcap_file::pcapng, mirroring src/capture.rs tests ...
    todo!("mirror make_pcapng from src/capture.rs tests")
}

fn device_cfg() -> DeviceConfig {
    DeviceConfig {
        mac: MacAddr([0x02, 0xc0, 0xa8, 0x01, 0x0f, 0x02]),
        properties: DeviceProperties {
            name_of_station: "i-device".to_string(),
            type_of_station: "S7-1500 (PLCSIM)".to_string(),
            vendor_id: 0x002a,
            device_id: 0x010e,
            device_role: 0x0000,
            device_instance: 0x1064,
            device_options: vec![0x02, 0x07],
            ip: [192, 168, 1, 61],
            subnet: [255, 255, 255, 0],
            gateway: [192, 168, 1, 61],
            ip_block_info: 0x0001,
        },
    }
}

#[test]
fn replay_identify_request_produces_golden_response() {
    let pcapng = make_pcapng(&[REQ_FRAME]);
    let cfg = device_cfg();

    let mut responses = Vec::new();
    for frame in PcapFrames::from_reader(Cursor::new(pcapng)).unwrap() {
        let frame = frame.expect("frame read");
        if let Some(resp) = handle_dcp_frame(&frame, &cfg).unwrap() {
            responses.push(resp);
        }
    }

    assert_eq!(responses.len(), 1, "exactly one Identify response expected");
    assert_eq!(responses[0], RESP_FRAME, "response must be byte-exact golden");
}
```

Replace the `todo!()` in `make_pcapng` with a real implementation copied/adapted from `src/capture.rs`'s test module (it already builds a valid pcapng the reader accepts).

- [ ] **Step 2: Run, verify it passes**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt --test dcp_replay 2>&1 | tail -20`
Expected: PASS (`replay_identify_request_produces_golden_response`). If the pcapng build is wrong, the reader yields no frames or the assert fails — fix `make_pcapng` against the proven pattern.

- [ ] **Step 3: Full suite + lint**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt 2>&1 | tail -6 && cargo clippy --all-targets -- -D warnings 2>&1 | tail -5`
Expected: all green, no warnings.

- [ ] **Step 4: Commit**

```bash
git add crates/profinet-rt/tests/dcp_replay.rs
git commit -m "test(dcp): end-to-end pcapng replay -> Identify response golden check"
```

---

### Task 3: crates.io / OSS readiness

**Files:**
- Modify: `crates/profinet-rt/Cargo.toml`
- Modify: `crates/profinet-rt/src/lib.rs`
- Create: `CONTRIBUTING.md`

- [ ] **Step 1: Crate metadata**

In `crates/profinet-rt/Cargo.toml`, add to `[package]` (after `repository.workspace = true`):
```toml
keywords = ["profinet", "industrial", "real-time", "ethernet", "plc"]
categories = ["network-programming", "embedded"]
readme = "../../README.md"
```

- [ ] **Step 2: Crate-level usage doctest**

In `crates/profinet-rt/src/lib.rs`, extend the crate doc with a runnable example (a doctest — `cargo test` compiles and runs it). Append to the `//!` block:
```rust
//!
//! # Example: answer a DCP Identify on the device side
//!
//! ```
//! use profinet_rt::dcp::{handle_dcp_frame, DeviceConfig, DeviceProperties};
//! use profinet_rt::eth::MacAddr;
//!
//! let cfg = DeviceConfig {
//!     mac: MacAddr([0x02, 0xc0, 0xa8, 0x01, 0x0f, 0x02]),
//!     properties: DeviceProperties {
//!         name_of_station: "i-device".to_string(),
//!         type_of_station: "demo".to_string(),
//!         vendor_id: 0x002a,
//!         device_id: 0x010e,
//!         device_role: 0,
//!         device_instance: 0x1064,
//!         device_options: vec![0x02, 0x07],
//!         ip: [192, 168, 1, 61],
//!         subnet: [255, 255, 255, 0],
//!         gateway: [192, 168, 1, 61],
//!         ip_block_info: 1,
//!     },
//! };
//!
//! // A DCP Identify request for "i-device" produces an Identify response.
//! let req = [
//!     0x01, 0x0e, 0xcf, 0x00, 0x00, 0x00, 0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f, 0x88, 0x92,
//!     0xfe, 0xfe, 0x05, 0x00, 0x03, 0x00, 0x01, 0x52, 0x00, 0x01, 0x00, 0x0c, 0x02, 0x02,
//!     0x00, 0x08, 0x69, 0x2d, 0x64, 0x65, 0x76, 0x69, 0x63, 0x65,
//! ];
//! let resp = handle_dcp_frame(&req, &cfg).unwrap();
//! assert!(resp.is_some());
//! ```
```
Verify the doctest compiles and runs (it uses only the public API). If any path/type differs, fix the example (not the library).

- [ ] **Step 3: CONTRIBUTING.md**

Create `CONTRIBUTING.md`:
```markdown
# Contributing

Thanks for your interest in `profinet-rt`.

## Development

```bash
cargo test                                   # unit + integration tests
cargo fmt --all                              # format (max_width = 100)
cargo clippy --all-targets -- -D warnings    # lint (warnings are errors)
```

All three must be clean before a PR.

## Guidelines

- **Pure Rust**, no bundled third-party C stack; no GPL/copyleft code.
- Wire-format code is validated **byte-exact** against real captures
  (see [`docs/dcp-golden-frames.md`](docs/dcp-golden-frames.md)); add a test vector
  with any new frame type.
- No IEC standard text copied into the repo — paraphrase only.
- Keep big-endian on the wire.

## License

By contributing, you agree your contribution is dual-licensed under
**MIT OR Apache-2.0**, matching the project.
```

- [ ] **Step 4: Build, test (incl. doctest), lint**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt 2>&1 | tail -8 && cargo clippy --all-targets -- -D warnings 2>&1 | tail -5`
Expected: unit + integration + **doctest** all pass (look for the `Doc-tests profinet_rt` line reporting 1 passed), no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/profinet-rt/Cargo.toml crates/profinet-rt/src/lib.rs CONTRIBUTING.md
git commit -m "chore: crates.io metadata, usage doctest, CONTRIBUTING"
```

---

## Notes for the executor
- Task 1 touches the `nix` error API — verify against the installed crate and adapt the `From<Errno>` conversion.
- Task 2 must mirror the proven `make_pcapng` from `src/capture.rs` tests — read it first.
- After all three: whole-branch review, then `superpowers:finishing-a-development-branch` (merge to `main` + push).

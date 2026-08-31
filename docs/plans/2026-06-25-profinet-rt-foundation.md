# PROFINET RT — Plan 1: Foundations (scaffold + `eth` layer + golden-frames harness)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Lay the foundations of the `profinet-rt` crate: workspace + CI, the Layer-2 Ethernet frame I/O abstraction (mockable trait + `AF_PACKET` backend), and a capture/replay harness for testing upper layers against Wireshark "golden" frames.

**Architecture:** Pure Rust crate, no bundled third-party C stack. The `eth` layer exposes an `EthTransport` trait (send/recv of raw frames with EtherType `0x8892`) with two implementations: `MockTransport` (in-memory queue, for tests) and `AfPacketTransport` (Linux raw socket). A `capture` module reads `.pcap` files (pure-Rust parser) to replay real exchanges in tests. This plan depends on neither the IEC standard nor a PLC.

**Tech Stack:** Rust 2021 (rust-version ≥ 1.74), `nix` (AF_PACKET socket), `thiserror` (errors), `pcap-file` (dev-dependency, pure-Rust pcap parsing). CI: `cargo test` + `cargo clippy -D warnings` + `cargo fmt --check`.

## Global Constraints

- **100% native Rust** — no bundled third-party C stack/dependency (strict prohibition of `p-net` code or any GPL stack). Syscall bindings (`nix`/`libc`) are allowed: they are kernel calls, not an embedded C stack.
- **Dual license MIT OR Apache-2.0** — `LICENSE-MIT` and `LICENSE-APACHE` files present, field `license = "MIT OR Apache-2.0"` in every `Cargo.toml`.
- **Trademark** — "PROFINET" used in descriptive context only; the README contains a PI non-affiliation disclaimer. No logo, no "certified".
- **No IEC standard text** copied into code or comments (paraphrase only).
- **Platform** — Linux (Debian PREEMPT_RT target); the `AF_PACKET` backend is `#[cfg(target_os = "linux")]`.
- **EtherType PROFINET** = `0x8892`. **EtherType VLAN** = `0x8100`.

---

### Task 1: Workspace scaffold, CI, licenses, disclaimer

**Files:**
- Create: `Cargo.toml` (workspace root)
- Create: `crates/profinet-rt/Cargo.toml`
- Create: `crates/profinet-rt/src/lib.rs`
- Create: `LICENSE-MIT`, `LICENSE-APACHE`
- Create: `README.md`
- Create: `rustfmt.toml`
- Create: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: (nothing)
- Produces: compilable `profinet-rt` crate, smoke function `pub fn version() -> &'static str`.

- [ ] **Step 1: Write the smoke test**

In `crates/profinet-rt/src/lib.rs`:

```rust
//! `profinet-rt` — pure-Rust PROFINET RT IO-Device stack.
//!
//! Community project, NOT affiliated with / endorsed by PROFIBUS & PROFINET
//! International. "PROFINET" is a registered trademark of PNO.

/// Crate version (foundations smoke test).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_not_empty() {
        assert!(!version().is_empty());
    }
}
```

- [ ] **Step 2: Create manifest and support files**

`Cargo.toml` (root):

```toml
[workspace]
members = ["crates/profinet-rt"]
resolver = "2"

[workspace.package]
edition = "2021"
rust-version = "1.74"
license = "MIT OR Apache-2.0"
repository = "https://github.com/core-engineering/profinet-rt"
```

`crates/profinet-rt/Cargo.toml`:

```toml
[package]
name = "profinet-rt"
version = "0.0.0"
description = "PROFINET RT IO-Device stack in pure Rust (community, not affiliated with PI)"
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
nix = { version = "0.27", default-features = false, features = ["net", "socket"] }
thiserror = "1"

[dev-dependencies]
pcap-file = "2"
```

`rustfmt.toml`:

```toml
max_width = 100
```

`README.md` (minimum):

```markdown
# profinet-rt

Pure-Rust **PROFINET RT Class 1 / CC-A IO-Device** stack for Linux PREEMPT_RT.

> **Disclaimer.** Community project, **not affiliated with, endorsed, or certified by**
> PROFIBUS & PROFINET International (PI). "PROFINET" is a registered trademark of PNO.
> This library is a clean-room implementation from the public standard
> IEC 61158/61784. No normative text is reproduced herein.

## License

Dual-licensed, your choice: [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE).
```

`.github/workflows/ci.yml`:

```yaml
name: CI
on: [push, pull_request]
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy, rustfmt
      - run: cargo fmt --all --check
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo test --all
```

Fetch the standard MIT and Apache-2.0 texts into `LICENSE-MIT` / `LICENSE-APACHE`.

- [ ] **Step 3: Verify the project compiles and the test passes**

Run: `cargo test --all`
Expected: PASS (`version_is_not_empty`), 0 warning.

- [ ] **Step 4: Verify lint + format**

Run: `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings`
Expected: no error output.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: scaffold workspace, CI, licences MIT/Apache, disclaimer PI"
```

---

### Task 2: Ethernet header parsing/serialization (+ VLAN)

**Files:**
- Create: `crates/profinet-rt/src/eth/mod.rs`
- Create: `crates/profinet-rt/src/eth/header.rs`
- Modify: `crates/profinet-rt/src/lib.rs` (add `pub mod eth;`)

**Interfaces:**
- Consumes: (nothing)
- Produces:
  - `pub struct MacAddr(pub [u8; 6])`
  - `pub struct EthHeader { pub dst: MacAddr, pub src: MacAddr, pub vlan: Option<u16>, pub ethertype: u16 }`
  - `pub fn EthHeader::parse(buf: &[u8]) -> Result<(EthHeader, usize), EthError>` (returns the header + payload offset)
  - `pub fn EthHeader::write(&self, out: &mut Vec<u8>)`
  - `pub enum EthError { TooShort }` (via `thiserror`)
  - constants `pub const ETHERTYPE_PROFINET: u16 = 0x8892;` `pub const ETHERTYPE_VLAN: u16 = 0x8100;`

- [ ] **Step 1: Write tests (parse without VLAN, parse with VLAN, round-trip, too short)**

In `crates/profinet-rt/src/eth/header.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // dst=01:0e:cf:00:00:00, src=00:11:22:33:44:55, ethertype=0x8892, payload=[0xfe,0xfe]
    const FRAME_NO_VLAN: [u8; 16] = [
        0x01, 0x0e, 0xcf, 0x00, 0x00, 0x00, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x88, 0x92,
        0xfe, 0xfe,
    ];

    // same frame with VLAN tag 0x8100, TCI=0xE000 (prio 7), before the ethertype
    const FRAME_VLAN: [u8; 20] = [
        0x01, 0x0e, 0xcf, 0x00, 0x00, 0x00, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x81, 0x00,
        0xe0, 0x00, 0x88, 0x92, 0xfe, 0xfe,
    ];

    #[test]
    fn parse_without_vlan() {
        let (h, off) = EthHeader::parse(&FRAME_NO_VLAN).unwrap();
        assert_eq!(h.dst, MacAddr([0x01, 0x0e, 0xcf, 0x00, 0x00, 0x00]));
        assert_eq!(h.src, MacAddr([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]));
        assert_eq!(h.vlan, None);
        assert_eq!(h.ethertype, ETHERTYPE_PROFINET);
        assert_eq!(off, 14);
        assert_eq!(&FRAME_NO_VLAN[off..], &[0xfe, 0xfe]);
    }

    #[test]
    fn parse_with_vlan() {
        let (h, off) = EthHeader::parse(&FRAME_VLAN).unwrap();
        assert_eq!(h.vlan, Some(0xe000));
        assert_eq!(h.ethertype, ETHERTYPE_PROFINET);
        assert_eq!(off, 18);
    }

    #[test]
    fn round_trip_no_vlan() {
        let (h, _) = EthHeader::parse(&FRAME_NO_VLAN).unwrap();
        let mut out = Vec::new();
        h.write(&mut out);
        assert_eq!(out, &FRAME_NO_VLAN[..14]);
    }

    #[test]
    fn too_short_is_error() {
        assert!(matches!(EthHeader::parse(&[0u8; 8]), Err(EthError::TooShort)));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p profinet-rt eth::header -v`
Expected: FAIL (compilation: `EthHeader` / `MacAddr` not defined).

- [ ] **Step 3: Implement the header**

At the top of `crates/profinet-rt/src/eth/header.rs`:

```rust
use thiserror::Error;

pub const ETHERTYPE_PROFINET: u16 = 0x8892;
pub const ETHERTYPE_VLAN: u16 = 0x8100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacAddr(pub [u8; 6]);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EthHeader {
    pub dst: MacAddr,
    pub src: MacAddr,
    pub vlan: Option<u16>,
    pub ethertype: u16,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EthError {
    #[error("frame too short")]
    TooShort,
}

impl EthHeader {
    /// Parses the L2 header; returns (header, payload offset).
    pub fn parse(buf: &[u8]) -> Result<(Self, usize), EthError> {
        if buf.len() < 14 {
            return Err(EthError::TooShort);
        }
        let mut dst = [0u8; 6];
        let mut src = [0u8; 6];
        dst.copy_from_slice(&buf[0..6]);
        src.copy_from_slice(&buf[6..12]);

        let first = u16::from_be_bytes([buf[12], buf[13]]);
        let (vlan, ethertype, off) = if first == ETHERTYPE_VLAN {
            if buf.len() < 18 {
                return Err(EthError::TooShort);
            }
            let tci = u16::from_be_bytes([buf[14], buf[15]]);
            let et = u16::from_be_bytes([buf[16], buf[17]]);
            (Some(tci), et, 18)
        } else {
            (None, first, 14)
        };

        Ok((Self { dst: MacAddr(dst), src: MacAddr(src), vlan, ethertype }, off))
    }

    /// Serializes the header (without payload) into `out`.
    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.dst.0);
        out.extend_from_slice(&self.src.0);
        if let Some(tci) = self.vlan {
            out.extend_from_slice(&ETHERTYPE_VLAN.to_be_bytes());
            out.extend_from_slice(&tci.to_be_bytes());
        }
        out.extend_from_slice(&self.ethertype.to_be_bytes());
    }
}
```

`crates/profinet-rt/src/eth/mod.rs`:

```rust
mod header;
pub use header::{EthError, EthHeader, MacAddr, ETHERTYPE_PROFINET, ETHERTYPE_VLAN};
```

Add to `crates/profinet-rt/src/lib.rs`: `pub mod eth;`

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p profinet-rt eth::header -v`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(eth): parsing/serialisation en-tete Ethernet + VLAN"
```

---

### Task 3: `EthTransport` trait + `MockTransport`

**Files:**
- Create: `crates/profinet-rt/src/eth/transport.rs`
- Modify: `crates/profinet-rt/src/eth/mod.rs` (exports)

**Interfaces:**
- Consumes: `MacAddr` (Task 2)
- Produces:
  - `pub trait EthTransport { fn send(&self, frame: &[u8]) -> Result<(), TransportError>; fn recv(&self, timeout: Option<Duration>) -> Result<Option<Vec<u8>>, TransportError>; }`
  - `pub struct MockTransport` with `pub fn new() -> Self`, `pub fn push_rx(&self, frame: Vec<u8>)`, `pub fn sent(&self) -> Vec<Vec<u8>>`
  - `pub enum TransportError` (via `thiserror`)

- [ ] **Step 1: Write the mock tests**

In `crates/profinet-rt/src/eth/transport.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_records_sent_frames() {
        let t = MockTransport::new();
        t.send(&[1, 2, 3]).unwrap();
        t.send(&[4, 5]).unwrap();
        assert_eq!(t.sent(), vec![vec![1, 2, 3], vec![4, 5]]);
    }

    #[test]
    fn mock_returns_pushed_rx_in_order_then_none() {
        let t = MockTransport::new();
        t.push_rx(vec![9, 9]);
        assert_eq!(t.recv(None).unwrap(), Some(vec![9, 9]));
        assert_eq!(t.recv(None).unwrap(), None);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p profinet-rt eth::transport -v`
Expected: FAIL (compilation: `MockTransport` not defined).

- [ ] **Step 3: Implement the trait and the mock**

At the top of `crates/profinet-rt/src/eth/transport.rs`:

```rust
use std::sync::Mutex;
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("io error: {0}")]
    Io(String),
}

/// Raw Ethernet frame I/O abstraction (L2 header included).
pub trait EthTransport {
    fn send(&self, frame: &[u8]) -> Result<(), TransportError>;
    /// Returns `Ok(None)` if no frame is available before `timeout`.
    fn recv(&self, timeout: Option<Duration>) -> Result<Option<Vec<u8>>, TransportError>;
}

/// In-memory transport for testing.
#[derive(Default)]
pub struct MockTransport {
    tx: Mutex<Vec<Vec<u8>>>,
    rx: Mutex<std::collections::VecDeque<Vec<u8>>>,
}

impl MockTransport {
    pub fn new() -> Self {
        Self::default()
    }
    /// Pushes a frame that `recv` will return next (FIFO).
    pub fn push_rx(&self, frame: Vec<u8>) {
        self.rx.lock().unwrap().push_back(frame);
    }
    /// All frames emitted via `send`, in order.
    pub fn sent(&self) -> Vec<Vec<u8>> {
        self.tx.lock().unwrap().clone()
    }
}

impl EthTransport for MockTransport {
    fn send(&self, frame: &[u8]) -> Result<(), TransportError> {
        self.tx.lock().unwrap().push(frame.to_vec());
        Ok(())
    }
    fn recv(&self, _timeout: Option<Duration>) -> Result<Option<Vec<u8>>, TransportError> {
        Ok(self.rx.lock().unwrap().pop_front())
    }
}
```

Update `crates/profinet-rt/src/eth/mod.rs`:

```rust
mod header;
mod transport;
pub use header::{EthError, EthHeader, MacAddr, ETHERTYPE_PROFINET, ETHERTYPE_VLAN};
pub use transport::{EthTransport, MockTransport, TransportError};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p profinet-rt eth::transport -v`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(eth): trait EthTransport + MockTransport"
```

---

### Task 4: `AfPacketTransport` backend (Linux raw socket)

**Files:**
- Create: `crates/profinet-rt/src/eth/afpacket.rs`
- Modify: `crates/profinet-rt/src/eth/mod.rs` (export `#[cfg(target_os = "linux")]`)

**Interfaces:**
- Consumes: `EthTransport`, `TransportError` (Task 3)
- Produces:
  - `#[cfg(target_os = "linux")] pub struct AfPacketTransport`
  - `pub fn AfPacketTransport::open(ifname: &str) -> Result<Self, TransportError>` — opens an `AF_PACKET`/`SOCK_RAW` socket bound to the interface, filtered on EtherType PROFINET
  - implements `EthTransport`

- [ ] **Step 1: Write the test (error on nonexistent interface)**

> Note: `open()` on a real interface requires `CAP_NET_RAW` and a NIC; these tests are
> marked `#[ignore]` and run manually. The non-ignored test validates the error path.

In `crates/profinet-rt/src/eth/afpacket.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_unknown_interface_errors() {
        let r = AfPacketTransport::open("nonexistent-iface-xyz");
        assert!(r.is_err());
    }

    #[test]
    #[ignore = "requires CAP_NET_RAW + a real interface; run: cargo test -- --ignored"]
    fn open_loopback_succeeds() {
        // Adapt the interface name to the test machine (e.g. "lo", "eth0").
        let t = AfPacketTransport::open("lo").expect("open lo");
        let _ = t.recv(Some(std::time::Duration::from_millis(10)));
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p profinet-rt eth::afpacket -v`
Expected: FAIL (compilation: `AfPacketTransport` not defined).

- [ ] **Step 3: Implement the AF_PACKET backend**

At the top of `crates/profinet-rt/src/eth/afpacket.rs` (implementation via `nix`):

```rust
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::Duration;

use nix::sys::socket::{
    bind, recv, send, socket, AddressFamily, LinkAddr, MsgFlags, SockFlag, SockProtocol, SockType,
};

use super::transport::{EthTransport, TransportError};
use super::ETHERTYPE_PROFINET;

fn io_err<E: std::fmt::Display>(e: E) -> TransportError {
    TransportError::Io(e.to_string())
}

/// Raw AF_PACKET socket bound to an interface, filtered on the PROFINET EtherType.
pub struct AfPacketTransport {
    fd: OwnedFd,
}

impl AfPacketTransport {
    pub fn open(ifname: &str) -> Result<Self, TransportError> {
        let proto = SockProtocol::EthAll; // we filter the EtherType ourselves at recv
        let fd = socket(AddressFamily::Packet, SockType::Raw, SockFlag::empty(), proto)
            .map_err(io_err)?;

        let ifindex = nix::net::if_::if_nametoindex(ifname).map_err(io_err)?;
        let addr = LinkAddr::new(libc_ethertype_all(), ifindex as i32);
        bind(fd.as_raw_fd(), &addr).map_err(io_err)?;

        Ok(Self { fd })
    }
}

fn libc_ethertype_all() -> u16 {
    // ETH_P_ALL = 0x0003 (htons applied by the socket layer depending on platform)
    0x0003
}

impl EthTransport for AfPacketTransport {
    fn send(&self, frame: &[u8]) -> Result<(), TransportError> {
        send(self.fd.as_raw_fd(), frame, MsgFlags::empty()).map_err(io_err)?;
        Ok(())
    }

    fn recv(&self, _timeout: Option<Duration>) -> Result<Option<Vec<u8>>, TransportError> {
        let mut buf = vec![0u8; 1522];
        let n = recv(self.fd.as_raw_fd(), &mut buf, MsgFlags::empty()).map_err(io_err)?;
        buf.truncate(n);
        // Filter PROFINET EtherType (offset 12, without VLAN — fine-grained filtering will come with eth::header).
        if n >= 14 && u16::from_be_bytes([buf[12], buf[13]]) == ETHERTYPE_PROFINET {
            Ok(Some(buf))
        } else {
            Ok(None)
        }
    }
}
```

> ⚠️ Implementation detail to adjust at runtime depending on the `nix` version (`LinkAddr`/`SockProtocol` signatures). If the `nix` API diverges, fall back to raw `libc` for `socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ALL))` + `bind(sockaddr_ll)`. The public contract (`open`, `send`, `recv`) remains identical. Timeout handling (via `setsockopt SO_RCVTIMEO` or `poll`) will be refined in Plan 4 when the RT loop needs it.

Update `crates/profinet-rt/src/eth/mod.rs`:

```rust
#[cfg(target_os = "linux")]
mod afpacket;
#[cfg(target_os = "linux")]
pub use afpacket::AfPacketTransport;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p profinet-rt eth::afpacket -v`
Expected: PASS (`open_unknown_interface_errors`), the other is `ignored`.

Manual test (on the edge, with privileges): `sudo -E cargo test -p profinet-rt -- --ignored`

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(eth): backend AF_PACKET (socket raw Linux)"
```

---

### Task 5: Capture/replay harness (pcap golden-frames reader)

**Files:**
- Create: `crates/profinet-rt/src/capture.rs`
- Create: `crates/profinet-rt/tests/fixtures/README.md` (where to place `.pcap` files)
- Create: `crates/profinet-rt/tests/capture_replay.rs`
- Modify: `crates/profinet-rt/src/lib.rs` (add `pub mod capture;`)

**Interfaces:**
- Consumes: (nothing)
- Produces:
  - `pub struct PcapFrames { ... }`
  - `pub fn PcapFrames::open(path: &Path) -> Result<PcapFrames, CaptureError>`
  - `impl Iterator for PcapFrames { type Item = Vec<u8>; }` (each item = raw Ethernet frame)
  - `pub enum CaptureError` (via `thiserror`)

- [ ] **Step 1: Write the unit test (count + first frame) on an in-memory generated pcap**

In `crates/profinet-rt/src/capture.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use pcap_file::pcap::{PcapPacket, PcapWriter};
    use std::io::Cursor;
    use std::time::Duration;

    fn make_pcap(frames: &[&[u8]]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = PcapWriter::new(&mut buf).unwrap();
            for f in frames {
                w.write_packet(&PcapPacket::new(Duration::ZERO, f.len() as u32, f))
                    .unwrap();
            }
        }
        buf
    }

    #[test]
    fn reads_all_frames_in_order() {
        let bytes = make_pcap(&[&[0xaa, 0xbb], &[0xcc]]);
        let frames: Vec<Vec<u8>> = PcapFrames::from_reader(Cursor::new(bytes))
            .unwrap()
            .collect();
        assert_eq!(frames, vec![vec![0xaa, 0xbb], vec![0xcc]]);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p profinet-rt capture -v`
Expected: FAIL (compilation: `PcapFrames` not defined).

- [ ] **Step 3: Implement the pcap reader**

At the top of `crates/profinet-rt/src/capture.rs`:

```rust
use std::fs::File;
use std::io::Read;
use std::path::Path;

use pcap_file::pcap::PcapReader;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("io error: {0}")]
    Io(String),
    #[error("pcap parse error: {0}")]
    Pcap(String),
}

/// Iterator over raw Ethernet frames from a pcap file.
pub struct PcapFrames<R: Read> {
    reader: PcapReader<R>,
}

impl PcapFrames<File> {
    pub fn open(path: &Path) -> Result<Self, CaptureError> {
        let file = File::open(path).map_err(|e| CaptureError::Io(e.to_string()))?;
        Self::from_reader(file)
    }
}

impl<R: Read> PcapFrames<R> {
    pub fn from_reader(r: R) -> Result<Self, CaptureError> {
        let reader = PcapReader::new(r).map_err(|e| CaptureError::Pcap(e.to_string()))?;
        Ok(Self { reader })
    }
}

impl<R: Read> Iterator for PcapFrames<R> {
    type Item = Vec<u8>;
    fn next(&mut self) -> Option<Self::Item> {
        match self.reader.next_packet() {
            Some(Ok(pkt)) => Some(pkt.data.into_owned()),
            _ => None,
        }
    }
}
```

> ⚠️ `pcap-file` must be promoted from `dev-dependencies` to `dependencies` since
> `capture` is part of the public API. Move the `pcap-file = "2"` line into
> `[dependencies]` in the crate's `Cargo.toml`.

`crates/profinet-rt/tests/fixtures/README.md`:

```markdown
# Fixtures golden-frames

Place Wireshark captures (`.pcap`) of reference PROFINET exchanges here
(DCP, AR establishment, RT frames). They serve as ground truth for tests
in plans 2+ (per-layer parsing/serialisation).
```

`crates/profinet-rt/tests/capture_replay.rs`:

```rust
//! Integration test: replays a pcap fixture if present (otherwise skipped).
use profinet_rt::capture::PcapFrames;
use std::path::Path;

#[test]
fn replay_fixture_if_present() {
    let p = Path::new("tests/fixtures/sample.pcap");
    if !p.exists() {
        eprintln!("no fixture sample.pcap — test skipped");
        return;
    }
    let n = PcapFrames::open(p).unwrap().count();
    assert!(n > 0, "the pcap fixture must not be empty");
}
```

Add to `crates/profinet-rt/src/lib.rs`: `pub mod capture;`

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p profinet-rt -v`
Expected: PASS (all tests, including `reads_all_frames_in_order` and `replay_fixture_if_present`).

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(capture): lecteur pcap golden-frames + harnais de replay"
```

---

## Roadmap for upcoming plans (to be detailed when the time comes)

Each plan produces a testable deliverable and relies on reference Wireshark captures
+ the acquired IEC standard. Recommended order:

- **Plan 2 — `dcp`**: Discovery & Configuration Protocol (Identify / Get / Set name-of-station /
  Set IP / flash). First observable exchange with TIA Portal; testable against golden
  frames and in HIL ("the device appears and can be named in TIA").
- **Plan 3 — `cm` / AR establishment**: DCE/RPC over UDP 34964, AR state machine
  (Connect / Write records / Read / Dcontrol / Ccontrol / Release). **Core risk.**
  Target: AR reaches the DATA state.
- **Plan 4 — cyclic `rt`**: PPM/CPM, IOPS/IOCS, data status, cycle counter, watchdog;
  RT thread `SCHED_FIFO` + I/O image double-buffer/seqlock. Target: 1 ms send clock held,
  data making the round-trip.
- **Plan 5 — `alarm` + `im`**: Application-Ready alarm (required to enter RUN),
  plug/return-of-submodule alarms, I&M0 records.
- **Plan 6 — `config` + GSDML + public API**: typed config model (BOOL/INT/DINT/REAL/
  WORD), generation/consistency of the sample GSDML (16 REAL + 32 BOOL), `ProfinetDevice` facade.
- **Plan 7 — HIL integration + determinism**: real S7-1500 bench, automated AR→RUN verification
  + data round-trip, jitter measurement (`cyclictest`-style), PREEMPT_RT tuning guide
  (`isolcpus`, `nohz_full`, IRQ affinity), demo binary.

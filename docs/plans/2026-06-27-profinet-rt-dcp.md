# PROFINET-RT `dcp` Layer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the device side of PROFINET DCP (Discovery & Config Protocol) — parse incoming Identify requests and build byte-exact Identify responses — validated against real golden frames captured from an S7-1500 controller.

**Architecture:** New `dcp` module in crate `profinet-rt`, layered on the existing `eth` module. Three codec layers (frame header → TLV blocks → Identify semantics) plus a device-side dispatch entry point. Pure parse/serialize, no I/O — testable byte-exact against golden frames embedded as hex literals.

**Tech Stack:** Rust (stable, ≥1.96 installed), `thiserror` (already a dep), reuse of `eth::{MacAddr, EthHeader, ETHERTYPE_PROFINET}`. Big-endian throughout (PROFINET "Motorola" format).

## Global Constraints

- **Pure Rust**, no new heavy dependencies (only `thiserror`, already present).
- **Big-endian** on the wire — use `u16::from_be_bytes` / `to_be_bytes` (no `data` swap helpers needed; reuse `data` only if convenient).
- **Reuse `eth`**: `MacAddr([u8;6])`, `EthHeader { dst, src, vlan: Option<u16>, ethertype }` with `parse`/`write`, `ETHERTYPE_PROFINET == 0x8892`. Do not duplicate Ethernet logic.
- **Golden frames are the source of truth**: see `docs/dcp-golden-frames.md`. Tests embed the exact hex; **do not read pcapng** (the `capture` harness only reads legacy pcap and is out of scope here).
- Formatting/lint: `rustfmt` `max_width = 100`; `cargo clippy --all-targets -- -D warnings` must pass.
- License: workspace is `MIT OR Apache-2.0`; no per-file headers.
- Wire constants (verbatim): EtherType `0x8892`; DCP multicast MAC `01:0e:cf:00:00:00`; FrameIDs `0xfefe` Identify-request, `0xfeff` Identify-response, `0xfefd` Get/Set, `0xfefc` Hello; ServiceID Get=3 Set=4 Identify=5 Hello=6; ServiceType Request=0 ResponseSuccess=1. DCP header after FrameID = `ServiceID(1) ServiceType(1) Xid(4) ResponseDelay(2) DCPDataLength(2)`. Block = `Option(1) Suboption(1) DCPBlockLength(2) [BlockInfo(2)] Value(..)` padded to **even** total (value+blockinfo) with a `0x00` byte. **Request filter blocks carry NO BlockInfo; response/get blocks DO.**

---

## File Structure

- Create `crates/profinet-rt/src/dcp/mod.rs` — module root, `DcpError`, re-exports, device-side dispatch (`handle_dcp_frame`).
- Create `crates/profinet-rt/src/dcp/frame.rs` — `FrameId`, `ServiceId`, `ServiceType`, `DcpHeader`, `DCP_MULTICAST_MAC`.
- Create `crates/profinet-rt/src/dcp/block.rs` — `DcpBlock`, block parse/serialize (TLV + BlockInfo + even padding).
- Create `crates/profinet-rt/src/dcp/identify.rs` — `DeviceProperties`, `IdentifyFilter`, `parse_identify_request`, `build_identify_response`.
- Modify `crates/profinet-rt/src/lib.rs` — add `pub mod dcp;`.

Tests live inline (`#[cfg(test)] mod tests`) in each file, per the existing crate convention.

---

### Task 1: DCP frame header codec

**Files:**
- Create: `crates/profinet-rt/src/dcp/frame.rs`
- Create: `crates/profinet-rt/src/dcp/mod.rs`
- Modify: `crates/profinet-rt/src/lib.rs` (add `pub mod dcp;`)

**Interfaces:**
- Consumes: `crate::eth::MacAddr`.
- Produces:
  - `pub const DCP_MULTICAST_MAC: MacAddr`
  - `pub enum FrameId { IdentifyRequest, IdentifyResponse, GetSet, Hello }` with `pub fn from_u16(u16) -> Option<FrameId>` and `pub fn to_u16(self) -> u16`
  - `pub enum ServiceId { Get, Set, Identify, Hello }` with `from_u8`/`to_u8`
  - `pub enum ServiceType { Request, ResponseSuccess }` with `from_u8`/`to_u8`
  - `pub struct DcpHeader { pub service_id: ServiceId, pub service_type: ServiceType, pub xid: u32, pub response_delay: u16, pub data_length: u16 }` with `pub fn parse(buf: &[u8]) -> Result<(DcpHeader, &[u8]), DcpError>` (returns header + the remaining block bytes, sliced to `data_length`) and `pub fn write(&self, out: &mut Vec<u8>)`
  - `pub enum DcpError` (in `mod.rs`): `TooShort { need: usize, have: usize }`, `BadServiceId(u8)`, `BadServiceType(u8)`, `BadFrameId(u16)`, `Malformed(&'static str)`

- [ ] **Step 1: Add module declarations and the error type**

In `crates/profinet-rt/src/lib.rs`, add alongside the existing `pub mod` lines:
```rust
pub mod dcp;
```

Create `crates/profinet-rt/src/dcp/mod.rs`:
```rust
//! PROFINET DCP (Discovery & Configuration Protocol) — device side.

pub mod block;
pub mod frame;
pub mod identify;

use thiserror::Error;

/// Errors from parsing/serializing DCP frames.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DcpError {
    #[error("buffer too short: need {need}, have {have}")]
    TooShort { need: usize, have: usize },
    #[error("unknown DCP service id {0}")]
    BadServiceId(u8),
    #[error("unknown DCP service type {0}")]
    BadServiceType(u8),
    #[error("unknown DCP frame id {0:#06x}")]
    BadFrameId(u16),
    #[error("malformed DCP frame: {0}")]
    Malformed(&'static str),
}
```

- [ ] **Step 2: Write the failing test for `frame.rs`**

Create `crates/profinet-rt/src/dcp/frame.rs` with only the test module first:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    // DCP header bytes from the golden Identify REQUEST (docs/dcp-golden-frames.md),
    // i.e. the 10 header bytes after FrameID 0xfefe, followed by its single block.
    // 05 00 03000152 0001 000c | 02020008 692d646576696365
    const REQ_AFTER_FRAMEID: &[u8] = &[
        0x05, 0x00, 0x03, 0x00, 0x01, 0x52, 0x00, 0x01, 0x00, 0x0c, // header
        0x02, 0x02, 0x00, 0x08, 0x69, 0x2d, 0x64, 0x65, 0x76, 0x69, 0x63, 0x65, // block
    ];

    #[test]
    fn frameid_roundtrip() {
        assert_eq!(FrameId::from_u16(0xfefe), Some(FrameId::IdentifyRequest));
        assert_eq!(FrameId::from_u16(0xfeff), Some(FrameId::IdentifyResponse));
        assert_eq!(FrameId::from_u16(0x1234), None);
        assert_eq!(FrameId::IdentifyResponse.to_u16(), 0xfeff);
    }

    #[test]
    fn parse_golden_request_header() {
        let (h, blocks) = DcpHeader::parse(REQ_AFTER_FRAMEID).unwrap();
        assert_eq!(h.service_id, ServiceId::Identify);
        assert_eq!(h.service_type, ServiceType::Request);
        assert_eq!(h.xid, 0x0300_0152);
        assert_eq!(h.response_delay, 1);
        assert_eq!(h.data_length, 12);
        assert_eq!(blocks.len(), 12); // sliced to data_length
        assert_eq!(&blocks[..4], &[0x02, 0x02, 0x00, 0x08]);
    }

    #[test]
    fn header_write_roundtrip() {
        let h = DcpHeader {
            service_id: ServiceId::Identify,
            service_type: ServiceType::ResponseSuccess,
            xid: 0x0300_0152,
            response_delay: 0,
            data_length: 88,
        };
        let mut out = Vec::new();
        h.write(&mut out);
        assert_eq!(out, vec![0x05, 0x01, 0x03, 0x00, 0x01, 0x52, 0x00, 0x00, 0x00, 0x58]);
    }

    #[test]
    fn parse_too_short() {
        assert!(matches!(DcpHeader::parse(&[0x05, 0x00]), Err(DcpError::TooShort { .. })));
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt dcp::frame 2>&1 | tail -20`
Expected: FAIL (compile error — items not defined).

- [ ] **Step 4: Implement `frame.rs`**

Prepend above the test module in `crates/profinet-rt/src/dcp/frame.rs`:
```rust
//! DCP frame ID and acyclic header.

use crate::dcp::DcpError;
use crate::eth::MacAddr;

/// Multicast destination for DCP Identify requests.
pub const DCP_MULTICAST_MAC: MacAddr = MacAddr([0x01, 0x0e, 0xcf, 0x00, 0x00, 0x00]);

/// PROFINET RT FrameID values used by DCP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameId {
    IdentifyRequest,
    IdentifyResponse,
    GetSet,
    Hello,
}

impl FrameId {
    pub fn from_u16(v: u16) -> Option<FrameId> {
        match v {
            0xfefe => Some(FrameId::IdentifyRequest),
            0xfeff => Some(FrameId::IdentifyResponse),
            0xfefd => Some(FrameId::GetSet),
            0xfefc => Some(FrameId::Hello),
            _ => None,
        }
    }

    pub fn to_u16(self) -> u16 {
        match self {
            FrameId::IdentifyRequest => 0xfefe,
            FrameId::IdentifyResponse => 0xfeff,
            FrameId::GetSet => 0xfefd,
            FrameId::Hello => 0xfefc,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceId {
    Get,
    Set,
    Identify,
    Hello,
}

impl ServiceId {
    pub fn from_u8(v: u8) -> Result<ServiceId, DcpError> {
        match v {
            3 => Ok(ServiceId::Get),
            4 => Ok(ServiceId::Set),
            5 => Ok(ServiceId::Identify),
            6 => Ok(ServiceId::Hello),
            other => Err(DcpError::BadServiceId(other)),
        }
    }
    pub fn to_u8(self) -> u8 {
        match self {
            ServiceId::Get => 3,
            ServiceId::Set => 4,
            ServiceId::Identify => 5,
            ServiceId::Hello => 6,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceType {
    Request,
    ResponseSuccess,
}

impl ServiceType {
    pub fn from_u8(v: u8) -> Result<ServiceType, DcpError> {
        match v {
            0 => Ok(ServiceType::Request),
            1 => Ok(ServiceType::ResponseSuccess),
            other => Err(DcpError::BadServiceType(other)),
        }
    }
    pub fn to_u8(self) -> u8 {
        match self {
            ServiceType::Request => 0,
            ServiceType::ResponseSuccess => 1,
        }
    }
}

/// The 10-byte DCP acyclic header that follows the FrameID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DcpHeader {
    pub service_id: ServiceId,
    pub service_type: ServiceType,
    pub xid: u32,
    pub response_delay: u16,
    pub data_length: u16,
}

impl DcpHeader {
    /// Parse the header from bytes positioned just after the FrameID.
    /// Returns the header and the block bytes sliced to `data_length`.
    pub fn parse(buf: &[u8]) -> Result<(DcpHeader, &[u8]), DcpError> {
        if buf.len() < 10 {
            return Err(DcpError::TooShort { need: 10, have: buf.len() });
        }
        let header = DcpHeader {
            service_id: ServiceId::from_u8(buf[0])?,
            service_type: ServiceType::from_u8(buf[1])?,
            xid: u32::from_be_bytes([buf[2], buf[3], buf[4], buf[5]]),
            response_delay: u16::from_be_bytes([buf[6], buf[7]]),
            data_length: u16::from_be_bytes([buf[8], buf[9]]),
        };
        let need = 10 + header.data_length as usize;
        if buf.len() < need {
            return Err(DcpError::TooShort { need, have: buf.len() });
        }
        Ok((header, &buf[10..need]))
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(self.service_id.to_u8());
        out.push(self.service_type.to_u8());
        out.extend_from_slice(&self.xid.to_be_bytes());
        out.extend_from_slice(&self.response_delay.to_be_bytes());
        out.extend_from_slice(&self.data_length.to_be_bytes());
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt dcp::frame 2>&1 | tail -20`
Expected: PASS (4 tests).

- [ ] **Step 6: Lint + format**

Run: `. "$HOME/.cargo/env" && cargo fmt && cargo clippy --all-targets -- -D warnings 2>&1 | tail -15`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/profinet-rt/src/lib.rs crates/profinet-rt/src/dcp/mod.rs crates/profinet-rt/src/dcp/frame.rs
git commit -m "feat(dcp): frame id + acyclic header codec"
```

---

### Task 2: DCP TLV block codec

**Files:**
- Create: `crates/profinet-rt/src/dcp/block.rs`

**Interfaces:**
- Consumes: `crate::dcp::DcpError`.
- Produces:
  - `pub struct DcpBlock { pub option: u8, pub suboption: u8, pub block_info: Option<u16>, pub value: Vec<u8> }`
  - `pub fn parse_blocks(buf: &[u8], has_block_info: bool) -> Result<Vec<DcpBlock>, DcpError>`
  - `pub fn write_blocks(blocks: &[DcpBlock], out: &mut Vec<u8>)`
  - `pub fn blocks_encoded_len(blocks: &[DcpBlock]) -> u16` (sum of each block's `4 + block_info?2:0 + value.len()` rounded up to even — used to fill `DcpHeader.data_length`)

Encoding rules (from Global Constraints): each block is `option(1) suboption(1) DCPBlockLength(2)` where `DCPBlockLength = (block_info?2:0) + value.len()`, followed by the optional 2-byte BlockInfo (big-endian) and the value, then a single `0x00` pad byte iff `DCPBlockLength` is odd.

- [ ] **Step 1: Write the failing test**

Create `crates/profinet-rt/src/dcp/block.rs` with the test module first:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    // The full block body of the golden Identify RESPONSE (docs/dcp-golden-frames.md),
    // i.e. the 88 bytes after the DCP header. All blocks carry BlockInfo.
    const RESP_BLOCKS: &[u8] = &[
        0x02, 0x02, 0x00, 0x0a, 0x00, 0x00, 0x69, 0x2d, 0x64, 0x65, 0x76, 0x69, 0x63, 0x65, // NameOfStation "i-device"
        0x02, 0x05, 0x00, 0x04, 0x00, 0x00, 0x02, 0x07, // DeviceOptions
        0x02, 0x01, 0x00, 0x12, 0x00, 0x00, 0x53, 0x37, 0x2d, 0x31, 0x35, 0x30, 0x30, 0x20,
        0x28, 0x50, 0x4c, 0x43, 0x53, 0x49, 0x4d, 0x29, // TypeOfStation "S7-1500 (PLCSIM)"
        0x02, 0x03, 0x00, 0x06, 0x00, 0x00, 0x00, 0x2a, 0x01, 0x0e, // DeviceID Vendor 0x002a Device 0x010e
        0x02, 0x04, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, // DeviceRole
        0x02, 0x07, 0x00, 0x04, 0x00, 0x00, 0x10, 0x64, // DeviceInstance
        0x01, 0x02, 0x00, 0x0e, 0x00, 0x01, 0xc0, 0xa8, 0x01, 0x3d, 0xff, 0xff, 0xff, 0x00,
        0xc0, 0xa8, 0x01, 0x3d, // IPParameter 192.168.1.61 / 255.255.255.0 / 192.168.1.61
    ];

    #[test]
    fn parse_golden_response_blocks() {
        let blocks = parse_blocks(RESP_BLOCKS, true).unwrap();
        assert_eq!(blocks.len(), 7);
        assert_eq!((blocks[0].option, blocks[0].suboption), (2, 2));
        assert_eq!(blocks[0].block_info, Some(0));
        assert_eq!(&blocks[0].value, b"i-device");
        assert_eq!((blocks[3].option, blocks[3].suboption), (2, 3));
        assert_eq!(blocks[3].value, vec![0x00, 0x2a, 0x01, 0x0e]);
        assert_eq!((blocks[6].option, blocks[6].suboption), (1, 2));
        assert_eq!(blocks[6].block_info, Some(1));
        assert_eq!(blocks[6].value.len(), 12);
    }

    #[test]
    fn response_blocks_roundtrip_byte_exact() {
        let blocks = parse_blocks(RESP_BLOCKS, true).unwrap();
        let mut out = Vec::new();
        write_blocks(&blocks, &mut out);
        assert_eq!(out, RESP_BLOCKS);
        assert_eq!(blocks_encoded_len(&blocks), 88);
    }

    #[test]
    fn parse_request_filter_block_without_info() {
        // 02 02 00 08 "i-device" — request filter, no BlockInfo.
        let buf = &[0x02, 0x02, 0x00, 0x08, 0x69, 0x2d, 0x64, 0x65, 0x76, 0x69, 0x63, 0x65];
        let blocks = parse_blocks(buf, false).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].block_info, None);
        assert_eq!(&blocks[0].value, b"i-device");
    }

    #[test]
    fn odd_length_block_is_padded() {
        let blocks = vec![DcpBlock { option: 2, suboption: 1, block_info: Some(0), value: vec![0x41] }];
        let mut out = Vec::new();
        write_blocks(&blocks, &mut out);
        // hdr(4) + info(2) + value(1) = blocklen 3 (odd) -> +1 pad byte
        assert_eq!(out, vec![0x02, 0x01, 0x00, 0x03, 0x00, 0x00, 0x41, 0x00]);
        // round-trips back to a single block with the same value (pad consumed)
        let back = parse_blocks(&out, true).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].value, vec![0x41]);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt dcp::block 2>&1 | tail -20`
Expected: FAIL (not defined).

- [ ] **Step 3: Implement `block.rs`**

Prepend above the test module:
```rust
//! DCP TLV block codec (option/suboption/length/blockinfo/value, even-padded).

use crate::dcp::DcpError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DcpBlock {
    pub option: u8,
    pub suboption: u8,
    pub block_info: Option<u16>,
    pub value: Vec<u8>,
}

/// Parse a contiguous sequence of DCP blocks. `has_block_info` selects whether
/// each block's payload begins with a 2-byte BlockInfo (true for response/get,
/// false for request filter blocks).
pub fn parse_blocks(buf: &[u8], has_block_info: bool) -> Result<Vec<DcpBlock>, DcpError> {
    let mut blocks = Vec::new();
    let mut p = 0usize;
    while p < buf.len() {
        if p + 4 > buf.len() {
            return Err(DcpError::TooShort { need: p + 4, have: buf.len() });
        }
        let option = buf[p];
        let suboption = buf[p + 1];
        let block_len = u16::from_be_bytes([buf[p + 2], buf[p + 3]]) as usize;
        let body_start = p + 4;
        let body_end = body_start + block_len;
        if body_end > buf.len() {
            return Err(DcpError::TooShort { need: body_end, have: buf.len() });
        }
        let (block_info, value) = if has_block_info {
            if block_len < 2 {
                return Err(DcpError::Malformed("block too short for BlockInfo"));
            }
            let info = u16::from_be_bytes([buf[body_start], buf[body_start + 1]]);
            (Some(info), buf[body_start + 2..body_end].to_vec())
        } else {
            (None, buf[body_start..body_end].to_vec())
        };
        blocks.push(DcpBlock { option, suboption, block_info, value });
        // advance past the block, skipping the pad byte if block_len is odd
        p = body_end + (block_len & 1);
    }
    Ok(blocks)
}

/// Serialize blocks back to the wire format, padding each odd-length block.
pub fn write_blocks(blocks: &[DcpBlock], out: &mut Vec<u8>) {
    for b in blocks {
        let info_len = if b.block_info.is_some() { 2 } else { 0 };
        let block_len = info_len + b.value.len();
        out.push(b.option);
        out.push(b.suboption);
        out.extend_from_slice(&(block_len as u16).to_be_bytes());
        if let Some(info) = b.block_info {
            out.extend_from_slice(&info.to_be_bytes());
        }
        out.extend_from_slice(&b.value);
        if block_len & 1 == 1 {
            out.push(0x00);
        }
    }
}

/// Total encoded length of a block sequence (including 4-byte headers and pad bytes).
pub fn blocks_encoded_len(blocks: &[DcpBlock]) -> u16 {
    let mut total = 0usize;
    for b in blocks {
        let info_len = if b.block_info.is_some() { 2 } else { 0 };
        let block_len = info_len + b.value.len();
        total += 4 + block_len + (block_len & 1);
    }
    total as u16
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt dcp::block 2>&1 | tail -20`
Expected: PASS (4 tests).

- [ ] **Step 5: Lint + format**

Run: `. "$HOME/.cargo/env" && cargo fmt && cargo clippy --all-targets -- -D warnings 2>&1 | tail -15`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/profinet-rt/src/dcp/block.rs
git commit -m "feat(dcp): TLV block codec with BlockInfo and even padding"
```

---

### Task 3: Identify request parse + response build

**Files:**
- Create: `crates/profinet-rt/src/dcp/identify.rs`

**Interfaces:**
- Consumes: `frame::{FrameId, ServiceId, ServiceType, DcpHeader}`, `block::{DcpBlock, parse_blocks, write_blocks, blocks_encoded_len}`, `eth::{MacAddr, EthHeader, ETHERTYPE_PROFINET}`, `DcpError`.
- Produces:
  - `pub struct IdentifyFilter { pub name_of_station: Option<String> }`
  - `pub fn parse_identify_request(block_bytes: &[u8]) -> Result<IdentifyFilter, DcpError>` (input = the block bytes returned by `DcpHeader::parse`)
  - `pub struct DeviceProperties { pub name_of_station: String, pub type_of_station: String, pub vendor_id: u16, pub device_id: u16, pub device_role: u16, pub device_instance: u16, pub device_options: Vec<u8>, pub ip: [u8;4], pub subnet: [u8;4], pub gateway: [u8;4], pub ip_block_info: u16 }`
  - `pub fn build_identify_response(dst: MacAddr, src: MacAddr, xid: u32, props: &DeviceProperties) -> Vec<u8>` (returns the full Ethernet frame)

Response block order (must match golden, verbatim): NameOfStation(2.2), DeviceOptions(2.5), TypeOfStation(2.1), DeviceID(2.3), DeviceRole(2.4), DeviceInstance(2.7), IPParameter(1.2). DeviceID value = `vendor_id` then `device_id` (each big-endian u16). IPParameter value = `ip ++ subnet ++ gateway` (4 bytes each).

- [ ] **Step 1: Write the failing test**

Create `crates/profinet-rt/src/dcp/identify.rs` with the test module first:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::eth::MacAddr;

    // Golden Identify REQUEST block bytes (after the DCP header): the NameOfStation filter.
    const REQ_BLOCKS: &[u8] =
        &[0x02, 0x02, 0x00, 0x08, 0x69, 0x2d, 0x64, 0x65, 0x76, 0x69, 0x63, 0x65];

    // Full golden Identify RESPONSE frame (114 bytes), docs/dcp-golden-frames.md.
    const RESP_FRAME: &[u8] = &[
        0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f, 0x02, 0xc0, 0xa8, 0x01, 0x0f, 0x02, 0x88, 0x92,
        0xfe, 0xff, 0x05, 0x01, 0x03, 0x00, 0x01, 0x52, 0x00, 0x00, 0x00, 0x58, 0x02, 0x02,
        0x00, 0x0a, 0x00, 0x00, 0x69, 0x2d, 0x64, 0x65, 0x76, 0x69, 0x63, 0x65, 0x02, 0x05,
        0x00, 0x04, 0x00, 0x00, 0x02, 0x07, 0x02, 0x01, 0x00, 0x12, 0x00, 0x00, 0x53, 0x37,
        0x2d, 0x31, 0x35, 0x30, 0x30, 0x20, 0x28, 0x50, 0x4c, 0x43, 0x53, 0x49, 0x4d, 0x29,
        0x02, 0x03, 0x00, 0x06, 0x00, 0x00, 0x00, 0x2a, 0x01, 0x0e, 0x02, 0x04, 0x00, 0x04,
        0x00, 0x00, 0x00, 0x00, 0x02, 0x07, 0x00, 0x04, 0x00, 0x00, 0x10, 0x64, 0x01, 0x02,
        0x00, 0x0e, 0x00, 0x01, 0xc0, 0xa8, 0x01, 0x3d, 0xff, 0xff, 0xff, 0x00, 0xc0, 0xa8,
        0x01, 0x3d,
    ];

    fn golden_props() -> DeviceProperties {
        DeviceProperties {
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
        }
    }

    #[test]
    fn parse_request_filter_name() {
        let f = parse_identify_request(REQ_BLOCKS).unwrap();
        assert_eq!(f.name_of_station.as_deref(), Some("i-device"));
    }

    #[test]
    fn build_response_is_byte_exact() {
        let dst = MacAddr([0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f]);
        let src = MacAddr([0x02, 0xc0, 0xa8, 0x01, 0x0f, 0x02]);
        let frame = build_identify_response(dst, src, 0x0300_0152, &golden_props());
        assert_eq!(frame, RESP_FRAME);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt dcp::identify 2>&1 | tail -20`
Expected: FAIL (not defined).

- [ ] **Step 3: Implement `identify.rs`**

Prepend above the test module:
```rust
//! DCP Identify: parse requests, build responses.

use crate::dcp::block::{blocks_encoded_len, parse_blocks, write_blocks, DcpBlock};
use crate::dcp::frame::{DcpHeader, FrameId, ServiceId, ServiceType};
use crate::dcp::DcpError;
use crate::eth::{EthHeader, MacAddr, ETHERTYPE_PROFINET};

/// Filter criteria extracted from an incoming Identify request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IdentifyFilter {
    pub name_of_station: Option<String>,
}

/// Parse the block bytes of an Identify request (filter blocks, no BlockInfo).
pub fn parse_identify_request(block_bytes: &[u8]) -> Result<IdentifyFilter, DcpError> {
    let blocks = parse_blocks(block_bytes, false)?;
    let mut filter = IdentifyFilter::default();
    for b in blocks {
        if (b.option, b.suboption) == (2, 2) {
            let name = String::from_utf8(b.value)
                .map_err(|_| DcpError::Malformed("NameOfStation not UTF-8"))?;
            filter.name_of_station = Some(name);
        }
    }
    Ok(filter)
}

/// Static device identity advertised in Identify responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceProperties {
    pub name_of_station: String,
    pub type_of_station: String,
    pub vendor_id: u16,
    pub device_id: u16,
    pub device_role: u16,
    pub device_instance: u16,
    pub device_options: Vec<u8>,
    pub ip: [u8; 4],
    pub subnet: [u8; 4],
    pub gateway: [u8; 4],
    pub ip_block_info: u16,
}

/// Build the full Ethernet frame of an Identify response for the given device.
pub fn build_identify_response(
    dst: MacAddr,
    src: MacAddr,
    xid: u32,
    props: &DeviceProperties,
) -> Vec<u8> {
    let mut device_id_val = Vec::with_capacity(4);
    device_id_val.extend_from_slice(&props.vendor_id.to_be_bytes());
    device_id_val.extend_from_slice(&props.device_id.to_be_bytes());

    let mut ip_val = Vec::with_capacity(12);
    ip_val.extend_from_slice(&props.ip);
    ip_val.extend_from_slice(&props.subnet);
    ip_val.extend_from_slice(&props.gateway);

    let blocks = vec![
        DcpBlock { option: 2, suboption: 2, block_info: Some(0), value: props.name_of_station.as_bytes().to_vec() },
        DcpBlock { option: 2, suboption: 5, block_info: Some(0), value: props.device_options.clone() },
        DcpBlock { option: 2, suboption: 1, block_info: Some(0), value: props.type_of_station.as_bytes().to_vec() },
        DcpBlock { option: 2, suboption: 3, block_info: Some(0), value: device_id_val },
        DcpBlock { option: 2, suboption: 4, block_info: Some(0), value: props.device_role.to_be_bytes().to_vec() },
        DcpBlock { option: 2, suboption: 7, block_info: Some(0), value: props.device_instance.to_be_bytes().to_vec() },
        DcpBlock { option: 1, suboption: 2, block_info: Some(props.ip_block_info), value: ip_val },
    ];

    let header = DcpHeader {
        service_id: ServiceId::Identify,
        service_type: ServiceType::ResponseSuccess,
        xid,
        response_delay: 0,
        data_length: blocks_encoded_len(&blocks),
    };

    let mut out = Vec::new();
    EthHeader { dst, src, vlan: None, ethertype: ETHERTYPE_PROFINET }.write(&mut out);
    out.extend_from_slice(&FrameId::IdentifyResponse.to_u16().to_be_bytes());
    header.write(&mut out);
    write_blocks(&blocks, &mut out);
    out
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt dcp::identify 2>&1 | tail -20`
Expected: PASS (2 tests). If `build_response_is_byte_exact` fails, diff the produced vs golden hex — it pinpoints the wrong field.

- [ ] **Step 5: Lint + format**

Run: `. "$HOME/.cargo/env" && cargo fmt && cargo clippy --all-targets -- -D warnings 2>&1 | tail -15`
Expected: no warnings. (If clippy flags `vec!` construction, leave as-is — readability over micro-opt here.)

- [ ] **Step 6: Commit**

```bash
git add crates/profinet-rt/src/dcp/identify.rs
git commit -m "feat(dcp): Identify request parse + byte-exact response build"
```

---

### Task 4: Device-side DCP dispatch

**Files:**
- Modify: `crates/profinet-rt/src/dcp/mod.rs`

**Interfaces:**
- Consumes: `eth::{EthHeader, MacAddr}`, `frame::{FrameId, DcpHeader}`, `identify::{DeviceProperties, parse_identify_request, build_identify_response}`.
- Produces:
  - `pub struct DeviceConfig { pub mac: MacAddr, pub properties: DeviceProperties }`
  - `pub fn handle_dcp_frame(frame: &[u8], cfg: &DeviceConfig) -> Result<Option<Vec<u8>>, DcpError>`

Behavior: parse the Ethernet header; if `ethertype != 0x8892`, return `Ok(None)`. Read the 2-byte FrameID. For `FrameId::IdentifyRequest`, parse the DCP header + blocks, extract the filter; if the filter's `name_of_station` is `None` (identify-all) **or** equals `cfg.properties.name_of_station`, build and return the Identify response (`dst = incoming eth src`, `src = cfg.mac`, echoing the request `xid`). Otherwise `Ok(None)`. Any other FrameID returns `Ok(None)` for now (Get/Set/flash deferred).

- [ ] **Step 1: Write the failing test**

Append a test module to `crates/profinet-rt/src/dcp/mod.rs`:
```rust
#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use crate::dcp::identify::DeviceProperties;
    use crate::eth::MacAddr;

    // Golden Identify REQUEST frame (56 bytes), docs/dcp-golden-frames.md.
    const REQ_FRAME: &[u8] = &[
        0x01, 0x0e, 0xcf, 0x00, 0x00, 0x00, 0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f, 0x88, 0x92,
        0xfe, 0xfe, 0x05, 0x00, 0x03, 0x00, 0x01, 0x52, 0x00, 0x01, 0x00, 0x0c, 0x02, 0x02,
        0x00, 0x08, 0x69, 0x2d, 0x64, 0x65, 0x76, 0x69, 0x63, 0x65, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    fn cfg() -> DeviceConfig {
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
    fn responds_to_matching_identify() {
        let resp = handle_dcp_frame(REQ_FRAME, &cfg()).unwrap().expect("expected a response");
        // dst must be the requester (controller), src must be our device
        assert_eq!(&resp[0..6], &[0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f]);
        assert_eq!(&resp[6..12], &[0x02, 0xc0, 0xa8, 0x01, 0x0f, 0x02]);
        assert_eq!(&resp[14..16], &[0xfe, 0xff]); // Identify response FrameID
        // echoes the request Xid
        assert_eq!(&resp[18..22], &[0x03, 0x00, 0x01, 0x52]);
    }

    #[test]
    fn ignores_other_device_name() {
        let mut c = cfg();
        c.properties.name_of_station = "other".to_string();
        assert_eq!(handle_dcp_frame(REQ_FRAME, &c).unwrap(), None);
    }

    #[test]
    fn ignores_non_profinet_frame() {
        let mut f = REQ_FRAME.to_vec();
        f[12] = 0x08;
        f[13] = 0x00; // ethertype IPv4
        assert_eq!(handle_dcp_frame(&f, &cfg()).unwrap(), None);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt dcp:: 2>&1 | tail -20`
Expected: FAIL (`DeviceConfig`/`handle_dcp_frame` not defined).

- [ ] **Step 3: Implement the dispatch in `mod.rs`**

Add to `crates/profinet-rt/src/dcp/mod.rs` (after the `DcpError` definition, before the test module):
```rust
use crate::dcp::frame::{DcpHeader, FrameId};
use crate::dcp::identify::{build_identify_response, parse_identify_request, DeviceProperties};
use crate::eth::{EthHeader, MacAddr, ETHERTYPE_PROFINET};

/// Identity + address this device answers DCP for.
#[derive(Debug, Clone)]
pub struct DeviceConfig {
    pub mac: MacAddr,
    pub properties: DeviceProperties,
}

/// Handle one received Ethernet frame. Returns the response frame to send, or
/// `None` if the frame is not a DCP request this device should answer.
pub fn handle_dcp_frame(frame: &[u8], cfg: &DeviceConfig) -> Result<Option<Vec<u8>>, DcpError> {
    let (eth, payload_off) = EthHeader::parse(frame)
        .map_err(|_| DcpError::Malformed("bad Ethernet header"))?;
    if eth.ethertype != ETHERTYPE_PROFINET {
        return Ok(None);
    }
    let payload = &frame[payload_off..];
    if payload.len() < 2 {
        return Err(DcpError::TooShort { need: payload_off + 2, have: frame.len() });
    }
    let frame_id = u16::from_be_bytes([payload[0], payload[1]]);
    match FrameId::from_u16(frame_id) {
        Some(FrameId::IdentifyRequest) => {
            let (header, blocks) = DcpHeader::parse(&payload[2..])?;
            let filter = parse_identify_request(blocks)?;
            let matches = match &filter.name_of_station {
                None => true,
                Some(name) => name == &cfg.properties.name_of_station,
            };
            if matches {
                Ok(Some(build_identify_response(
                    eth.src,
                    cfg.mac,
                    header.xid,
                    &cfg.properties,
                )))
            } else {
                Ok(None)
            }
        }
        _ => Ok(None),
    }
}
```

Note: `EthHeader::parse` returns `(EthHeader, usize)` where the `usize` is the payload offset (VLAN-aware) — confirm this matches the existing signature in `eth/header.rs`; if it returns a different shape, adapt the destructuring (do not change `eth`).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt dcp:: 2>&1 | tail -20`
Expected: PASS (all dcp tests, incl. 3 dispatch tests).

- [ ] **Step 5: Full suite + lint + format**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt 2>&1 | tail -15 && cargo fmt && cargo clippy --all-targets -- -D warnings 2>&1 | tail -15`
Expected: all tests pass, no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/profinet-rt/src/dcp/mod.rs
git commit -m "feat(dcp): device-side dispatch (handle Identify, name filter, Xid echo)"
```

---

## Out of Scope (deferred)

- **Get / Set-NameOfStation / Set-IP / Flash (Signal LED)**: structure is the same TLV codec, but no golden frames yet (PLCSIM does not receive Set; capture on a real device or via p-net later). Add when golden frames exist.
- **pcapng support in the `capture` harness** (currently legacy pcap only) — golden frames here are embedded hex, so not blocking. Tracked in `FOLLOWUPS.md`.
- **Wiring DCP into a live `EthTransport` receive loop** — belongs with Plan 3/4 (AR + RT thread).

## Notes for the executor

- All four tasks are pure codec/logic with no I/O; they compile and test fast.
- The byte-exact tests (Task 2 `response_blocks_roundtrip_byte_exact`, Task 3 `build_response_is_byte_exact`) are the spec gates — they encode the real wire format. If they fail, the produced-vs-golden hex diff localizes the bug.
- Golden frame reference: `docs/dcp-golden-frames.md`. Capture provenance: real S7-1500 1515-2 PN (FW V2.9) ↔ PLCSIM `i-device`, 2026-06-26.

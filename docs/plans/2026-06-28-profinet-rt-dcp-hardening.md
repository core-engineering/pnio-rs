# PROFINET-RT `dcp` Hardening + pcapng Capture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Resolve the review follow-ups on the merged `dcp` module (fix Identify over-response, clear minor findings) and upgrade the `capture` harness to read pcapng and surface errors instead of swallowing them.

**Architecture:** Three independent tasks. (1) tighten DCP Identify filter matching; (2) mechanical minors (dead variant, overflow guards, re-exports, coverage); (3) rework `capture.rs` to support both pcap and pcapng with typed, non-silent errors.

**Tech Stack:** Rust (stable), `thiserror`, `pcap-file = "2"` (pcap **and** pcapng modules). Reuse existing `eth`/`dcp` modules.

## Global Constraints

- Pure Rust, no new dependencies (pcapng support comes from the already-present `pcap-file`).
- Big-endian on the wire; `rustfmt` `max_width = 100`; `cargo clippy --all-targets -- -D warnings` must pass.
- All cargo commands must be prefixed: `. "$HOME/.cargo/env" && cargo ...`.
- DCP wire rules unchanged (see `docs/dcp-golden-frames.md`): AllSelector block = option `0xff` suboption `0xff`; NameOfStation = option `2` suboption `2`.
- **No behavior regressions:** the existing 35 tests must still pass. The golden-frame byte-exact tests are sacred.
- Branch: `feat/dcp-hardening`, base `master` (HEAD `a666c2b`).

---

## File Structure

- Modify `crates/profinet-rt/src/dcp/identify.rs` — extend `IdentifyFilter` + `parse_identify_request` (Task 1).
- Modify `crates/profinet-rt/src/dcp/mod.rs` — tighten matching in `handle_dcp_frame`; remove dead `BadFrameId`; add `pub use` re-exports (Tasks 1 & 2).
- Modify `crates/profinet-rt/src/dcp/frame.rs` — coverage tests (Task 2).
- Modify `crates/profinet-rt/src/dcp/block.rs` — overflow `debug_assert!` (Task 2).
- Modify `crates/profinet-rt/src/capture.rs` — pcapng support + typed `CaptureError` + `Result` iterator (Task 3).

---

### Task 1: Fix Identify over-response (Important review finding)

**Problem:** `handle_dcp_frame` responds whenever `name_of_station` is `None` (identify-all). An Identify targeting another device *by DeviceID/IP* carries no NameOfStation block → `None` → this device replies spuriously on a multi-device segment.

**Fix:** classify every filter block; respond only when we can positively confirm a full match — matching NameOfStation, or an explicit AllSelector — and never when an unrecognized filter block is present.

**Files:**
- Modify: `crates/profinet-rt/src/dcp/identify.rs`
- Modify: `crates/profinet-rt/src/dcp/mod.rs`

**Interfaces:**
- Produces: `IdentifyFilter { name_of_station: Option<String>, all_selector: bool, other_filters: Vec<(u8,u8)> }` (still `Default`).

- [ ] **Step 1: Update the failing tests first**

In `crates/profinet-rt/src/dcp/identify.rs`, replace the `parse_request_filter_name` test and add classification tests:
```rust
    #[test]
    fn parse_request_filter_name() {
        let f = parse_identify_request(REQ_BLOCKS).unwrap();
        assert_eq!(f.name_of_station.as_deref(), Some("i-device"));
        assert!(!f.all_selector);
        assert!(f.other_filters.is_empty());
    }

    #[test]
    fn parse_all_selector() {
        // AllSelector: option 0xff suboption 0xff, length 0
        let f = parse_identify_request(&[0xff, 0xff, 0x00, 0x00]).unwrap();
        assert!(f.all_selector);
        assert_eq!(f.name_of_station, None);
    }

    #[test]
    fn parse_other_filter_is_recorded() {
        // DeviceID filter (2,3), 4-byte value, no name block
        let f = parse_identify_request(&[0x02, 0x03, 0x00, 0x04, 0x00, 0x2a, 0x01, 0x0e]).unwrap();
        assert_eq!(f.name_of_station, None);
        assert_eq!(f.other_filters, vec![(2, 3)]);
    }
```

- [ ] **Step 2: Run, verify failure**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt dcp::identify 2>&1 | tail -20`
Expected: FAIL (fields `all_selector`/`other_filters` don't exist).

- [ ] **Step 3: Extend `IdentifyFilter` and the parser**

In `identify.rs`, replace the struct and `parse_identify_request`:
```rust
/// Filter criteria extracted from an incoming Identify request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IdentifyFilter {
    pub name_of_station: Option<String>,
    /// AllSelector block (option 0xff, suboption 0xff) present.
    pub all_selector: bool,
    /// Filter blocks present that are neither NameOfStation nor AllSelector.
    pub other_filters: Vec<(u8, u8)>,
}

/// Parse the block bytes of an Identify request (filter blocks, no BlockInfo).
pub fn parse_identify_request(block_bytes: &[u8]) -> Result<IdentifyFilter, DcpError> {
    let blocks = parse_blocks(block_bytes, false)?;
    let mut filter = IdentifyFilter::default();
    for b in blocks {
        match (b.option, b.suboption) {
            (2, 2) => {
                let name = String::from_utf8(b.value)
                    .map_err(|_| DcpError::Malformed("NameOfStation not UTF-8"))?;
                filter.name_of_station = Some(name);
            }
            (0xff, 0xff) => filter.all_selector = true,
            other => filter.other_filters.push(other),
        }
    }
    Ok(filter)
}
```

- [ ] **Step 4: Run identify tests, verify pass**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt dcp::identify 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Update dispatch matching + add dispatch tests**

In `crates/profinet-rt/src/dcp/mod.rs`, inside `handle_dcp_frame`, replace the `matches` block:
```rust
            let filter = parse_identify_request(blocks)?;
            // Respond only on a confirmable full match: a matching NameOfStation,
            // or an explicit AllSelector. Any unrecognized filter block => stay silent
            // (we cannot confirm it matches this device).
            let respond = if !filter.other_filters.is_empty() {
                false
            } else if let Some(name) = &filter.name_of_station {
                name == &cfg.properties.name_of_station
            } else {
                filter.all_selector
            };
            if respond {
```
(rename the downstream `if matches {` to `if respond {`.)

Add to the `dispatch_tests` module a frame builder + new cases:
```rust
    // Wrap raw DCP filter blocks into a full Identify-request Ethernet frame.
    fn req_frame(blocks: &[u8]) -> Vec<u8> {
        let mut f = vec![
            0x01, 0x0e, 0xcf, 0x00, 0x00, 0x00, // dst multicast
            0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f, // src controller
            0x88, 0x92, // ethertype
            0xfe, 0xfe, // FrameID Identify request
            0x05, 0x00, 0x03, 0x00, 0x01, 0x52, 0x00, 0x01, // svc/type/xid/respdelay
        ];
        f.extend_from_slice(&(blocks.len() as u16).to_be_bytes()); // DCPDataLength
        f.extend_from_slice(blocks);
        f
    }

    #[test]
    fn responds_to_all_selector() {
        let f = req_frame(&[0xff, 0xff, 0x00, 0x00]);
        assert!(handle_dcp_frame(&f, &cfg()).unwrap().is_some());
    }

    #[test]
    fn ignores_identify_by_other_filter() {
        // DeviceID filter only, no NameOfStation -> must NOT respond (over-response fix)
        let f = req_frame(&[0x02, 0x03, 0x00, 0x04, 0x00, 0x2a, 0x01, 0x0e]);
        assert_eq!(handle_dcp_frame(&f, &cfg()).unwrap(), None);
    }
```

- [ ] **Step 6: Full dcp suite + lint**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt dcp:: 2>&1 | tail -20 && cargo fmt && cargo clippy --all-targets -- -D warnings 2>&1 | tail -8`
Expected: all pass, no warnings. (Existing `responds_to_matching_identify` and `ignores_other_device_name` must still pass.)

- [ ] **Step 7: Commit**

```bash
git add crates/profinet-rt/src/dcp/identify.rs crates/profinet-rt/src/dcp/mod.rs
git commit -m "fix(dcp): Identify responds only on confirmable match (no over-response)"
```

---

### Task 2: Minor follow-ups (dead variant, overflow guards, re-exports, coverage)

**Files:**
- Modify: `crates/profinet-rt/src/dcp/mod.rs` (remove `BadFrameId`, add re-exports)
- Modify: `crates/profinet-rt/src/dcp/block.rs` (overflow `debug_assert!`)
- Modify: `crates/profinet-rt/src/dcp/frame.rs` (coverage tests)

- [ ] **Step 1: Remove the dead `BadFrameId` variant**

In `mod.rs`, delete these two lines from `DcpError`:
```rust
    #[error("unknown DCP frame id {0:#06x}")]
    BadFrameId(u16),
```
(Unknown FrameIDs already fall through `handle_dcp_frame`'s `_ => Ok(None)`. Confirm with a grep that nothing constructs it: `grep -rn BadFrameId crates/` should return only the deleted lines.)

- [ ] **Step 2: Add `dcp::` re-exports**

In `mod.rs`, after the `pub mod identify;` line, add:
```rust
pub use block::{parse_blocks, write_blocks, DcpBlock};
pub use frame::{DcpHeader, FrameId, ServiceId, ServiceType, DCP_MULTICAST_MAC};
pub use identify::{
    build_identify_response, parse_identify_request, DeviceProperties, IdentifyFilter,
};
```

- [ ] **Step 3: Overflow guards in `block.rs`**

In `write_blocks`, before the length is written, add a guard. Replace:
```rust
        let block_len = info_len + b.value.len();
        out.push(b.option);
```
with:
```rust
        let block_len = info_len + b.value.len();
        debug_assert!(block_len <= u16::MAX as usize, "DCP block exceeds u16 length: {block_len}");
        out.push(b.option);
```
In `blocks_encoded_len`, replace `total as u16` with:
```rust
    debug_assert!(total <= u16::MAX as usize, "DCP blocks exceed u16 length: {total}");
    total as u16
```

- [ ] **Step 4: Coverage tests in `frame.rs`**

Add to the `tests` module in `frame.rs`:
```rust
    #[test]
    fn frameid_to_u16_all_variants() {
        assert_eq!(FrameId::IdentifyRequest.to_u16(), 0xfefe);
        assert_eq!(FrameId::GetSet.to_u16(), 0xfefd);
        assert_eq!(FrameId::Hello.to_u16(), 0xfefc);
    }

    #[test]
    fn service_enums_reject_unknown() {
        assert_eq!(ServiceId::from_u8(7), Err(DcpError::BadServiceId(7)));
        assert_eq!(ServiceType::from_u8(9), Err(DcpError::BadServiceType(9)));
    }

    #[test]
    fn parse_truncated_block_data_is_too_short() {
        // valid 10-byte header claims DCPDataLength=12 but only 4 block bytes follow
        let buf = &[
            0x05, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x0c, // header, data_length=12
            0x02, 0x02, 0x00, 0x08, // only 4 of 12 block bytes
        ];
        assert!(matches!(
            DcpHeader::parse(buf),
            Err(DcpError::TooShort { need: 22, have: 14 })
        ));
    }
```

- [ ] **Step 5: Run full crate suite + lint**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt 2>&1 | tail -15 && cargo fmt && cargo clippy --all-targets -- -D warnings 2>&1 | tail -8`
Expected: all pass, no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/profinet-rt/src/dcp/mod.rs crates/profinet-rt/src/dcp/block.rs crates/profinet-rt/src/dcp/frame.rs
git commit -m "refactor(dcp): drop dead BadFrameId, add re-exports, overflow guards, coverage"
```

---

### Task 3: pcapng support + typed, non-silent `CaptureError`

**Goal:** `PcapFrames` reads **both** legacy pcap and pcapng (auto-detected by magic), yields `Result<Vec<u8>, CaptureError>` (no more swallowed errors), and `CaptureError` wraps typed sources.

> **pcap-file API note (verify against the installed crate, do not guess):** before coding, check the exact API of `pcap-file = "2"` — read the installed source under `~/.cargo/registry/src/*/pcap-file-2*/` (or `cargo doc -p pcap-file --open` is unavailable headless, so read the source). Confirm: (a) the pcapng reader type and its iteration method (`PcapNgReader::new` + `next_block` returning `Option<Result<Block, _>>`); (b) the `Block` enum variant names for captured packets (expected `EnhancedPacket` and `SimplePacket`) and how to get their bytes (`.data`); (c) the error type(s) — if pcap and pcapng share `pcap_file::PcapError`, one `#[from]` suffices; if pcapng has its own error, add a second variant. Adapt the code below to what compiles; keep the behavior and the public shape (`Result` item, typed error).

**Files:**
- Modify: `crates/profinet-rt/src/capture.rs`

**Interfaces:**
- Produces: `PcapFrames<R: Read>` with `Iterator<Item = Result<Vec<u8>, CaptureError>>`; `CaptureError` with `#[from]` sources + `UnknownFormat([u8;4])`.

- [ ] **Step 1: Write the pcapng + error tests first**

Replace the `tests` module in `capture.rs`:
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

    fn collect(bytes: Vec<u8>) -> Vec<Vec<u8>> {
        PcapFrames::from_reader(Cursor::new(bytes))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn reads_all_pcap_frames_in_order() {
        let frames = collect(make_pcap(&[&[0xaa, 0xbb], &[0xcc]]));
        assert_eq!(frames, vec![vec![0xaa, 0xbb], vec![0xcc]]);
    }

    #[test]
    fn reads_pcapng_frames_in_order() {
        // Build a minimal pcapng via the writer, then read it back.
        let bytes = make_pcapng(&[&[0xaa, 0xbb], &[0xcc]]);
        let frames = collect(bytes);
        assert_eq!(frames, vec![vec![0xaa, 0xbb], vec![0xcc]]);
    }

    #[test]
    fn unknown_format_errors() {
        let err = PcapFrames::from_reader(Cursor::new(vec![0x00, 0x01, 0x02, 0x03, 0x04]));
        assert!(matches!(err, Err(CaptureError::UnknownFormat(_))));
    }
}
```

You must provide `make_pcapng(frames: &[&[u8]]) -> Vec<u8>` in the test module using pcap-file's pcapng writer (SectionHeader + InterfaceDescription(LinkType Ethernet) + one EnhancedPacket per frame). Write it against the real API. If the writer API proves awkward, instead embed a hand-built minimal pcapng `const` (SHB + IDB + EPBs) and read that — either way the test must feed real pcapng bytes through `PcapFrames`.

- [ ] **Step 2: Run, verify failure**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt capture 2>&1 | tail -25`
Expected: FAIL (compile — new API not present).

- [ ] **Step 3: Rewrite `capture.rs`**

Replace the non-test part of the file with (adapt the marked pcap-file specifics to what compiles):
```rust
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::Path;

use pcap_file::pcap::PcapReader;
use pcap_file::pcapng::PcapNgReader;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("capture parse error: {0}")]
    Pcap(#[from] pcap_file::PcapError),
    #[error("unknown capture format (magic {0:02x?})")]
    UnknownFormat([u8; 4]),
}

type Peeked<R> = std::io::Chain<Cursor<Vec<u8>>, R>;

enum Inner<R: Read> {
    Pcap(PcapReader<Peeked<R>>),
    PcapNg(PcapNgReader<Peeked<R>>),
}

/// Iterator over raw Ethernet frames from a pcap or pcapng source (auto-detected).
pub struct PcapFrames<R: Read> {
    inner: Inner<R>,
}

impl PcapFrames<File> {
    pub fn open(path: &Path) -> Result<Self, CaptureError> {
        Self::from_reader(File::open(path)?)
    }
}

impl<R: Read> PcapFrames<R> {
    pub fn from_reader(mut r: R) -> Result<Self, CaptureError> {
        let mut magic = [0u8; 4];
        r.read_exact(&mut magic)?;
        let chained = Cursor::new(magic.to_vec()).chain(r);
        let inner = match magic {
            // pcapng Section Header Block type
            [0x0a, 0x0d, 0x0d, 0x0a] => Inner::PcapNg(PcapNgReader::new(chained)?),
            // classic pcap magics (LE/BE, us/ns precision)
            [0xd4, 0xc3, 0xb2, 0xa1]
            | [0xa1, 0xb2, 0xc3, 0xd4]
            | [0x4d, 0x3c, 0xb2, 0xa1]
            | [0xa1, 0xb2, 0x3c, 0x4d] => Inner::Pcap(PcapReader::new(chained)?),
            other => return Err(CaptureError::UnknownFormat(other)),
        };
        Ok(Self { inner })
    }
}

impl<R: Read> Iterator for PcapFrames<R> {
    type Item = Result<Vec<u8>, CaptureError>;
    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            Inner::Pcap(rd) => match rd.next_packet() {
                Some(Ok(pkt)) => Some(Ok(pkt.data.into_owned())),
                Some(Err(e)) => Some(Err(e.into())),
                None => None,
            },
            Inner::PcapNg(rd) => loop {
                match rd.next_block() {
                    Some(Ok(block)) => {
                        use pcap_file::pcapng::Block;
                        let data = match block {
                            Block::EnhancedPacket(b) => Some(b.data.into_owned()),
                            Block::SimplePacket(b) => Some(b.data.into_owned()),
                            _ => None, // skip section/interface/option blocks
                        };
                        if let Some(d) = data {
                            return Some(Ok(d));
                        }
                    }
                    Some(Err(e)) => return Some(Err(e.into())),
                    None => return None,
                }
            },
        }
    }
}
```

- [ ] **Step 4: Run capture tests, verify pass**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt capture 2>&1 | tail -25`
Expected: PASS (3 tests). Fix any pcap-file API mismatch (error type, Block variants, writer) until green.

- [ ] **Step 5: Full crate suite + integration test + lint**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt 2>&1 | tail -15 && cargo clippy --all-targets -- -D warnings 2>&1 | tail -8`
Expected: all pass (incl. `replay_fixture_if_present` integration test, which uses `.count()` and is unaffected by the `Result` item), no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/profinet-rt/src/capture.rs
git commit -m "feat(capture): read pcapng + pcap, typed non-silent CaptureError"
```

---

## Notes for the executor
- Tasks are independent; do them in order. Task 3 is the only one touching a third-party crate API — verify pcap-file specifics against the installed source rather than guessing.
- The merged baseline has 35 passing tests; each task must keep them green and add its own.
- After all three: whole-branch review, then `superpowers:finishing-a-development-branch` (merge to `master`).

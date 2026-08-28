#![allow(dead_code)]
//! Test-only helpers: golden frame loading from `testdata/cm/*.hex`.
//!
//! Duplicated from `src/testutil.rs` because integration tests cannot see
//! `crate::testutil` (it is a private, `#[cfg(test)]`-only module of the library crate).

/// Offset of the DCE-RPC PDU inside an untagged IPv4/UDP Ethernet frame (14 + 20 + 8).
pub const RPC_OFF: usize = 42;
/// Offset of the PROFINET FrameID inside a VLAN-tagged Ethernet frame (14 + 4).
pub const VLAN_PAYLOAD_OFF: usize = 18;

/// Parse a hex dump: `#` starts a comment to end of line, whitespace separates bytes.
pub fn parse_hex(text: &str) -> Vec<u8> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .flat_map(|l| {
            l.split_whitespace()
                .map(|b| u8::from_str_radix(b, 16).expect("hex byte"))
        })
        .collect::<Vec<u8>>()
}

/// Load `testdata/cm/<name>.hex` relative to the crate root.
pub fn golden(name: &str) -> Vec<u8> {
    let path = format!("{}/testdata/cm/{name}.hex", env!("CARGO_MANIFEST_DIR"));
    parse_hex(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")))
}

/// Offsets inside a VLAN-tagged RTC1 golden frame.
pub const RT_FRAMEID_OFF: usize = 18;
pub const RT_CSDU_OFF: usize = 20;
pub const RT_APDU_OFF: usize = 60;

/// Load `testdata/rt/<name>.hex` relative to the crate root.
pub fn golden_rt(name: &str) -> Vec<u8> {
    let path = format!("{}/testdata/rt/{name}.hex", env!("CARGO_MANIFEST_DIR"));
    parse_hex(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")))
}

//! Test-only helpers: golden frame loading from `testdata/cm/*.hex`.

/// Offset of the DCE-RPC PDU inside an untagged IPv4/UDP Ethernet frame (14 + 20 + 8).
///
/// Unused until later tasks' codec tests consume it; `dead_code` is allowed here rather
/// than deferring the constant's addition.
#[allow(dead_code)]
pub const RPC_OFF: usize = 42;
/// Offset of the PROFINET FrameID inside a VLAN-tagged Ethernet frame (14 + 4).
#[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_skips_comments_and_whitespace() {
        assert_eq!(parse_hex("# c\n01 02\n  0a # tail\n"), vec![1, 2, 10]);
    }

    #[test]
    fn golden_files_have_expected_lengths() {
        for (name, len) in [
            ("ident_ok_pnet", 144),
            ("dcp_set_req", 64),
            ("dcp_set_res", 34),
            ("connect_req", 699),
            ("connect_res", 232),
            ("write_req", 486),
            ("write_res", 462),
            ("prmend_req", 174),
            ("prmend_res", 174),
            ("appready_req", 174),
            ("appready_res", 174),
        ] {
            assert_eq!(golden(name).len(), len, "{name}");
        }
    }
}

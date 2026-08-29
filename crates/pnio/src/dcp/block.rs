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
            return Err(DcpError::TooShort {
                need: p + 4,
                have: buf.len(),
            });
        }
        let option = buf[p];
        let suboption = buf[p + 1];
        let block_len = u16::from_be_bytes([buf[p + 2], buf[p + 3]]) as usize;
        let body_start = p + 4;
        let body_end = body_start + block_len;
        if body_end > buf.len() {
            return Err(DcpError::TooShort {
                need: body_end,
                have: buf.len(),
            });
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
        blocks.push(DcpBlock {
            option,
            suboption,
            block_info,
            value,
        });
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
        debug_assert!(
            block_len <= u16::MAX as usize,
            "DCP block exceeds u16 length: {block_len}"
        );
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
    debug_assert!(
        total <= u16::MAX as usize,
        "DCP blocks exceed u16 length: {total}"
    );
    total as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    // The full block body of the golden Identify RESPONSE (docs/dcp-golden-frames.md),
    // i.e. the 88 bytes after the DCP header. All blocks carry BlockInfo.
    const RESP_BLOCKS: &[u8] = &[
        0x02, 0x02, 0x00, 0x0a, 0x00, 0x00, 0x69, 0x2d, 0x64, 0x65, 0x76, 0x69, 0x63,
        0x65, // NameOfStation "i-device"
        0x02, 0x05, 0x00, 0x04, 0x00, 0x00, 0x02, 0x07, // DeviceOptions
        0x02, 0x01, 0x00, 0x12, 0x00, 0x00, 0x53, 0x37, 0x2d, 0x31, 0x35, 0x30, 0x30, 0x20, 0x28,
        0x50, 0x4c, 0x43, 0x53, 0x49, 0x4d, 0x29, // TypeOfStation "S7-1500 (PLCSIM)"
        0x02, 0x03, 0x00, 0x06, 0x00, 0x00, 0x00, 0x2a, 0x01,
        0x0e, // DeviceID Vendor 0x002a Device 0x010e
        0x02, 0x04, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, // DeviceRole
        0x02, 0x07, 0x00, 0x04, 0x00, 0x00, 0x10, 0x64, // DeviceInstance
        0x01, 0x02, 0x00, 0x0e, 0x00, 0x01, 0xc0, 0xa8, 0x01, 0x3d, 0xff, 0xff, 0xff, 0x00, 0xc0,
        0xa8, 0x01, 0x3d, // IPParameter 192.168.1.61 / 255.255.255.0 / 192.168.1.61
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
        let buf = &[
            0x02, 0x02, 0x00, 0x08, 0x69, 0x2d, 0x64, 0x65, 0x76, 0x69, 0x63, 0x65,
        ];
        let blocks = parse_blocks(buf, false).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].block_info, None);
        assert_eq!(&blocks[0].value, b"i-device");
    }

    #[test]
    fn odd_length_block_is_padded() {
        let blocks = vec![DcpBlock {
            option: 2,
            suboption: 1,
            block_info: Some(0),
            value: vec![0x41],
        }];
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

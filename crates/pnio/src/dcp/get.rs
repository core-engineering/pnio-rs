//! DCP Get: parse requests, build responses.

use crate::dcp::block::{blocks_encoded_len, write_blocks, DcpBlock};
use crate::dcp::frame::{DcpHeader, FrameId, ServiceId, ServiceType};
use crate::dcp::identify::{property_block, DeviceProperties};
use crate::dcp::set::SetBlockError;
use crate::dcp::DcpError;
use crate::eth::{EthHeader, MacAddr, ETHERTYPE_PROFINET};

/// Parse the block bytes of a Get request: `(option, suboption)` pairs, no length.
pub fn parse_get_request(block_bytes: &[u8]) -> Result<Vec<(u8, u8)>, DcpError> {
    if block_bytes.len() % 2 != 0 {
        return Err(DcpError::Malformed("odd DCP Get request length"));
    }
    Ok(block_bytes.chunks_exact(2).map(|p| (p[0], p[1])).collect())
}

/// Build the full Ethernet frame of a Get response: one block per requested pair, in
/// request order, a Control/Response block with the error for what this device lacks.
pub fn build_get_response(
    dst: MacAddr,
    src: MacAddr,
    xid: u32,
    requested: &[(u8, u8)],
    props: &DeviceProperties,
) -> Vec<u8> {
    let blocks: Vec<DcpBlock> = requested
        .iter()
        .map(|&(option, suboption)| {
            property_block(props, option, suboption).unwrap_or_else(|| {
                let err = match option {
                    1 | 2 => SetBlockError::SuboptionNotSupported,
                    _ => SetBlockError::OptionNotSupported,
                };
                DcpBlock {
                    option: 5,
                    suboption: 4,
                    block_info: None,
                    value: vec![option, suboption, err as u8],
                }
            })
        })
        .collect();

    let header = DcpHeader {
        service_id: ServiceId::Get,
        service_type: ServiceType::ResponseSuccess,
        xid,
        response_delay: 0,
        data_length: blocks_encoded_len(&blocks),
    };

    let mut out = Vec::new();
    EthHeader {
        dst,
        src,
        vlan: None,
        ethertype: ETHERTYPE_PROFINET,
    }
    .write(&mut out);
    out.extend_from_slice(&FrameId::GetSet.to_u16().to_be_bytes());
    header.write(&mut out);
    write_blocks(&blocks, &mut out);
    out
}

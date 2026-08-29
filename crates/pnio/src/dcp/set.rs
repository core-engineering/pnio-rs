//! DCP Set: parse the IP-suite Set request, decide (guarded), build the response.
//!
//! The interface address is never changed by this stack (the bench edge is also a
//! gateway) — `decide_set` only ever accepts a Set that matches the IP the device
//! already has (spec §5.6); anything else is reported as refused, not applied.

use crate::dcp::block::{blocks_encoded_len, parse_blocks, write_blocks, DcpBlock};
use crate::dcp::frame::{DcpHeader, FrameId, ServiceId, ServiceType};
use crate::dcp::identify::DeviceProperties;
use crate::dcp::DcpError;
use crate::eth::{EthHeader, MacAddr, ETHERTYPE_PROFINET};

/// One block from a Set request, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetBlock {
    /// IP option (1), suboption 2 (IPParameter): the qualifier and requested triple.
    IpSuite {
        qualifier: u16,
        ip: [u8; 4],
        subnet: [u8; 4],
        gateway: [u8; 4],
    },
    /// Any other option/suboption we don't implement.
    Other { option: u8, suboption: u8 },
}

/// A parsed DCP Set request: the ordered list of blocks it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetRequest {
    pub blocks: Vec<SetBlock>,
}

/// Per-block Set result, carried in the Control/Response block of the reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SetBlockError {
    Ok = 0x00,
    OptionNotSupported = 0x01,
    SuboptionNotSupported = 0x02,
    SuboptionNotSet = 0x03,
    ResourceError = 0x04,
    SetNotPossible = 0x05,
}

/// Parse the block bytes of a Set request. Set request blocks carry a 2-byte
/// BlockQualifier where responses carry BlockInfo, so this reads with
/// `has_block_info = true` and treats `block_info` as the qualifier.
pub fn parse_set_request(block_bytes: &[u8]) -> Result<SetRequest, DcpError> {
    let blocks = parse_blocks(block_bytes, true)?;
    let mut out = Vec::with_capacity(blocks.len());
    for b in blocks {
        let qualifier = b
            .block_info
            .expect("parse_blocks(.., true) always yields Some block_info");
        match (b.option, b.suboption) {
            (1, 2) => {
                if b.value.len() != 12 {
                    return Err(DcpError::Malformed("IPParameter value must be 12 bytes"));
                }
                let mut ip = [0u8; 4];
                let mut subnet = [0u8; 4];
                let mut gateway = [0u8; 4];
                ip.copy_from_slice(&b.value[0..4]);
                subnet.copy_from_slice(&b.value[4..8]);
                gateway.copy_from_slice(&b.value[8..12]);
                out.push(SetBlock::IpSuite {
                    qualifier,
                    ip,
                    subnet,
                    gateway,
                });
            }
            (option, suboption) => out.push(SetBlock::Other { option, suboption }),
        }
    }
    Ok(SetRequest { blocks: out })
}

/// Decide the outcome of each block in a Set request against the device's current
/// IP suite. The interface is **never** modified: `Set Ok` means the whole requested
/// suite (ip, subnet **and** gateway) already equals the configured one; any
/// difference in any of the three is refused with `SetNotPossible`. Unsupported
/// options are refused with `SuboptionNotSupported`.
pub fn decide_set(req: &SetRequest, current: &DeviceProperties) -> Vec<(u8, u8, SetBlockError)> {
    req.blocks
        .iter()
        .map(|b| match b {
            SetBlock::IpSuite {
                ip,
                subnet,
                gateway,
                ..
            } if *ip == current.ip && *subnet == current.subnet && *gateway == current.gateway => {
                (1, 2, SetBlockError::Ok)
            }
            SetBlock::IpSuite { .. } => (1, 2, SetBlockError::SetNotPossible),
            SetBlock::Other { option, suboption } => {
                (*option, *suboption, SetBlockError::SuboptionNotSupported)
            }
        })
        .collect()
}

/// Build the full Ethernet frame of a Set response (untagged, no BlockInfo on the
/// Control/Response blocks) carrying one result per input tuple.
pub fn build_set_response(
    dst: MacAddr,
    src: MacAddr,
    xid: u32,
    results: &[(u8, u8, SetBlockError)],
) -> Vec<u8> {
    let blocks: Vec<DcpBlock> = results
        .iter()
        .map(|(option, suboption, err)| DcpBlock {
            option: 5,
            suboption: 4,
            block_info: None,
            value: vec![*option, *suboption, *err as u8],
        })
        .collect();

    let header = DcpHeader {
        service_id: ServiceId::Set,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dcp::{handle_dcp_frame, DeviceConfig, DeviceProperties};
    use crate::eth::MacAddr;
    use crate::testutil::{golden, VLAN_PAYLOAD_OFF};

    fn cfg(ip: [u8; 4]) -> DeviceConfig {
        cfg_full(ip, [255, 255, 255, 0], ip)
    }

    fn cfg_full(ip: [u8; 4], subnet: [u8; 4], gateway: [u8; 4]) -> DeviceConfig {
        DeviceConfig {
            mac: MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]),
            properties: DeviceProperties {
                name_of_station: "rt-labs-dev".into(),
                type_of_station: "P-Net Sample Application".into(),
                vendor_id: 0x0493,
                device_id: 0x0002,
                device_role: 0x0100,
                device_instance: 1,
                device_options: vec![1, 2, 2, 2, 2, 3],
                ip,
                subnet,
                gateway,
                ip_block_info: 1,
            },
        }
    }

    #[test]
    fn parse_golden_set_ip_request() {
        let f = golden("dcp_set_req");
        // FrameID at 18 (VLAN), DCP header at 20, blocks after the 10-byte header
        let (h, blocks) = crate::dcp::DcpHeader::parse(&f[VLAN_PAYLOAD_OFF + 2..]).unwrap();
        assert_eq!(h.xid, 0x0300_012d);
        let req = parse_set_request(blocks).unwrap();
        assert_eq!(
            req.blocks,
            vec![SetBlock::IpSuite {
                qualifier: 0,
                ip: [172, 16, 2, 10],
                subnet: [255, 255, 255, 0],
                gateway: [172, 16, 2, 10]
            }]
        );
    }

    #[test]
    fn set_ok_response_is_byte_exact_via_dispatch() {
        let resp = handle_dcp_frame(&golden("dcp_set_req"), &cfg([172, 16, 2, 10]))
            .unwrap()
            .unwrap();
        assert_eq!(resp, golden("dcp_set_res"));
    }

    #[test]
    fn different_ip_is_refused_not_applied() {
        let resp = handle_dcp_frame(&golden("dcp_set_req"), &cfg([172, 16, 2, 99]))
            .unwrap()
            .unwrap();
        // same frame, BlockError = SetNotPossible (0x05) at the last value byte
        let mut expected = golden("dcp_set_res");
        expected[32] = 0x05;
        assert_eq!(resp, expected);
    }

    #[test]
    fn unsupported_option_gets_suboption_not_supported() {
        let req = SetRequest {
            blocks: vec![SetBlock::Other {
                option: 2,
                suboption: 2,
            }],
        };
        assert_eq!(
            decide_set(&req, &cfg([1, 2, 3, 4]).properties),
            vec![(2, 2, SetBlockError::SuboptionNotSupported)]
        );
    }

    #[test]
    fn matching_ip_but_other_subnet_is_refused() {
        let resp = handle_dcp_frame(
            &golden("dcp_set_req"),
            &cfg_full([172, 16, 2, 10], [255, 255, 0, 0], [172, 16, 2, 10]),
        )
        .unwrap()
        .unwrap();
        assert_eq!(resp[32], 0x05);
    }

    #[test]
    fn matching_ip_but_other_gateway_is_refused() {
        let resp = handle_dcp_frame(
            &golden("dcp_set_req"),
            &cfg_full([172, 16, 2, 10], [255, 255, 255, 0], [172, 16, 2, 1]),
        )
        .unwrap()
        .unwrap();
        assert_eq!(resp[32], 0x05);
    }

    #[test]
    fn get_request_is_ignored() {
        let mut f = golden("dcp_set_req");
        f[VLAN_PAYLOAD_OFF + 2] = 3; // ServiceID Get
        assert_eq!(handle_dcp_frame(&f, &cfg([172, 16, 2, 10])).unwrap(), None);
    }
}

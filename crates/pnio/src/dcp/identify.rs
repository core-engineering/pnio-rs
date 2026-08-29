//! DCP Identify: parse requests, build responses.

use crate::dcp::block::{blocks_encoded_len, parse_blocks, write_blocks, DcpBlock};
use crate::dcp::frame::{DcpHeader, FrameId, ServiceId, ServiceType};
use crate::dcp::DcpError;
use crate::eth::{EthHeader, MacAddr, ETHERTYPE_PROFINET};

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
        DcpBlock {
            option: 2,
            suboption: 2,
            block_info: Some(0),
            value: props.name_of_station.as_bytes().to_vec(),
        },
        DcpBlock {
            option: 2,
            suboption: 5,
            block_info: Some(0),
            value: props.device_options.clone(),
        },
        DcpBlock {
            option: 2,
            suboption: 1,
            block_info: Some(0),
            value: props.type_of_station.as_bytes().to_vec(),
        },
        DcpBlock {
            option: 2,
            suboption: 3,
            block_info: Some(0),
            value: device_id_val,
        },
        DcpBlock {
            option: 2,
            suboption: 4,
            block_info: Some(0),
            value: props.device_role.to_be_bytes().to_vec(),
        },
        DcpBlock {
            option: 2,
            suboption: 7,
            block_info: Some(0),
            value: props.device_instance.to_be_bytes().to_vec(),
        },
        DcpBlock {
            option: 1,
            suboption: 2,
            block_info: Some(props.ip_block_info),
            value: ip_val,
        },
    ];

    let header = DcpHeader {
        service_id: ServiceId::Identify,
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
    out.extend_from_slice(&FrameId::IdentifyResponse.to_u16().to_be_bytes());
    header.write(&mut out);
    write_blocks(&blocks, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eth::MacAddr;

    // Golden Identify REQUEST block bytes (after the DCP header): the NameOfStation filter.
    const REQ_BLOCKS: &[u8] = &[
        0x02, 0x02, 0x00, 0x08, 0x69, 0x2d, 0x64, 0x65, 0x76, 0x69, 0x63, 0x65,
    ];

    // Full golden Identify RESPONSE frame (114 bytes), docs/dcp-golden-frames.md.
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
        assert!(!f.all_selector);
    }

    #[test]
    fn build_response_is_byte_exact() {
        let dst = MacAddr([0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f]);
        let src = MacAddr([0x02, 0xc0, 0xa8, 0x01, 0x0f, 0x02]);
        let frame = build_identify_response(dst, src, 0x0300_0152, &golden_props());
        assert_eq!(frame, RESP_FRAME);
    }
}

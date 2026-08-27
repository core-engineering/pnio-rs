//! PROFINET DCP (Discovery & Configuration Protocol) — device side.

pub mod block;
pub mod frame;
pub mod identify;
pub mod set;

pub use block::{parse_blocks, write_blocks, DcpBlock};
pub use frame::{DcpHeader, FrameId, ServiceId, ServiceType, DCP_MULTICAST_MAC};
pub use identify::{
    build_identify_response, parse_identify_request, DeviceProperties, IdentifyFilter,
};
pub use set::{
    build_set_response, decide_set, parse_set_request, SetBlock, SetBlockError, SetRequest,
};

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
    #[error("malformed DCP frame: {0}")]
    Malformed(&'static str),
}

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
    let (eth, payload_off) =
        EthHeader::parse(frame).map_err(|_| DcpError::Malformed("bad Ethernet header"))?;
    if eth.ethertype != ETHERTYPE_PROFINET {
        return Ok(None);
    }
    let payload = &frame[payload_off..];
    if payload.len() < 2 {
        return Err(DcpError::TooShort {
            need: payload_off + 2,
            have: frame.len(),
        });
    }
    let frame_id = u16::from_be_bytes([payload[0], payload[1]]);
    match FrameId::from_u16(frame_id) {
        Some(FrameId::IdentifyRequest) => {
            let (header, blocks) = DcpHeader::parse(&payload[2..])?;
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
        Some(FrameId::GetSet) => {
            let (header, blocks) = DcpHeader::parse(&payload[2..])?;
            if header.service_id != ServiceId::Set || header.service_type != ServiceType::Request {
                log::debug!(
                    "ignoring DCP Get/Set-response frame (service_id={:?}, service_type={:?})",
                    header.service_id,
                    header.service_type
                );
                return Ok(None);
            }
            let req = parse_set_request(blocks)?;
            let results = decide_set(&req, cfg.properties.ip);
            for (option, suboption, err) in &results {
                log::info!("DCP Set block ({option}, {suboption}) -> {err:?}");
            }
            Ok(Some(build_set_response(
                eth.src, cfg.mac, header.xid, &results,
            )))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use crate::dcp::identify::DeviceProperties;
    use crate::eth::MacAddr;

    // Golden Identify REQUEST frame (56 bytes), docs/dcp-golden-frames.md.
    const REQ_FRAME: &[u8] = &[
        0x01, 0x0e, 0xcf, 0x00, 0x00, 0x00, 0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f, 0x88, 0x92, 0xfe,
        0xfe, 0x05, 0x00, 0x03, 0x00, 0x01, 0x52, 0x00, 0x01, 0x00, 0x0c, 0x02, 0x02, 0x00, 0x08,
        0x69, 0x2d, 0x64, 0x65, 0x76, 0x69, 0x63, 0x65, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
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
        let resp = handle_dcp_frame(REQ_FRAME, &cfg())
            .unwrap()
            .expect("expected a response");
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

    #[test]
    fn ignores_empty_identify_request() {
        // No filter blocks at all -> not identify-all (that needs an explicit AllSelector) -> silent.
        let f = req_frame(&[]);
        assert_eq!(handle_dcp_frame(&f, &cfg()).unwrap(), None);
    }
}

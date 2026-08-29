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
            return Err(DcpError::TooShort {
                need: 10,
                have: buf.len(),
            });
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
            return Err(DcpError::TooShort {
                need,
                have: buf.len(),
            });
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
        assert_eq!(
            out,
            vec![0x05, 0x01, 0x03, 0x00, 0x01, 0x52, 0x00, 0x00, 0x00, 0x58]
        );
    }

    #[test]
    fn parse_too_short() {
        assert!(matches!(
            DcpHeader::parse(&[0x05, 0x00]),
            Err(DcpError::TooShort { .. })
        ));
    }

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
            0x05, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
            0x0c, // header, data_length=12
            0x02, 0x02, 0x00, 0x08, // only 4 of 12 block bytes
        ];
        assert!(matches!(
            DcpHeader::parse(buf),
            Err(DcpError::TooShort { need: 22, have: 14 })
        ));
    }
}

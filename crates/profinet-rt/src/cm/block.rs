//! PNIO block codec: the 6-byte block header shared by every PNIO block, and the
//! parsers for the Connect-request block bodies (ARBlockReq, IOCRBlockReq,
//! ExpectedSubmoduleBlockReq, AlarmCRBlockReq).

use super::BlockError;
use crate::eth::MacAddr;
use crate::rpc::{Drep, Uuid};

/// Block type constants (`BlockType` field), request and response.
pub mod ty {
    pub const AR_BLOCK_REQ: u16 = 0x0101;
    pub const IOCR_BLOCK_REQ: u16 = 0x0102;
    pub const ALARM_CR_BLOCK_REQ: u16 = 0x0103;
    pub const EXPECTED_SUBMODULE_BLOCK_REQ: u16 = 0x0104;
    pub const IOD_WRITE_REQ_HEADER: u16 = 0x0008;
    pub const IOD_CONTROL_REQ_PRM_END: u16 = 0x0110;
    pub const IOX_BLOCK_REQ_APP_READY: u16 = 0x0112;
    pub const RELEASE_BLOCK_REQ: u16 = 0x0114;
    pub const AR_BLOCK_RES: u16 = 0x8101;
    pub const IOCR_BLOCK_RES: u16 = 0x8102;
    pub const ALARM_CR_BLOCK_RES: u16 = 0x8103;
    pub const AR_SERVER_BLOCK_RES: u16 = 0x8106;
    pub const IOD_WRITE_RES_HEADER: u16 = 0x8008;
    pub const IOD_CONTROL_RES_PRM_END: u16 = 0x8110;
    pub const IOX_BLOCK_RES_APP_READY: u16 = 0x8112;
    pub const RELEASE_BLOCK_RES: u16 = 0x8114;
}

// ---------------------------------------------------------------------------------
// Cursor: a small big-endian reader over a byte slice, used by every block parser.
// ---------------------------------------------------------------------------------

/// A big-endian cursor over a block body. Every accessor returns `BlockError::TooShort`
/// on overrun instead of panicking.
pub struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Cursor { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn need(&self, n: usize) -> Result<(), BlockError> {
        if self.pos + n > self.buf.len() {
            Err(BlockError::TooShort {
                need: self.pos + n,
                have: self.buf.len(),
            })
        } else {
            Ok(())
        }
    }

    pub fn u8(&mut self) -> Result<u8, BlockError> {
        self.need(1)?;
        let v = self.buf[self.pos];
        self.pos += 1;
        Ok(v)
    }

    pub fn u16(&mut self) -> Result<u16, BlockError> {
        self.need(2)?;
        let v = u16::from_be_bytes([self.buf[self.pos], self.buf[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    pub fn u32(&mut self) -> Result<u32, BlockError> {
        self.need(4)?;
        let v = u32::from_be_bytes(self.buf[self.pos..self.pos + 4].try_into().unwrap());
        self.pos += 4;
        Ok(v)
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], BlockError> {
        self.need(n)?;
        let v = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(v)
    }

    pub fn uuid(&mut self) -> Result<Uuid, BlockError> {
        let b = self.bytes(16)?;
        Uuid::read(b, Drep::BIG).ok_or(BlockError::Malformed("uuid"))
    }

    pub fn mac(&mut self) -> Result<MacAddr, BlockError> {
        let b = self.bytes(6)?;
        Ok(MacAddr([b[0], b[1], b[2], b[3], b[4], b[5]]))
    }
}

// ---------------------------------------------------------------------------------
// BlockHeader
// ---------------------------------------------------------------------------------

/// The 6-byte header shared by every PNIO block: type, length, and version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockHeader {
    pub block_type: u16,
    pub block_length: u16,
    pub version_high: u8,
    pub version_low: u8,
}

impl BlockHeader {
    pub const LEN: usize = 6;

    /// Parse one block header and return it along with its body (`block_length - 2`
    /// bytes after the version, i.e. everything but the 2 version bytes the length
    /// field counts).
    pub fn parse(buf: &[u8]) -> Result<(BlockHeader, &[u8]), BlockError> {
        if buf.len() < Self::LEN {
            return Err(BlockError::TooShort {
                need: Self::LEN,
                have: buf.len(),
            });
        }
        let block_type = u16::from_be_bytes([buf[0], buf[1]]);
        let block_length = u16::from_be_bytes([buf[2], buf[3]]);
        let version_high = buf[4];
        let version_low = buf[5];
        if (version_high, version_low) != (1, 0) {
            return Err(BlockError::BadVersion(version_high, version_low));
        }
        let available = buf.len() - Self::LEN;
        let body_len = (block_length as usize).saturating_sub(2);
        if body_len > available {
            return Err(BlockError::BadLength {
                declared: block_length,
                available,
            });
        }
        let header = BlockHeader {
            block_type,
            block_length,
            version_high,
            version_low,
        };
        Ok((header, &buf[Self::LEN..Self::LEN + body_len]))
    }

    /// Write a block header for a body of `body_len` bytes (version 1.0).
    pub fn write(out: &mut Vec<u8>, block_type: u16, body_len: u16) {
        out.extend_from_slice(&block_type.to_be_bytes());
        out.extend_from_slice(&(body_len + 2).to_be_bytes());
        out.push(1);
        out.push(0);
    }

    /// Parse consecutive blocks until `buf` is exhausted.
    pub fn read_all(buf: &[u8]) -> Result<Vec<(BlockHeader, &[u8])>, BlockError> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos < buf.len() {
            let (header, body) = BlockHeader::parse(&buf[pos..])?;
            pos += BlockHeader::LEN + body.len();
            out.push((header, body));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------------
// ARBlockReq
// ---------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ArBlockReq {
    pub ar_type: u16,
    pub ar_uuid: Uuid,
    pub session_key: u16,
    pub initiator_mac: MacAddr,
    pub initiator_object_uuid: Uuid,
    pub ar_properties: u32,
    pub activity_timeout_factor: u16,
    pub initiator_udp_rt_port: u16,
    pub station_name: String,
}

impl ArBlockReq {
    /// `body` = the bytes after the 6-byte block header.
    pub fn parse(body: &[u8]) -> Result<Self, BlockError> {
        let mut c = Cursor::new(body);
        let ar_type = c.u16()?;
        let ar_uuid = c.uuid()?;
        let session_key = c.u16()?;
        let initiator_mac = c.mac()?;
        let initiator_object_uuid = c.uuid()?;
        let ar_properties = c.u32()?;
        let activity_timeout_factor = c.u16()?;
        let initiator_udp_rt_port = c.u16()?;
        let name_len = c.u16()? as usize;
        let name_bytes = c.bytes(name_len)?;
        let station_name = String::from_utf8(name_bytes.to_vec())
            .map_err(|_| BlockError::Malformed("station name not valid utf-8"))?;
        Ok(ArBlockReq {
            ar_type,
            ar_uuid,
            session_key,
            initiator_mac,
            initiator_object_uuid,
            ar_properties,
            activity_timeout_factor,
            initiator_udp_rt_port,
            station_name,
        })
    }
}

// ---------------------------------------------------------------------------------
// IOCRBlockReq
// ---------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct IocrBlockReq {
    pub iocr_type: u16,
    pub reference: u16,
    pub lt: u16,
    pub properties: u32,
    pub data_length: u16,
    pub frame_id: u16,
    pub send_clock_factor: u16,
    pub reduction_ratio: u16,
    pub phase: u16,
    pub sequence: u16,
    pub frame_send_offset: u32,
    pub watchdog_factor: u16,
    pub data_hold_factor: u16,
    pub tag_header: u16,
    pub multicast_mac: MacAddr,
    pub apis: Vec<IocrApi>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IocrApi {
    pub api: u32,
    pub io_data: Vec<IocrObject>,
    pub iocs: Vec<IocrObject>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IocrObject {
    pub slot: u16,
    pub subslot: u16,
    pub frame_offset: u16,
}

impl IocrBlockReq {
    /// `body` = the bytes after the 6-byte block header.
    pub fn parse(body: &[u8]) -> Result<Self, BlockError> {
        let mut c = Cursor::new(body);
        let iocr_type = c.u16()?;
        let reference = c.u16()?;
        let lt = c.u16()?;
        let properties = c.u32()?;
        let data_length = c.u16()?;
        let frame_id = c.u16()?;
        let send_clock_factor = c.u16()?;
        let reduction_ratio = c.u16()?;
        let phase = c.u16()?;
        let sequence = c.u16()?;
        let frame_send_offset = c.u32()?;
        let watchdog_factor = c.u16()?;
        let data_hold_factor = c.u16()?;
        let tag_header = c.u16()?;
        let multicast_mac = c.mac()?;
        let number_of_apis = c.u16()?;
        let mut apis = Vec::with_capacity(number_of_apis as usize);
        for _ in 0..number_of_apis {
            let api = c.u32()?;
            let number_of_io_data = c.u16()?;
            let mut io_data = Vec::with_capacity(number_of_io_data as usize);
            for _ in 0..number_of_io_data {
                io_data.push(IocrObject {
                    slot: c.u16()?,
                    subslot: c.u16()?,
                    frame_offset: c.u16()?,
                });
            }
            let number_of_iocs = c.u16()?;
            let mut iocs = Vec::with_capacity(number_of_iocs as usize);
            for _ in 0..number_of_iocs {
                iocs.push(IocrObject {
                    slot: c.u16()?,
                    subslot: c.u16()?,
                    frame_offset: c.u16()?,
                });
            }
            apis.push(IocrApi { api, io_data, iocs });
        }
        Ok(IocrBlockReq {
            iocr_type,
            reference,
            lt,
            properties,
            data_length,
            frame_id,
            send_clock_factor,
            reduction_ratio,
            phase,
            sequence,
            frame_send_offset,
            watchdog_factor,
            data_hold_factor,
            tag_header,
            multicast_mac,
            apis,
        })
    }
}

// ---------------------------------------------------------------------------------
// ExpectedSubmoduleBlockReq
// ---------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ExpectedSubmoduleBlockReq {
    pub apis: Vec<ExpectedApi>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExpectedApi {
    pub api: u32,
    pub slot: u16,
    pub module_ident: u32,
    pub module_properties: u16,
    pub submodules: Vec<ExpectedSubmodule>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExpectedSubmodule {
    pub subslot: u16,
    pub submodule_ident: u32,
    pub properties: u16,
    pub input: Option<DataDescription>,
    pub output: Option<DataDescription>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DataDescription {
    pub data_length: u16,
    pub length_iocs: u8,
    pub length_iops: u8,
}

/// `DataDescription` tag values (the `data_description` u16 preceding each descriptor).
const DATA_DESCRIPTION_INPUT: u16 = 1;
const DATA_DESCRIPTION_OUTPUT: u16 = 2;

impl ExpectedSubmoduleBlockReq {
    /// `body` = the bytes after the 6-byte block header.
    pub fn parse(body: &[u8]) -> Result<Self, BlockError> {
        let mut c = Cursor::new(body);
        let number_of_apis = c.u16()?;
        let mut apis = Vec::with_capacity(number_of_apis as usize);
        for _ in 0..number_of_apis {
            let api = c.u32()?;
            let slot = c.u16()?;
            let module_ident = c.u32()?;
            let module_properties = c.u16()?;
            let number_of_submodules = c.u16()?;
            let mut submodules = Vec::with_capacity(number_of_submodules as usize);
            for _ in 0..number_of_submodules {
                let subslot = c.u16()?;
                let submodule_ident = c.u32()?;
                let properties = c.u16()?;
                // Ground truth from the pinned capture (frame 50, verified with tshark):
                // Type 0 ("no input and no output data", e.g. the DAP's own subslots) still
                // carries exactly one Input DataDescription with SubmoduleDataLength = 0 — it
                // is NOT omitted. So Input is read for Type 0 and 1, Output for Type 2, both
                // for Type 3.
                let (input, output) = match properties & 0x3 {
                    0 | 1 => (
                        Some(Self::data_description(&mut c, DATA_DESCRIPTION_INPUT)?),
                        None,
                    ),
                    2 => (
                        None,
                        Some(Self::data_description(&mut c, DATA_DESCRIPTION_OUTPUT)?),
                    ),
                    _ => {
                        let input = Self::data_description(&mut c, DATA_DESCRIPTION_INPUT)?;
                        let output = Self::data_description(&mut c, DATA_DESCRIPTION_OUTPUT)?;
                        (Some(input), Some(output))
                    }
                };
                submodules.push(ExpectedSubmodule {
                    subslot,
                    submodule_ident,
                    properties,
                    input,
                    output,
                });
            }
            apis.push(ExpectedApi {
                api,
                slot,
                module_ident,
                module_properties,
                submodules,
            });
        }
        Ok(ExpectedSubmoduleBlockReq { apis })
    }

    fn data_description(c: &mut Cursor, expected_tag: u16) -> Result<DataDescription, BlockError> {
        let tag = c.u16()?;
        if tag != expected_tag {
            return Err(BlockError::Malformed("data description tag"));
        }
        let data_length = c.u16()?;
        let length_iocs = c.u8()?;
        let length_iops = c.u8()?;
        Ok(DataDescription {
            data_length,
            length_iocs,
            length_iops,
        })
    }
}

// ---------------------------------------------------------------------------------
// AlarmCRBlockReq
// ---------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct AlarmCrBlockReq {
    pub alarm_cr_type: u16,
    pub lt: u16,
    pub properties: u32,
    pub rta_timeout_factor: u16,
    pub rta_retries: u16,
    pub local_alarm_reference: u16,
    pub max_alarm_data_length: u16,
    pub tag_header_high: u16,
    pub tag_header_low: u16,
}

impl AlarmCrBlockReq {
    /// `body` = the bytes after the 6-byte block header.
    pub fn parse(body: &[u8]) -> Result<Self, BlockError> {
        let mut c = Cursor::new(body);
        let alarm_cr_type = c.u16()?;
        let lt = c.u16()?;
        let properties = c.u32()?;
        let rta_timeout_factor = c.u16()?;
        let rta_retries = c.u16()?;
        let local_alarm_reference = c.u16()?;
        let max_alarm_data_length = c.u16()?;
        let tag_header_high = c.u16()?;
        let tag_header_low = c.u16()?;
        Ok(AlarmCrBlockReq {
            alarm_cr_type,
            lt,
            properties,
            rta_timeout_factor,
            rta_retries,
            local_alarm_reference,
            max_alarm_data_length,
            tag_header_high,
            tag_header_low,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::golden;

    const BLOCKS: usize = 142;

    fn connect_blocks() -> Vec<u8> {
        golden("connect_req")[BLOCKS..].to_vec()
    }

    #[test]
    fn read_all_connect_blocks_in_order() {
        let b = connect_blocks();
        let all = BlockHeader::read_all(&b).unwrap();
        let types: Vec<u16> = all.iter().map(|(h, _)| h.block_type).collect();
        assert_eq!(
            types,
            vec![0x0101, 0x0102, 0x0102, 0x0104, 0x0104, 0x0104, 0x0104, 0x0104, 0x0103]
        );
        assert_eq!(all[0].0.block_length, 91);
        assert_eq!(all[0].1.len(), 89);
    }

    #[test]
    fn parse_ar_block_req() {
        let b = connect_blocks();
        let (h, body) = BlockHeader::parse(&b).unwrap();
        assert_eq!(h.block_type, ty::AR_BLOCK_REQ);
        let ar = ArBlockReq::parse(body).unwrap();
        assert_eq!(ar.ar_type, 1);
        assert_eq!(
            ar.ar_uuid.to_string(),
            "e5e1aecc-b133-4b4d-b187-cc68b0211ed2"
        );
        assert_eq!(ar.session_key, 2);
        assert_eq!(ar.initiator_mac.0, [0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f]);
        assert_eq!(
            ar.initiator_object_uuid.to_string(),
            "dea00000-6c97-11d1-8271-1064010e002a"
        );
        assert_eq!(ar.ar_properties, 0x4000_0011);
        assert_eq!(ar.activity_timeout_factor, 200);
        assert_eq!(ar.initiator_udp_rt_port, 0x8892);
        assert_eq!(ar.station_name, "plcxbbench.profinetxainterfacexb25fbd");
    }

    #[test]
    fn parse_iocr_blocks() {
        let b = connect_blocks();
        let all = BlockHeader::read_all(&b).unwrap();
        let input = IocrBlockReq::parse(all[1].1).unwrap();
        assert_eq!(
            (input.iocr_type, input.reference, input.frame_id),
            (1, 1, 0x8000)
        );
        assert_eq!(
            (
                input.data_length,
                input.send_clock_factor,
                input.reduction_ratio
            ),
            (40, 32, 32)
        );
        assert_eq!(
            (
                input.watchdog_factor,
                input.data_hold_factor,
                input.tag_header
            ),
            (3, 3, 0xc000)
        );
        assert_eq!(input.frame_send_offset, 0xffff_ffff);
        assert_eq!(input.apis.len(), 1);
        assert_eq!(input.apis[0].io_data.len(), 6);
        assert_eq!(input.apis[0].iocs.len(), 3);
        assert_eq!(
            input.apis[0].io_data[5],
            IocrObject {
                slot: 4,
                subslot: 1,
                frame_offset: 9
            }
        );
        assert_eq!(
            input.apis[0].iocs[2],
            IocrObject {
                slot: 4,
                subslot: 1,
                frame_offset: 18
            }
        );
        let output = IocrBlockReq::parse(all[2].1).unwrap();
        assert_eq!(
            (output.iocr_type, output.reference, output.frame_id),
            (2, 2, 0x8001)
        );
        assert_eq!(
            (output.apis[0].io_data.len(), output.apis[0].iocs.len()),
            (3, 6)
        );
    }

    #[test]
    fn parse_expected_submodules() {
        let b = connect_blocks();
        let all = BlockHeader::read_all(&b).unwrap();
        let dap = ExpectedSubmoduleBlockReq::parse(all[3].1).unwrap();
        assert_eq!(dap.apis[0].slot, 0);
        assert_eq!(dap.apis[0].module_ident, 0x1);
        assert_eq!(dap.apis[0].submodules.len(), 3);
        assert_eq!(dap.apis[0].submodules[1].subslot, 0x8000);
        assert_eq!(dap.apis[0].submodules[1].submodule_ident, 0x8000);
        assert_eq!(
            dap.apis[0].submodules[0].input,
            Some(DataDescription {
                data_length: 0,
                length_iocs: 1,
                length_iops: 1
            })
        );
        assert_eq!(dap.apis[0].submodules[0].output, None);
        let echo = ExpectedSubmoduleBlockReq::parse(all[7].1).unwrap();
        let sm = &echo.apis[0].submodules[0];
        assert_eq!(
            (
                echo.apis[0].slot,
                echo.apis[0].module_ident,
                sm.submodule_ident,
                sm.properties
            ),
            (4, 0x40, 0x140, 3)
        );
        assert_eq!(sm.input.unwrap().data_length, 8);
        assert_eq!(sm.output.unwrap().data_length, 8);
        let out_only = ExpectedSubmoduleBlockReq::parse(all[5].1).unwrap();
        let sm = &out_only.apis[0].submodules[0];
        assert_eq!((sm.input, sm.output.unwrap().data_length), (None, 1));
    }

    #[test]
    fn parse_alarm_cr() {
        let b = connect_blocks();
        let all = BlockHeader::read_all(&b).unwrap();
        let a = AlarmCrBlockReq::parse(all[8].1).unwrap();
        assert_eq!((a.alarm_cr_type, a.lt, a.properties), (1, 0x8892, 0));
        assert_eq!(
            (a.rta_timeout_factor, a.rta_retries, a.local_alarm_reference),
            (1, 3, 0)
        );
        assert_eq!(
            (a.max_alarm_data_length, a.tag_header_high, a.tag_header_low),
            (256, 0xc000, 0xa000)
        );
    }

    #[test]
    fn header_errors() {
        assert!(matches!(
            BlockHeader::parse(&[1, 1, 0, 5]),
            Err(BlockError::TooShort { .. })
        ));
        assert!(matches!(
            BlockHeader::parse(&[1, 1, 0, 5, 2, 0, 0, 0, 0]),
            Err(BlockError::BadVersion(2, 0))
        ));
        assert!(matches!(
            BlockHeader::parse(&[1, 1, 0, 9, 1, 0, 0, 0]),
            Err(BlockError::BadLength {
                declared: 9,
                available: 2
            })
        ));
        let mut out = Vec::new();
        BlockHeader::write(&mut out, ty::AR_BLOCK_RES, 28);
        assert_eq!(out, vec![0x81, 0x01, 0x00, 0x1e, 0x01, 0x00]);
    }

    #[test]
    fn truncated_ar_block_is_too_short() {
        let b = connect_blocks();
        assert!(matches!(
            ArBlockReq::parse(&b[6..40]),
            Err(BlockError::TooShort { .. })
        ));
    }
}

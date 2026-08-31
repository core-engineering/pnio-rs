//! IODWriteReq / MultipleWrite parsing and the byte-exact IODWriteRes response.
//!
//! At Connect time, the CPU stacks several parameter-write records inside one
//! `MultipleWrite` container (index `0xe040`): an outer `IODWriteReqHeader` whose
//! data holds nested `IODWriteReqHeader + data` records back to back, each padded
//! to the next 4-byte boundary. The response mirrors the request one-for-one, in
//! request order, each response record a fixed 64 bytes with `status = 0`.

use super::{ty, BlockError, BlockHeader, CmError, Cursor};
use crate::rpc::{Drep, Uuid};

/// The index of the `MultipleWrite` container record (IEC 61158-6-10 §4.8.5.3).
pub const INDEX_MULTIPLE_WRITE: u16 = 0xe040;

/// One `IODWriteReqHeader` record: fixed fields plus its `data[record_data_length]`
/// payload (the raw record data, before any nested parsing).
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    /// `Sequence Number`, echoed back unchanged in the matching response record.
    pub seq: u16,
    /// The AR this record is written against.
    pub ar_uuid: Uuid,
    /// Application Process Identifier.
    pub api: u32,
    /// Target slot (`0xffff` on the outer `MultipleWrite` record).
    pub slot: u16,
    /// Target subslot (`0xffff` on the outer `MultipleWrite` record).
    pub subslot: u16,
    /// The record data index being written (e.g. [`INDEX_MULTIPLE_WRITE`], or an I&M index).
    pub index: u16,
    /// The record's raw payload, `record_data_length` bytes, unparsed.
    pub data: Vec<u8>,
}

/// A Write request's records, in request order. When the first record's index is
/// `INDEX_MULTIPLE_WRITE` it is kept as `records[0]` (with its raw data) and the
/// nested records parsed out of that data follow it; otherwise `records` holds the
/// single record.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteReq {
    /// The request's records, in request order (see the type's doc for `MultipleWrite`
    /// unwrapping).
    pub records: Vec<Record>,
}

impl WriteReq {
    /// Parse the concatenated PNIO blocks of a Write request (no RPC header/NDR).
    pub fn parse(blocks: &[u8]) -> Result<WriteReq, CmError> {
        let (outer, _) = parse_one(blocks)?;
        let mut records = Vec::new();
        if outer.index == INDEX_MULTIPLE_WRITE {
            let data = outer.data.clone();
            records.push(outer);
            let mut pos = 0usize;
            while pos < data.len() {
                let (record, consumed) = parse_one(&data[pos..])?;
                records.push(record);
                pos = (pos + consumed + 3) & !3;
            }
        } else {
            records.push(outer);
        }
        Ok(WriteReq { records })
    }
}

/// Parse one `IODWriteReqHeader` block (header + fixed body + `data[record_data_length]`)
/// starting at `buf[0]`. Returns the record and the number of bytes consumed, including
/// the trailing data (the data is not counted in the block's own declared length).
fn parse_one(buf: &[u8]) -> Result<(Record, usize), BlockError> {
    let (header, body) = BlockHeader::parse(buf)?;
    if header.block_type != ty::IOD_WRITE_REQ_HEADER {
        return Err(BlockError::UnexpectedType {
            expected: ty::IOD_WRITE_REQ_HEADER,
            got: header.block_type,
        });
    }
    let mut c = Cursor::new(body);
    let seq = c.u16()?;
    let ar_uuid = c.uuid()?;
    let api = c.u32()?;
    let slot = c.u16()?;
    let subslot = c.u16()?;
    let _pad = c.u16()?;
    let index = c.u16()?;
    let record_data_length = c.u32()? as usize;
    let _padding = c.bytes(24)?;

    let header_end = BlockHeader::LEN + body.len();
    let data_end = match header_end.checked_add(record_data_length) {
        Some(end) if end <= buf.len() => end,
        _ => {
            return Err(BlockError::TooShort {
                need: header_end.saturating_add(record_data_length),
                have: buf.len(),
            })
        }
    };
    let data = buf[header_end..data_end].to_vec();
    let consumed = data_end;

    Ok((
        Record {
            seq,
            ar_uuid,
            api,
            slot,
            subslot,
            index,
            data,
        },
        consumed,
    ))
}

/// Build the Write response's PNIO blocks: one `IODWriteResHeader` per request record,
/// in request order, each 64 bytes (`record_data_length = 0`, `additional_value_1/2 = 0`,
/// `status = 0`).
pub fn build_write_res(req: &WriteReq) -> Vec<u8> {
    let mut out = Vec::with_capacity(req.records.len() * 64);
    for r in &req.records {
        BlockHeader::write(&mut out, ty::IOD_WRITE_RES_HEADER, 58);
        out.extend_from_slice(&r.seq.to_be_bytes());
        r.ar_uuid.write(&mut out, Drep::BIG);
        out.extend_from_slice(&r.api.to_be_bytes());
        out.extend_from_slice(&r.slot.to_be_bytes());
        out.extend_from_slice(&r.subslot.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes()); // pad
        out.extend_from_slice(&r.index.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes()); // record_data_length
        out.extend_from_slice(&0u16.to_be_bytes()); // additional_value_1
        out.extend_from_slice(&0u16.to_be_bytes()); // additional_value_2
        out.extend_from_slice(&0u32.to_be_bytes()); // status
        out.extend_from_slice(&[0u8; 16]); // padding
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::golden;

    const BLOCKS: usize = 142;

    #[test]
    fn parse_multiple_write_records() {
        let w = WriteReq::parse(&golden("write_req")[BLOCKS..]).unwrap();
        let idx: Vec<u16> = w.records.iter().map(|r| r.index).collect();
        assert_eq!(idx, vec![0xe040, 0x8071, 0x7b, 0x7c, 0x7d]);
        assert_eq!(w.records[0].data.len(), 280);
        assert_eq!(
            (w.records[0].api, w.records[0].slot, w.records[0].subslot),
            (0xffff_ffff, 0xffff, 0xffff)
        );
        assert_eq!((w.records[1].slot, w.records[1].subslot), (0, 0x8000));
        assert_eq!(
            w.records[1].data,
            vec![0x02, 0x50, 0x00, 0x08, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01]
        );
        assert_eq!(
            (w.records[2].slot, w.records[2].subslot, w.records[2].seq),
            (3, 1, 2)
        );
        assert_eq!(w.records[4].data, vec![0, 0, 0, 2]);
        assert_eq!(
            w.records[3].ar_uuid.to_string(),
            "e5e1aecc-b133-4b4d-b187-cc68b0211ed2"
        );
    }

    #[test]
    fn write_response_is_byte_exact() {
        let w = WriteReq::parse(&golden("write_req")[BLOCKS..]).unwrap();
        let out = build_write_res(&w);
        assert_eq!(out, &golden("write_res")[BLOCKS..]);
        assert_eq!(out.len(), 320);
    }

    #[test]
    fn single_record_write() {
        // hand-built: one record, index 0x7b, 4 data bytes
        let mut b = Vec::new();
        b.extend_from_slice(&[0x00, 0x08, 0x00, 0x3c, 0x01, 0x00, 0x00, 0x05]);
        b.extend_from_slice(&[0x11; 16]); // ar_uuid
        b.extend_from_slice(&[0, 0, 0, 0, 0, 3, 0, 1, 0, 0, 0, 0x7b, 0, 0, 0, 4]);
        b.extend_from_slice(&[0; 24]);
        b.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        let w = WriteReq::parse(&b).unwrap();
        assert_eq!(w.records.len(), 1);
        assert_eq!(w.records[0].data, vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(build_write_res(&w).len(), 64);
    }

    #[test]
    fn huge_record_length_is_rejected_not_panic() {
        // Same hand-built buffer as `single_record_write`, but with
        // `record_data_length` set to `0xffff_ffff` — must be rejected with a typed
        // error, not panic via overflow/inverted-range slicing on 32-bit `usize`.
        let mut b = Vec::new();
        b.extend_from_slice(&[0x00, 0x08, 0x00, 0x3c, 0x01, 0x00, 0x00, 0x05]);
        b.extend_from_slice(&[0x11; 16]); // ar_uuid
        b.extend_from_slice(&[
            0, 0, 0, 0, 0, 3, 0, 1, 0, 0, 0, 0x7b, 0xff, 0xff, 0xff, 0xff,
        ]);
        b.extend_from_slice(&[0; 24]);
        b.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        assert!(matches!(WriteReq::parse(&b), Err(CmError::Block(_))));
    }

    #[test]
    fn truncated_data_is_an_error() {
        let b = &golden("write_req")[BLOCKS..BLOCKS + 100];
        assert!(matches!(WriteReq::parse(b), Err(CmError::Block(_))));
    }

    #[test]
    fn wrong_block_type_is_rejected() {
        let b = &golden("prmend_req")[BLOCKS..];
        assert!(matches!(
            WriteReq::parse(b),
            Err(CmError::Block(BlockError::UnexpectedType { .. }))
        ));
    }
}

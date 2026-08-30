//! `IODReadReq`/`IODReadRes`: I&M record reads served over `Read` (opnum 2) and
//! `ReadImplicit` (opnum 5), plus the I&M1-3 side of a parameter Write.
//!
//! Which submodule answers which index (from the p-net alarm/I&M capture, see
//! `docs/alarm-golden-frames.md`): every submodule the model knows answers I&M0
//! (`0xAFF0`) with the same `IM_Supported = 0x000E` — the capture shows the interface
//! submodule (slot 0, subslot 0x8000) answering exactly what the DAP (slot 0,
//! subslot 1) answers — and every known submodule also reads and writes I&M1-3
//! (`0xAFF1..=0xAFF3`) from the one device-wide [`ImStore`].

use super::model::DeviceModel;
use super::{ty, BlockError, BlockHeader, CmError, Cursor, PnioStatus, Record};
use crate::im::{encode_im0, Im0, ImStore, IM_SUPPORTED_DAP, INDEX_IM0, INDEX_IM1, INDEX_IM3};
use crate::rpc::{Drep, Uuid};

/// One `IODReadReqHeader`/`IODReadImplicitReqHeader` request (58-byte body after the
/// 6-byte block header): identifies the record to read by `(api, slot, subslot,
/// index)`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadReq {
    pub seq: u16,
    pub ar_uuid: Uuid,
    pub api: u32,
    pub slot: u16,
    pub subslot: u16,
    pub index: u16,
    pub record_data_length: u32,
    pub target_ar_uuid: Uuid,
}

impl ReadReq {
    /// Parse the concatenated PNIO blocks of a Read/ReadImplicit request (no RPC
    /// header/NDR): exactly one `IODReadReqHeader` (`0x0009`) — the same block shape
    /// serves both `Read` and `ReadImplicit`.
    pub fn parse(blocks: &[u8]) -> Result<ReadReq, CmError> {
        let (header, body) = BlockHeader::parse(blocks)?;
        if header.block_type != ty::IOD_READ_REQ_HEADER {
            return Err(CmError::Block(BlockError::UnexpectedType {
                expected: ty::IOD_READ_REQ_HEADER,
                got: header.block_type,
            }));
        }
        let mut c = Cursor::new(body);
        let seq = c.u16()?;
        let ar_uuid = c.uuid()?;
        let api = c.u32()?;
        let slot = c.u16()?;
        let subslot = c.u16()?;
        let _pad = c.u16()?;
        let index = c.u16()?;
        let record_data_length = c.u32()?;
        let target_ar_uuid = c.uuid()?;
        let _pad = c.bytes(8)?;
        Ok(ReadReq {
            seq,
            ar_uuid,
            api,
            slot,
            subslot,
            index,
            record_data_length,
            target_ar_uuid,
        })
    }
}

/// Build the Read response's PNIO blocks: one `IODReadResHeader` (`0x8009`) whose
/// fixed fields echo the request, `record_data_length = data.len()`, followed
/// immediately by `data` (the record itself, its own block header included).
pub fn build_read_res(req: &ReadReq, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + data.len());
    BlockHeader::write(&mut out, ty::IOD_READ_RES_HEADER, 58);
    out.extend_from_slice(&req.seq.to_be_bytes());
    req.ar_uuid.write(&mut out, Drep::BIG);
    out.extend_from_slice(&req.api.to_be_bytes());
    out.extend_from_slice(&req.slot.to_be_bytes());
    out.extend_from_slice(&req.subslot.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // pad
    out.extend_from_slice(&req.index.to_be_bytes());
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // additional_value_1
    out.extend_from_slice(&0u16.to_be_bytes()); // additional_value_2
    out.extend_from_slice(&[0u8; 20]); // pad
    out.extend_from_slice(data);
    out
}

/// Everything [`read_record`] needs that is not in the request.
pub struct RecordCtx<'a> {
    pub model: &'a DeviceModel,
    pub im0: &'a Im0,
    pub im: &'a ImStore,
}

/// Serve one record read: `Some(record bytes)` for an index we answer on that
/// `(slot, subslot)`, `None` for "invalid index" — including a `(slot, subslot)`
/// absent from the model entirely.
///
/// - `0xAFF0` (I&M0): any submodule the model knows (`DeviceModel::find`), always
///   encoded with `IM_SUPPORTED_DAP` (`0x000E`) — the capture answers the same mask on
///   the interface submodule as on the DAP.
/// - `0xAFF1..=0xAFF3` (I&M1-3): any submodule the model knows, served from the one
///   device-wide [`ImStore`].
pub fn read_record(req: &ReadReq, ctx: &RecordCtx) -> Option<Vec<u8>> {
    ctx.model.find(req.slot, req.subslot)?;
    match req.index {
        INDEX_IM0 => Some(encode_im0(ctx.model.vendor_id, ctx.im0, IM_SUPPORTED_DAP)),
        INDEX_IM1..=INDEX_IM3 => ctx.im.read(req.index),
        _ => None,
    }
}

/// Called for every Write record with index `0xAFF1..=0xAFF3` once the AR has
/// accepted the Write. Every submodule the model knows is writable and they all share
/// the one device-wide [`ImStore`] (matching the read side): a record on a
/// `(slot, subslot)` absent from the model, or one [`ImStore::write`] rejects for its
/// own reasons (bad block header/type/length), answers
/// [`PnioStatus::write_invalid_parameter`]. Not placed on the wire — the Write
/// response keeps the AR's own OK status regardless (per-record statuses are out of
/// scope) — the caller logs a non-OK result instead.
pub fn write_im_record(r: &Record, model: &DeviceModel, im: &mut ImStore) -> PnioStatus {
    if model.find(r.slot, r.subslot).is_none() {
        return PnioStatus::write_invalid_parameter();
    }
    match im.write(r.index, &r.data) {
        Ok(()) => PnioStatus::OK,
        Err(_) => PnioStatus::write_invalid_parameter(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::im::{Im0, ImStore, SwRevision};
    use crate::testutil::{golden_alarm, RPC_OFF};

    const BLOCKS: usize = RPC_OFF + 80 + 20;

    #[test]
    fn read_req_parses_the_cpu_im0_request() {
        let r = ReadReq::parse(&golden_alarm("im0_read_req")[BLOCKS..]).unwrap();
        assert_eq!(
            (r.api, r.slot, r.subslot, r.index, r.record_data_length),
            (0, 0, 1, 0xAFF0, 0x8000)
        );
        let i = ReadReq::parse(&golden_alarm("im0_read_req_if")[BLOCKS..]).unwrap();
        assert_eq!((i.slot, i.subslot), (0, 0x8000));
    }

    #[test]
    fn read_res_matches_the_pnet_response_blocks() {
        let req = ReadReq::parse(&golden_alarm("im0_read_req")[BLOCKS..]).unwrap();
        let im0 = Im0 {
            order_id: "12345 Abcdefghijk".into(),
            serial_number: "007".into(),
            hardware_revision: 3,
            software_revision: SwRevision {
                prefix: 'V',
                functional: 0,
                bug_fix: 2,
                internal: 0,
            },
            revision_counter: 0,
            profile_id: 0x1234,
            profile_specific_type: 0x5678,
        };
        let model = crate::cm::DeviceModel::pnet_sample(crate::eth::MacAddr([
            0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8,
        ]));
        let store = ImStore::new();
        let data = read_record(
            &req,
            &RecordCtx {
                model: &model,
                im0: &im0,
                im: &store,
            },
        )
        .unwrap();
        assert_eq!(
            build_read_res(&req, &data),
            golden_alarm("im0_read_res")[BLOCKS..].to_vec()
        );
    }

    #[test]
    fn im1_on_every_known_submodule_and_unknown_index_is_none() {
        let model = crate::cm::DeviceModel::pnet_sample(crate::eth::MacAddr([0; 6]));
        let (im0, mut store) = (Im0::default(), ImStore::new());
        store.write(INDEX_IM1, &im1_record()).unwrap();
        let ctx = RecordCtx {
            model: &model,
            im0: &im0,
            im: &store,
        };
        let mut req = ReadReq::parse(&golden_alarm("im0_read_req")[BLOCKS..]).unwrap();
        req.index = INDEX_IM1;
        // The DAP, the interface submodule and a real I/O submodule all serve the one
        // device-wide store.
        for (slot, subslot) in [(0u16, 1u16), (0, 0x8000), (1, 1)] {
            req.slot = slot;
            req.subslot = subslot;
            assert_eq!(
                read_record(&req, &ctx).as_deref(),
                Some(&im1_record()[..]),
                "I&M1 on {slot}/{subslot:#x}"
            );
        }
        // A (slot, subslot) the model does not know answers nothing at all.
        req.slot = 99;
        req.subslot = 1;
        assert!(read_record(&req, &ctx).is_none());
        // Neither does an index we do not serve.
        req.slot = 0;
        req.subslot = 1;
        req.index = 0xF840;
        assert!(read_record(&req, &ctx).is_none());
    }

    #[test]
    fn im0_carries_im_supported_on_every_known_submodule() {
        let model = crate::cm::DeviceModel::pnet_sample(crate::eth::MacAddr([0; 6]));
        let (im0, store) = (Im0::default(), ImStore::new());
        let ctx = RecordCtx {
            model: &model,
            im0: &im0,
            im: &store,
        };
        let want = crate::im::encode_im0(model.vendor_id, &im0, IM_SUPPORTED_DAP);
        let mut req = ReadReq::parse(&golden_alarm("im0_read_req")[BLOCKS..]).unwrap();
        for (slot, subslot) in [(0u16, 1u16), (0, 0x8000), (0, 0x8001), (1, 1)] {
            req.slot = slot;
            req.subslot = subslot;
            assert_eq!(
                read_record(&req, &ctx),
                Some(want.clone()),
                "I&M0 on {slot}/{subslot:#x}"
            );
        }
    }

    #[test]
    fn write_im_record_accepts_every_known_submodule_and_rejects_the_rest() {
        let model = crate::cm::DeviceModel::pnet_sample(crate::eth::MacAddr([0; 6]));
        let mut store = ImStore::new();
        let r = Record {
            seq: 0,
            ar_uuid: Uuid::NIL,
            api: 0,
            slot: 0,
            subslot: 1,
            index: INDEX_IM1,
            data: im1_record(),
        };
        // The DAP, the interface submodule and a real I/O submodule all write the one
        // device-wide store.
        for (slot, subslot) in [(0u16, 1u16), (0, 0x8000), (1, 1)] {
            let rec = Record {
                slot,
                subslot,
                ..r.clone()
            };
            assert_eq!(
                write_im_record(&rec, &model, &mut store),
                PnioStatus::OK,
                "I&M1 write on {slot}/{subslot:#x}"
            );
            assert!(store.read(INDEX_IM1).is_some());
        }

        let unknown_slot = Record {
            slot: 99,
            ..r.clone()
        };
        assert_eq!(
            write_im_record(&unknown_slot, &model, &mut store),
            PnioStatus::write_invalid_parameter()
        );

        let bad_shape = Record {
            data: vec![0u8; 3],
            ..r
        };
        assert_eq!(
            write_im_record(&bad_shape, &model, &mut store),
            PnioStatus::write_invalid_parameter()
        );
    }

    /// A minimal well-formed I&M1 record (block header + 54 space-padded bytes).
    fn im1_record() -> Vec<u8> {
        let mut rec = Vec::new();
        crate::cm::block::BlockHeader::write(&mut rec, 0x0021, 54);
        rec.extend_from_slice(&[b' '; 54]);
        rec
    }
}

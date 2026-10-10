//! `IODReadReq`/`IODReadRes`: I&M and PDEV record reads served over `Read` (opnum 2)
//! and `ReadImplicit` (opnum 5), plus the I&M1-3 and PDEV sides of a parameter Write.
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
use crate::pdev::{Pdev, PdevAction};
use crate::rpc::{Drep, Uuid};
use std::time::Instant;

/// Largest PDEV engineering record [`write_pdev_record`] stores, in bytes.
/// `PDExpectedData` (`0xF842`) nests every stored record under 16-bit `BlockLength`s;
/// capping each record keeps those lengths from wrapping.
pub const MAX_PDEV_RECORD_LEN: usize = 4096;

/// One `IODReadReqHeader`/`IODReadImplicitReqHeader` request (58-byte body after the
/// 6-byte block header): identifies the record to read by `(api, slot, subslot,
/// index)`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadReq {
    /// `Sequence Number`, echoed back unchanged in the matching response.
    pub seq: u16,
    /// The AR this record read is issued against.
    pub ar_uuid: Uuid,
    /// Application Process Identifier.
    pub api: u32,
    /// Target slot.
    pub slot: u16,
    /// Target subslot.
    pub subslot: u16,
    /// The record data index being read (e.g. [`crate::im::INDEX_IM0`]).
    pub index: u16,
    /// `RecordDataLength`: the buffer size the requester is prepared to receive.
    pub record_data_length: u32,
    /// `TargetARUUID`, present but unused by this crate's read handling.
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
    /// The device's slot/submodule model, used to check `(slot, subslot)` is known.
    pub model: &'a DeviceModel,
    /// The device identity answered for I&M0 reads.
    pub im0: &'a Im0,
    /// The device-wide writable I&M1-3 store.
    pub im: &'a ImStore,
    /// The port/interface (PDEV) records, when the device runs LLDP; `None` leaves
    /// every PDEV index to the "invalid index" fallback.
    pub pdev: Option<&'a Pdev>,
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
/// - PDEV indices, when `ctx.pdev` is set: whatever [`crate::pdev::records::read`]
///   serves on that `(slot, subslot)`, an empty record included (nothing written yet
///   is a successful empty read, not an invalid index).
pub fn read_record(req: &ReadReq, ctx: &RecordCtx) -> Option<Vec<u8>> {
    ctx.model.find(req.slot, req.subslot)?;
    if let Some(p) = ctx.pdev {
        if let Some(d) = crate::pdev::records::read(p, req.index, req.slot, req.subslot) {
            return Some(d);
        }
    }
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
/// [`PnioStatus::write_invalid_parameter`]. The status is that record's own status in
/// the Write response (`IODWriteResHeader`); the RPC-level status stays OK.
pub fn write_im_record(r: &Record, model: &DeviceModel, im: &mut ImStore) -> PnioStatus {
    if model.find(r.slot, r.subslot).is_none() {
        return PnioStatus::write_invalid_parameter();
    }
    match im.write(r.index, &r.data) {
        Ok(()) => PnioStatus::OK,
        Err(_) => PnioStatus::write_invalid_parameter(),
    }
}

/// Called for every Write record on a PDEV index (`0x802B`, `0x802F`, `0x8071`) once the
/// AR has accepted the Write. `PDPortDataCheck` is parsed and handed to the [`Pdev`]
/// (the resulting diagnosis changes are pushed to `actions`); the Adjust records are
/// stored raw. Answers [`PnioStatus::write_invalid_parameter`] for a record on the
/// wrong slot/subslot (the ones [`crate::pdev::records::read`] serves it back from) or
/// a malformed `PDPortDataCheck`, and
/// [`PnioStatus::write_resource_unavailable`] for one longer than
/// [`MAX_PDEV_RECORD_LEN`]; nothing is stored then.
pub fn write_pdev_record(
    p: &mut Pdev,
    r: &Record,
    now: Instant,
    actions: &mut Vec<PdevAction>,
) -> PnioStatus {
    use crate::pdev::records::{index, parse_check};
    use crate::pdev::{IFACE_SUBSLOT, PORT_SUBSLOT};
    if r.data.len() > MAX_PDEV_RECORD_LEN {
        log::warn!(
            "PDEV record {:#06x} refused: {} bytes, over {MAX_PDEV_RECORD_LEN}",
            r.index,
            r.data.len()
        );
        return PnioStatus::write_resource_unavailable();
    }
    match (r.index, r.slot, r.subslot) {
        (index::PD_PORT_DATA_CHECK, 0, PORT_SUBSLOT) => match parse_check(&r.data) {
            Ok(c) => {
                actions.extend(p.set_check(Some(c), now));
                PnioStatus::OK
            }
            Err(e) => {
                log::warn!("PDPortDataCheck rejected: {e}");
                PnioStatus::write_invalid_parameter()
            }
        },
        (index::PD_PORT_DATA_ADJUST, 0, PORT_SUBSLOT) => {
            p.set_adjust(Some(r.data.clone()));
            PnioStatus::OK
        }
        (index::PD_INTERFACE_ADJUST, 0, IFACE_SUBSLOT) => {
            p.set_iface_adjust(Some(r.data.clone()));
            PnioStatus::OK
        }
        _ => PnioStatus::write_invalid_parameter(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eth::MacAddr;
    use crate::im::{Im0, ImStore, SwRevision};
    use crate::pdev::{MockLink, Pdev};
    use crate::testutil::{golden_alarm, synthetic_check_record, RPC_OFF};
    use std::time::Instant;

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
                pdev: None,
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
            pdev: None,
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
            pdev: None,
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

    #[test]
    fn read_dispatches_pdev_indices_when_a_pdev_is_present() {
        let model = DeviceModel::pnet_sample(MacAddr([0; 6]));
        let (im0, store) = (Im0::default(), ImStore::new());
        let pdev = Pdev::new([0; 6], Box::new(MockLink(true.into())), Instant::now());
        let ctx = RecordCtx {
            model: &model,
            im0: &im0,
            im: &store,
            pdev: Some(&pdev),
        };
        let req = |index, subslot| ReadReq {
            seq: 0,
            ar_uuid: Uuid([0; 16]),
            api: 0,
            slot: 0,
            subslot,
            index,
            record_data_length: 0x8000,
            target_ar_uuid: Uuid([0; 16]),
        };
        assert!(read_record(&req(0x802A, 0x8001), &ctx).is_some());
        assert!(read_record(&req(0x8080, 0x8000), &ctx).is_some());
        assert!(read_record(&req(0xF841, 0x0001), &ctx).is_some());
        let none = RecordCtx { pdev: None, ..ctx };
        assert!(read_record(&req(0x802A, 0x8001), &none).is_none());
    }

    fn pdev_record(subslot: u16, index: u16, data: Vec<u8>) -> Record {
        Record {
            seq: 0,
            ar_uuid: Uuid::NIL,
            api: 0,
            slot: 0,
            subslot,
            index,
            data,
        }
    }

    #[test]
    fn write_pdev_record_stores_each_record_on_its_own_subslot_only() {
        use crate::pdev::records::index;
        let now = Instant::now();
        let mut p = Pdev::new([0; 6], Box::new(MockLink(true.into())), now);
        let mut actions = Vec::new();
        let check = synthetic_check_record("port-001.peer", "peer");
        let r = pdev_record(0x8001, index::PD_PORT_DATA_CHECK, check.clone());
        assert_eq!(
            write_pdev_record(&mut p, &r, now, &mut actions),
            PnioStatus::OK
        );
        assert_eq!(p.check().unwrap().raw, check);
        let r = pdev_record(0x8001, index::PD_PORT_DATA_ADJUST, vec![1, 2, 3, 4]);
        assert_eq!(
            write_pdev_record(&mut p, &r, now, &mut actions),
            PnioStatus::OK
        );
        assert_eq!(p.adjust(), Some(&[1u8, 2, 3, 4][..]));
        let r = pdev_record(0x8000, index::PD_INTERFACE_ADJUST, vec![5, 6]);
        assert_eq!(
            write_pdev_record(&mut p, &r, now, &mut actions),
            PnioStatus::OK
        );
        assert_eq!(p.iface_adjust(), Some(&[5u8, 6][..]));

        // Wrong subslot, or a Check record that does not parse: refused, nothing stored.
        let r = pdev_record(0x8000, index::PD_PORT_DATA_ADJUST, vec![9]);
        assert_eq!(
            write_pdev_record(&mut p, &r, now, &mut actions),
            PnioStatus::write_invalid_parameter()
        );
        assert_eq!(p.adjust(), Some(&[1u8, 2, 3, 4][..]));
        let r = pdev_record(0x8001, index::PD_PORT_DATA_CHECK, vec![0x02, 0x00, 0x00]);
        assert_eq!(
            write_pdev_record(&mut p, &r, now, &mut actions),
            PnioStatus::write_invalid_parameter()
        );
        assert_eq!(p.check().unwrap().raw, check);
    }

    /// A record over `MAX_PDEV_RECORD_LEN` is refused and not stored, so the u16
    /// `BlockLength` of `PDExpectedData` (`0xF842`), which nests every stored record,
    /// cannot wrap.
    #[test]
    fn write_pdev_record_refuses_an_oversized_record() {
        use crate::pdev::records::index;
        let now = Instant::now();
        let mut p = Pdev::new([0; 6], Box::new(MockLink(true.into())), now);
        let mut actions = Vec::new();
        let r = pdev_record(0x8001, index::PD_PORT_DATA_ADJUST, vec![0; 4097]);
        assert_eq!(
            write_pdev_record(&mut p, &r, now, &mut actions),
            PnioStatus::write_resource_unavailable()
        );
        assert_eq!(p.adjust(), None);
        let r = pdev_record(0x8000, index::PD_INTERFACE_ADJUST, vec![0; 4097]);
        assert_eq!(
            write_pdev_record(&mut p, &r, now, &mut actions),
            PnioStatus::write_resource_unavailable()
        );
        assert_eq!(p.iface_adjust(), None);
        // The limit itself is accepted.
        let r = pdev_record(0x8001, index::PD_PORT_DATA_ADJUST, vec![0; 4096]);
        assert_eq!(
            write_pdev_record(&mut p, &r, now, &mut actions),
            PnioStatus::OK
        );
        assert!(actions.is_empty());
    }

    /// A minimal well-formed I&M1 record (block header + 54 space-padded bytes).
    fn im1_record() -> Vec<u8> {
        let mut rec = Vec::new();
        crate::cm::block::BlockHeader::write(&mut rec, 0x0021, 54);
        rec.extend_from_slice(&[b' '; 54]);
        rec
    }
}

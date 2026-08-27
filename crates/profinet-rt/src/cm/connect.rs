//! Connect request/response: groups the parsed Connect-request blocks (Task 4),
//! validates them against a `DeviceModel`, extracts the AR parameters, and builds
//! the Connect response's PNIO blocks (byte-exact against the p-net golden capture).

use super::model::DeviceModel;
use super::{
    ty, AlarmCrBlockReq, ArBlockReq, BlockHeader, CmError, ConnectBlock, IocrBlockReq, IocrObject,
    PnioStatus,
};
use crate::eth::MacAddr;
use crate::rpc::{Drep, Uuid};

// ---------------------------------------------------------------------------------
// ConnectReq: the grouped Connect-request blocks.
// ---------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ConnectReq {
    pub ar: ArBlockReq,
    pub iocrs: Vec<IocrBlockReq>,
    pub expected: Vec<super::ExpectedSubmoduleBlockReq>,
    pub alarm_cr: AlarmCrBlockReq,
}

impl ConnectReq {
    /// Parse the concatenated PNIO blocks of a Connect request (no RPC header/NDR).
    /// Requires exactly one ARBlockReq and one AlarmCRBlockReq; any other known block
    /// type is grouped, and any unknown block type is rejected.
    pub fn parse(blocks: &[u8]) -> Result<ConnectReq, CmError> {
        let all = BlockHeader::read_all(blocks)?;
        let mut ar = None;
        let mut iocrs = Vec::new();
        let mut expected = Vec::new();
        let mut alarm_cr = None;
        for (header, body) in all {
            match header.block_type {
                ty::AR_BLOCK_REQ => {
                    if ar.is_some() {
                        return Err(CmError::Reject(PnioStatus::connect_reject(
                            ConnectBlock::ArBlock,
                            0,
                        )));
                    }
                    ar = Some(ArBlockReq::parse(body)?);
                }
                ty::IOCR_BLOCK_REQ => iocrs.push(IocrBlockReq::parse(body)?),
                ty::EXPECTED_SUBMODULE_BLOCK_REQ => {
                    expected.push(super::ExpectedSubmoduleBlockReq::parse(body)?)
                }
                ty::ALARM_CR_BLOCK_REQ => {
                    if alarm_cr.is_some() {
                        return Err(CmError::Reject(PnioStatus::connect_reject(
                            ConnectBlock::AlarmCr,
                            0,
                        )));
                    }
                    alarm_cr = Some(AlarmCrBlockReq::parse(body)?);
                }
                _ => {
                    return Err(CmError::Reject(PnioStatus::connect_reject(
                        ConnectBlock::ArBlock,
                        0xff,
                    )))
                }
            }
        }
        let ar = ar
            .ok_or_else(|| CmError::Reject(PnioStatus::connect_reject(ConnectBlock::ArBlock, 0)))?;
        let alarm_cr = alarm_cr
            .ok_or_else(|| CmError::Reject(PnioStatus::connect_reject(ConnectBlock::AlarmCr, 0)))?;
        Ok(ConnectReq {
            ar,
            iocrs,
            expected,
            alarm_cr,
        })
    }
}

// ---------------------------------------------------------------------------------
// ArParams / IocrParams: the AR parameters extracted from a validated Connect request.
// ---------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ArParams {
    pub ar_uuid: Uuid,
    pub session_key: u16,
    pub initiator_mac: MacAddr,
    pub initiator_object_uuid: Uuid,
    pub activity_timeout_factor: u16,
    pub input_cr: IocrParams,
    pub output_cr: IocrParams,
    pub alarm_ref_remote: u16,
    pub max_alarm_data_length: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IocrParams {
    pub reference: u16,
    pub frame_id: u16,
    pub data_length: u16,
    pub send_clock_factor: u16,
    pub reduction_ratio: u16,
    pub watchdog_factor: u16,
    pub data_hold_factor: u16,
    pub io_data: Vec<IocrObject>,
    pub iocs: Vec<IocrObject>,
}

impl IocrParams {
    fn from_req(cr: &IocrBlockReq) -> IocrParams {
        let (io_data, iocs) = match cr.apis.first() {
            Some(api) => (api.io_data.clone(), api.iocs.clone()),
            None => (Vec::new(), Vec::new()),
        };
        IocrParams {
            reference: cr.reference,
            frame_id: cr.frame_id,
            data_length: cr.data_length,
            send_clock_factor: cr.send_clock_factor,
            reduction_ratio: cr.reduction_ratio,
            watchdog_factor: cr.watchdog_factor,
            data_hold_factor: cr.data_hold_factor,
            io_data,
            iocs,
        }
    }
}

/// Valid PROFINET RT frame ID range for IO cyclic data (IEC 61158-6-10 §4.10.3.1.2.1).
const FRAME_ID_RANGE: std::ops::RangeInclusive<u16> = 0x8000..=0xBBFF;

/// Validate a Connect request against `model`, extracting the AR parameters on
/// success. Rejection reasons follow spec §6 (see `PnioStatus::connect_reject`).
pub fn validate(req: &ConnectReq, model: &DeviceModel) -> Result<ArParams, PnioStatus> {
    if req.ar.ar_type != 1 {
        return Err(PnioStatus::connect_reject(ConnectBlock::ArBlock, 1));
    }
    if req.ar.ar_uuid == Uuid::NIL {
        return Err(PnioStatus::connect_reject(ConnectBlock::ArBlock, 2));
    }

    if req.iocrs.len() != 2 {
        return Err(PnioStatus::connect_reject(ConnectBlock::IocrBlock, 1));
    }
    let input = req.iocrs.iter().find(|c| c.iocr_type == 1);
    let output = req.iocrs.iter().find(|c| c.iocr_type == 2);
    let (input, output) = match (input, output) {
        (Some(i), Some(o)) => (i, o),
        _ => return Err(PnioStatus::connect_reject(ConnectBlock::IocrBlock, 1)),
    };
    for cr in [input, output] {
        if !FRAME_ID_RANGE.contains(&cr.frame_id) {
            return Err(PnioStatus::connect_reject(ConnectBlock::IocrBlock, 6));
        }
    }

    for block in &req.expected {
        for api in &block.apis {
            for sm in &api.submodules {
                let input_len = sm.input.map(|d| d.data_length).unwrap_or(0);
                let output_len = sm.output.map(|d| d.data_length).unwrap_or(0);
                match model.find(api.slot, sm.subslot) {
                    Some(m) if m.input_len == input_len && m.output_len == output_len => {}
                    _ => {
                        return Err(PnioStatus::connect_reject(
                            ConnectBlock::ExpectedSubmodule,
                            7,
                        ))
                    }
                }
            }
        }
    }

    if req.alarm_cr.alarm_cr_type != 1 {
        return Err(PnioStatus::connect_reject(ConnectBlock::AlarmCr, 1));
    }

    Ok(ArParams {
        ar_uuid: req.ar.ar_uuid,
        session_key: req.ar.session_key,
        initiator_mac: req.ar.initiator_mac,
        initiator_object_uuid: req.ar.initiator_object_uuid,
        activity_timeout_factor: req.ar.activity_timeout_factor,
        input_cr: IocrParams::from_req(input),
        output_cr: IocrParams::from_req(output),
        alarm_ref_remote: req.alarm_cr.local_alarm_reference,
        max_alarm_data_length: model.max_alarm_data_length,
    })
}

// ---------------------------------------------------------------------------------
// build_connect_res: the Connect response's PNIO blocks (no RPC header/NDR).
// ---------------------------------------------------------------------------------

/// Build the Connect response's PNIO blocks: ARBlockRes, IOCRBlockRes x2 (input then
/// output, matching request order), AlarmCRBlockRes, ARServerBlockRes.
pub fn build_connect_res(params: &ArParams, model: &DeviceModel) -> Vec<u8> {
    let mut out = Vec::new();
    write_ar_block_res(&mut out, params, model);
    write_iocr_block_res(&mut out, 1, &params.input_cr);
    write_iocr_block_res(&mut out, 2, &params.output_cr);
    write_alarm_cr_block_res(&mut out, model.max_alarm_data_length);
    write_ar_server_block_res(&mut out, &model.station_name);
    out
}

fn write_ar_block_res(out: &mut Vec<u8>, params: &ArParams, model: &DeviceModel) {
    let mut body = Vec::with_capacity(28);
    body.extend_from_slice(&1u16.to_be_bytes()); // ar_type
    params.ar_uuid.write(&mut body, Drep::BIG);
    body.extend_from_slice(&params.session_key.to_be_bytes());
    body.extend_from_slice(&model.mac.0);
    body.extend_from_slice(&0x8892u16.to_be_bytes());
    BlockHeader::write(out, ty::AR_BLOCK_RES, body.len() as u16);
    out.extend_from_slice(&body);
}

fn write_iocr_block_res(out: &mut Vec<u8>, iocr_type: u16, cr: &IocrParams) {
    let mut body = Vec::with_capacity(6);
    body.extend_from_slice(&iocr_type.to_be_bytes());
    body.extend_from_slice(&cr.reference.to_be_bytes());
    body.extend_from_slice(&cr.frame_id.to_be_bytes());
    BlockHeader::write(out, ty::IOCR_BLOCK_RES, body.len() as u16);
    out.extend_from_slice(&body);
}

fn write_alarm_cr_block_res(out: &mut Vec<u8>, max_alarm_data_length: u16) {
    let mut body = Vec::with_capacity(6);
    body.extend_from_slice(&1u16.to_be_bytes()); // alarm_cr_type
    body.extend_from_slice(&0u16.to_be_bytes()); // local_alarm_reference
    body.extend_from_slice(&max_alarm_data_length.to_be_bytes());
    BlockHeader::write(out, ty::ALARM_CR_BLOCK_RES, body.len() as u16);
    out.extend_from_slice(&body);
}

fn write_ar_server_block_res(out: &mut Vec<u8>, station_name: &str) {
    let name = station_name.as_bytes();
    let mut body = Vec::with_capacity(2 + name.len());
    body.extend_from_slice(&(name.len() as u16).to_be_bytes());
    body.extend_from_slice(name);
    // Pad so `BlockHeader::LEN + body.len()` (the whole block) is a multiple of 4.
    let total = BlockHeader::LEN + body.len();
    let pad = (4 - total % 4) % 4;
    body.extend(std::iter::repeat(0u8).take(pad));
    BlockHeader::write(out, ty::AR_SERVER_BLOCK_RES, body.len() as u16);
    out.extend_from_slice(&body);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cm::model::DeviceModel;
    use crate::eth::MacAddr;
    use crate::testutil::golden;

    const REQ_BLOCKS: usize = 142;
    const RES_BLOCKS: usize = 142;
    const DEVICE_MAC: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);

    fn req() -> ConnectReq {
        ConnectReq::parse(&golden("connect_req")[REQ_BLOCKS..]).unwrap()
    }

    #[test]
    fn parse_groups_blocks() {
        let r = req();
        assert_eq!(r.iocrs.len(), 2);
        assert_eq!(r.expected.len(), 5);
        assert_eq!(r.alarm_cr.max_alarm_data_length, 256);
    }

    #[test]
    fn validate_against_pnet_model() {
        let p = validate(&req(), &DeviceModel::pnet_sample(DEVICE_MAC)).unwrap();
        assert_eq!(p.session_key, 2);
        assert_eq!(
            (p.input_cr.frame_id, p.output_cr.frame_id),
            (0x8000, 0x8001)
        );
        assert_eq!(p.input_cr.reduction_ratio, 32);
        assert_eq!(p.activity_timeout_factor, 200);
        assert_eq!(p.max_alarm_data_length, 200);
    }

    #[test]
    fn connect_response_is_byte_exact() {
        let model = DeviceModel::pnet_sample(DEVICE_MAC);
        let p = validate(&req(), &model).unwrap();
        let out = build_connect_res(&p, &model);
        assert_eq!(out, &golden("connect_res")[RES_BLOCKS..]);
        assert_eq!(out.len(), 90);
    }

    #[test]
    fn mismatching_module_is_rejected_with_explicit_status() {
        let mut model = DeviceModel::pnet_sample(DEVICE_MAC);
        model.slots[4].submodules[0].input_len = 4; // Echo expects 8
        let err = validate(&req(), &model).unwrap_err();
        assert_eq!(
            err,
            PnioStatus::connect_reject(ConnectBlock::ExpectedSubmodule, 7)
        );
        let mut model = DeviceModel::pnet_sample(DEVICE_MAC);
        model.slots.pop(); // slot 4 missing
        assert_eq!(
            validate(&req(), &model).unwrap_err(),
            PnioStatus::connect_reject(ConnectBlock::ExpectedSubmodule, 7)
        );
    }

    #[test]
    fn bad_frame_id_and_ar_type_are_rejected() {
        let mut r = req();
        r.iocrs[0].frame_id = 0xc000;
        assert_eq!(
            validate(&r, &DeviceModel::pnet_sample(DEVICE_MAC)).unwrap_err(),
            PnioStatus::connect_reject(ConnectBlock::IocrBlock, 6)
        );
        let mut r = req();
        r.ar.ar_type = 6;
        assert_eq!(
            validate(&r, &DeviceModel::pnet_sample(DEVICE_MAC)).unwrap_err(),
            PnioStatus::connect_reject(ConnectBlock::ArBlock, 1)
        );
    }

    #[test]
    fn missing_alarm_cr_block_is_a_reject() {
        let b = golden("connect_req");
        let without_alarm = &b[REQ_BLOCKS..b.len() - 26]; // AlarmCRBlockReq = 6 + 20 bytes
        assert!(matches!(
            ConnectReq::parse(without_alarm),
            Err(CmError::Reject(_))
        ));
    }
}

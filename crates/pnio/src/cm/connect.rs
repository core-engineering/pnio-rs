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

/// The Connect request's blocks, grouped by type; see [`ConnectReq::parse`].
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectReq {
    /// The (single, required) ARBlockReq.
    pub ar: ArBlockReq,
    /// The IOCRBlockReq blocks, in request order (normally one Input, one Output).
    pub iocrs: Vec<IocrBlockReq>,
    /// The ExpectedSubmoduleBlockReq blocks, in request order.
    pub expected: Vec<super::ExpectedSubmoduleBlockReq>,
    /// The (single, required) AlarmCRBlockReq.
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

/// The AR parameters extracted from a validated Connect request, ready to build the
/// device's layout, alarm channel and Connect response from.
#[derive(Debug, Clone, PartialEq)]
pub struct ArParams {
    /// The AR's identifying UUID (`ARBlockReq.ARUUID`).
    pub ar_uuid: Uuid,
    /// Session key, echoed on every subsequent control exchange for this AR.
    pub session_key: u16,
    /// The controller's MAC address (`ARBlockReq.CMInitiatorMacAdd`).
    pub initiator_mac: MacAddr,
    /// The controller's PNIO object UUID (`ARBlockReq.CMInitiatorObjectUUID`).
    pub initiator_object_uuid: Uuid,
    /// `ARBlockReq.ARProperties`-adjacent `CMInitiatorActivityTimeoutFactor`, in units
    /// of [`super::ar::ACTIVITY_TIMEOUT_UNIT`].
    pub activity_timeout_factor: u16,
    /// The negotiated Input CR (device-to-controller) parameters.
    pub input_cr: IocrParams,
    /// The negotiated Output CR (controller-to-device) parameters.
    pub output_cr: IocrParams,
    /// The controller's `LocalAlarmReference` (`AlarmCRBlockReq.LocalAlarmReference`),
    /// used as `AlarmDstEndpoint` on outgoing alarm frames.
    pub alarm_ref_remote: u16,
    /// The value this device answers with in `AlarmCRBlockRes.MaxAlarmDataLength`
    /// (from [`DeviceModel::max_alarm_data_length`]).
    pub max_alarm_data_length: u16,
    /// `AlarmCRBlockReq.MaxAlarmDataLength` as the CPU asked for it (256 on the
    /// bench) — `max_alarm_data_length` above is our own value (from the model),
    /// which is what we actually answer in `AlarmCRBlockRes`.
    pub max_alarm_data_length_remote: u16,
    /// `AlarmCRBlockReq.RTATimeoutFactor`: how long to wait for a transport ACK or
    /// content AlarmAck before resending, in units defined by the standard.
    pub rta_timeout_factor: u16,
    /// `AlarmCRBlockReq.RTARetries`: resends of an unacknowledged alarm before the
    /// channel gives up and aborts the AR.
    pub rta_retries: u16,
    /// Our own local alarm reference, always 0 — what `AlarmCRBlockRes` answers.
    pub alarm_ref_local: u16,
    /// `AlarmCRBlockReq.AlarmCRTagHeaderHigh`: the VLAN TCI negotiated for the High
    /// priority alarm channel.
    pub alarm_tag_high: u16,
    /// `AlarmCRBlockReq.AlarmCRTagHeaderLow`: the VLAN TCI negotiated for the Low
    /// priority alarm channel.
    pub alarm_tag_low: u16,
}

/// One Communication Relationship's negotiated parameters, extracted from its
/// `IOCRBlockReq` (and, for the Output CR, resolved of a device-selected FrameID).
#[derive(Debug, Clone, PartialEq)]
pub struct IocrParams {
    /// `IOCRReference`, identifying this CR within the AR.
    pub reference: u16,
    /// The CR's `FrameID`; this crate requires `0x8000..=0xBBFF` on input (or
    /// `0xFFFF`/[`FRAME_ID_DEVICE_SELECTS`] on the Output CR, resolved here to a real
    /// value).
    pub frame_id: u16,
    /// Total C-SDU length in bytes, as negotiated.
    pub data_length: u16,
    /// `SendClockFactor`: base send-clock multiplier (send clock = `send_clock_factor * 31.25 us`).
    pub send_clock_factor: u16,
    /// `ReductionRatio`: how many send-clock periods make up one cycle for this CR.
    pub reduction_ratio: u16,
    /// `WatchdogFactor`: consumer watchdog window, in cycles.
    pub watchdog_factor: u16,
    /// `DataHoldFactor`: cycles the consumer holds the last valid data before declaring the watchdog expired.
    pub data_hold_factor: u16,
    /// The CR's IO data objects (one per submodule carrying data in this direction).
    pub io_data: Vec<IocrObject>,
    /// The CR's IOCS-only objects.
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

/// FrameID value `0xFFFF` in an Output CR's IOCRBlockReq: the IO controller leaves the
/// choice to the IO device, which picks one and returns it in the IOCRBlockRes.
pub const FRAME_ID_DEVICE_SELECTS: u16 = 0xFFFF;

/// Pick a FrameID for a device-selected Output CR: the first value in `0x8001..=0xBBFF`
/// that differs from the Input CR's FrameID.
fn select_output_frame_id(input_frame_id: u16) -> u16 {
    (0x8001..=0xBBFFu16)
        .find(|&id| id != input_frame_id)
        .unwrap_or(0x8001)
}

/// Validate one IOCR's structure and DataLength against the model: exactly one API
/// (else reject `(IocrBlock, 15)`, NumberOfAPIs); every IODataObject must fit
/// `frame_offset + model_len + 1` (payload plus its trailing IOPS byte), and every
/// IOCS object must fit `frame_offset + 1` (else reject `(IocrBlock, 5)`, DataLength).
/// `model_len` is the model submodule's `input_len` for the Input CR or `output_len`
/// for the Output CR (`is_output_cr`). An IODataObject referring to a (slot, subslot)
/// absent from the model is also rejected here (normally caught by the
/// ExpectedSubmodule check, but guarded independently since this check looks the
/// submodule up itself).
fn check_iocr_data_length(
    cr: &IocrBlockReq,
    model: &DeviceModel,
    is_output_cr: bool,
) -> Result<(), PnioStatus> {
    if cr.apis.len() != 1 {
        return Err(PnioStatus::connect_reject(ConnectBlock::IocrBlock, 15));
    }
    let too_small = PnioStatus::connect_reject(ConnectBlock::IocrBlock, 5);
    let api = &cr.apis[0];
    for obj in &api.io_data {
        let sm = model.find(obj.slot, obj.subslot).ok_or(too_small)?;
        let model_len = if is_output_cr {
            sm.output_len
        } else {
            sm.input_len
        };
        let end = obj.frame_offset as u32 + model_len as u32 + 1;
        if end > cr.data_length as u32 {
            return Err(too_small);
        }
    }
    for obj in &api.iocs {
        let end = obj.frame_offset as u32 + 1;
        if end > cr.data_length as u32 {
            return Err(too_small);
        }
    }
    Ok(())
}

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
    if !FRAME_ID_RANGE.contains(&input.frame_id) {
        return Err(PnioStatus::connect_reject(ConnectBlock::IocrBlock, 6));
    }
    if !FRAME_ID_RANGE.contains(&output.frame_id) && output.frame_id != FRAME_ID_DEVICE_SELECTS {
        return Err(PnioStatus::connect_reject(ConnectBlock::IocrBlock, 6));
    }
    // Run before `check_iocr_data_length`: an identity or size mismatch against the
    // model is a more specific diagnosis (ExpectedSubmodule) than the data-length
    // guard's generic reject for the same missing lookup. Identity (module/submodule
    // ident) is checked before size, per field: `4` (module), `6` (submodule), `7`
    // (missing submodule, or found but wrong length).
    for block in &req.expected {
        for api in &block.apis {
            if let Some(slot_model) = model.slots.iter().find(|s| s.slot == api.slot) {
                if slot_model.module_ident != api.module_ident {
                    return Err(PnioStatus::connect_reject(
                        ConnectBlock::ExpectedSubmodule,
                        4,
                    ));
                }
            }
            for sm in &api.submodules {
                let m = match model.find(api.slot, sm.subslot) {
                    Some(m) => m,
                    None => {
                        return Err(PnioStatus::connect_reject(
                            ConnectBlock::ExpectedSubmodule,
                            7,
                        ))
                    }
                };
                if m.submodule_ident != sm.submodule_ident {
                    return Err(PnioStatus::connect_reject(
                        ConnectBlock::ExpectedSubmodule,
                        6,
                    ));
                }
                let input_len = sm.input.map(|d| d.data_length).unwrap_or(0);
                let output_len = sm.output.map(|d| d.data_length).unwrap_or(0);
                if m.input_len != input_len || m.output_len != output_len {
                    return Err(PnioStatus::connect_reject(
                        ConnectBlock::ExpectedSubmodule,
                        7,
                    ));
                }
            }
        }
    }

    check_iocr_data_length(input, model, false)?;
    check_iocr_data_length(output, model, true)?;

    if req.alarm_cr.alarm_cr_type != 1 {
        return Err(PnioStatus::connect_reject(ConnectBlock::AlarmCr, 1));
    }

    let mut output_cr = IocrParams::from_req(output);
    if output.frame_id == FRAME_ID_DEVICE_SELECTS {
        output_cr.frame_id = select_output_frame_id(input.frame_id);
        log::info!(
            "Output CR requested a device-selected FrameID; selected {:#06x}",
            output_cr.frame_id
        );
    }

    Ok(ArParams {
        ar_uuid: req.ar.ar_uuid,
        session_key: req.ar.session_key,
        initiator_mac: req.ar.initiator_mac,
        initiator_object_uuid: req.ar.initiator_object_uuid,
        activity_timeout_factor: req.ar.activity_timeout_factor,
        input_cr: IocrParams::from_req(input),
        output_cr,
        alarm_ref_remote: req.alarm_cr.local_alarm_reference,
        max_alarm_data_length: model.max_alarm_data_length,
        max_alarm_data_length_remote: req.alarm_cr.max_alarm_data_length,
        rta_timeout_factor: req.alarm_cr.rta_timeout_factor,
        rta_retries: req.alarm_cr.rta_retries,
        alarm_ref_local: 0,
        alarm_tag_high: req.alarm_cr.tag_header_high,
        alarm_tag_low: req.alarm_cr.tag_header_low,
    })
}

// ---------------------------------------------------------------------------------
// build_connect_res: the Connect response's PNIO blocks (no RPC header/NDR).
// ---------------------------------------------------------------------------------

/// Build the Connect response's PNIO blocks: ARBlockRes, IOCRBlockRes x2 (input then
/// output — that order is assumed, not read back from the request: every PROFINET
/// engineering tool sends it that way, but nothing here re-derives it from
/// `ArParams`), AlarmCRBlockRes, ARServerBlockRes.
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
    fn mismatching_module_ident_is_rejected() {
        let mut model = DeviceModel::pnet_sample(DEVICE_MAC);
        model.slots[1].module_ident = 0x99; // was 0x30
        assert_eq!(
            validate(&req(), &model).unwrap_err(),
            PnioStatus::connect_reject(ConnectBlock::ExpectedSubmodule, 4)
        );
    }

    #[test]
    fn mismatching_submodule_ident_is_rejected() {
        let mut model = DeviceModel::pnet_sample(DEVICE_MAC);
        model.slots[4].submodules[0].submodule_ident = 0x999; // was 0x140
        assert_eq!(
            validate(&req(), &model).unwrap_err(),
            PnioStatus::connect_reject(ConnectBlock::ExpectedSubmodule, 6)
        );
    }

    #[test]
    fn output_cr_frame_id_0xffff_is_replaced_by_a_device_selected_id() {
        let mut r = req();
        let output = r
            .iocrs
            .iter_mut()
            .find(|c| c.iocr_type == 2)
            .expect("golden request has an Output CR");
        output.frame_id = FRAME_ID_DEVICE_SELECTS;
        let model = DeviceModel::pnet_sample(DEVICE_MAC);
        let params = validate(&r, &model).unwrap();
        assert_eq!(params.output_cr.frame_id, 0x8001);
        assert_eq!(params.input_cr.frame_id, 0x8000);
        let out = build_connect_res(&params, &model);
        assert_eq!(out, &golden("connect_res")[RES_BLOCKS..]);
    }

    #[test]
    fn input_cr_frame_id_0xffff_is_still_rejected() {
        let mut r = req();
        let input = r
            .iocrs
            .iter_mut()
            .find(|c| c.iocr_type == 1)
            .expect("golden request has an Input CR");
        input.frame_id = FRAME_ID_DEVICE_SELECTS;
        assert_eq!(
            validate(&r, &DeviceModel::pnet_sample(DEVICE_MAC)).unwrap_err(),
            PnioStatus::connect_reject(ConnectBlock::IocrBlock, 6)
        );
    }

    #[test]
    fn device_selected_id_avoids_the_input_cr_id() {
        let mut r = req();
        for cr in r.iocrs.iter_mut() {
            match cr.iocr_type {
                1 => cr.frame_id = 0x8001,
                2 => cr.frame_id = FRAME_ID_DEVICE_SELECTS,
                _ => unreachable!(),
            }
        }
        let params = validate(&r, &DeviceModel::pnet_sample(DEVICE_MAC)).unwrap();
        assert_eq!(params.input_cr.frame_id, 0x8001);
        assert_eq!(params.output_cr.frame_id, 0x8002);
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
    fn data_length_too_small_is_rejected() {
        let mut r = req();
        r.iocrs[0].data_length = 10;
        assert_eq!(
            validate(&r, &DeviceModel::pnet_sample(DEVICE_MAC)).unwrap_err(),
            PnioStatus::connect_reject(ConnectBlock::IocrBlock, 5)
        );
    }

    #[test]
    fn iocr_without_api_is_rejected() {
        let mut r = req();
        r.iocrs[0].apis.clear();
        assert_eq!(
            validate(&r, &DeviceModel::pnet_sample(DEVICE_MAC)).unwrap_err(),
            PnioStatus::connect_reject(ConnectBlock::IocrBlock, 15)
        );
    }

    #[test]
    fn iocr_with_two_apis_is_rejected() {
        let mut r = req();
        let api = r.iocrs[0].apis[0].clone();
        r.iocrs[0].apis.push(api);
        assert_eq!(
            validate(&r, &DeviceModel::pnet_sample(DEVICE_MAC)).unwrap_err(),
            PnioStatus::connect_reject(ConnectBlock::IocrBlock, 15)
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

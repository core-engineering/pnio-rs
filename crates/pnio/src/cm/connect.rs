//! Connect request/response: groups the parsed Connect-request blocks (Task 4),
//! validates them against a `DeviceModel`, extracts the AR parameters, and builds
//! the Connect response's PNIO blocks (byte-exact against the p-net golden capture).

use super::model::DeviceModel;
use super::status::{
    alarm_cr_field, ar_field, block_field, expected_submodule_field, iocr_field, CmrpcReason,
};
use super::{
    ty, AlarmCrBlockReq, ArBlockReq, BlockError, BlockHeader, CmError, ConnectBlock, IocrBlockReq,
    IocrObject, PnioStatus,
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

/// The [`ConnectBlock`] a Connect-request `BlockType` is reported under, if it is one
/// this crate knows.
fn connect_block(block_type: u16) -> Option<ConnectBlock> {
    match block_type {
        ty::AR_BLOCK_REQ => Some(ConnectBlock::ArBlock),
        ty::IOCR_BLOCK_REQ => Some(ConnectBlock::IocrBlock),
        ty::EXPECTED_SUBMODULE_BLOCK_REQ => Some(ConnectBlock::ExpectedSubmodule),
        ty::ALARM_CR_BLOCK_REQ => Some(ConnectBlock::AlarmCr),
        _ => None,
    }
}

fn reject(status: PnioStatus) -> CmError {
    CmError::Reject(status)
}

fn reject_cmrpc(reason: CmrpcReason) -> CmError {
    reject(PnioStatus::connect_reject_cmrpc(reason))
}

/// The status for a block header that failed to parse at `rest` (the unparsed tail of
/// the Connect request's blocks): a bad version is reported against the block it
/// heads (`BlockVersionHigh`/`BlockVersionLow`) when that block is one this crate
/// knows, and as `UnknownBlocks` otherwise; a header or body that does not fit the
/// remaining args is `ArgsLengthInvalid`.
fn header_reject(rest: &[u8], e: BlockError) -> CmError {
    match e {
        BlockError::BadVersion(high, _) => {
            let block_type = u16::from_be_bytes([rest[0], rest[1]]);
            match connect_block(block_type) {
                Some(block) => {
                    let field = if high != 1 {
                        block_field::BLOCK_VERSION_HIGH
                    } else {
                        block_field::BLOCK_VERSION_LOW
                    };
                    reject(PnioStatus::connect_reject(block, field))
                }
                None => reject_cmrpc(CmrpcReason::UnknownBlocks),
            }
        }
        _ => reject_cmrpc(CmrpcReason::ArgsLengthInvalid),
    }
}

/// The status for a block body that failed to parse: reported against `block`'s
/// `BlockLength`, as the body does not hold what the block type promises (too short
/// for its fields, or a count or tag that disagrees with the bytes that follow).
fn body_reject(block: ConnectBlock, e: BlockError) -> CmError {
    log::warn!("Connect {block:?} body does not parse: {e}");
    reject(PnioStatus::connect_reject(block, block_field::BLOCK_LENGTH))
}

impl ConnectReq {
    /// Parse the concatenated PNIO blocks of a Connect request (no RPC header/NDR).
    /// Requires exactly one ARBlockReq and one AlarmCRBlockReq; any other known block
    /// type is grouped, and any unknown block type is rejected. Every failure is a
    /// [`CmError::Reject`] carrying the status to answer with: a missing or duplicated
    /// ARBlockReq/AlarmCRBlockReq is `WrongBlockCount`, an unknown block type
    /// `UnknownBlocks`, a header that does not fit the args `ArgsLengthInvalid`, a bad
    /// block version `(block, BlockVersion…)` and a body that does not parse
    /// `(block, BlockLength)`.
    pub fn parse(blocks: &[u8]) -> Result<ConnectReq, CmError> {
        let mut ar = None;
        let mut iocrs = Vec::new();
        let mut expected = Vec::new();
        let mut alarm_cr = None;
        let mut pos = 0usize;
        while pos < blocks.len() {
            let rest = &blocks[pos..];
            let (header, body) = BlockHeader::parse(rest).map_err(|e| header_reject(rest, e))?;
            pos += BlockHeader::LEN + body.len();
            let block = match connect_block(header.block_type) {
                Some(block) => block,
                None => return Err(reject_cmrpc(CmrpcReason::UnknownBlocks)),
            };
            match block {
                ConnectBlock::ArBlock => {
                    if ar.is_some() {
                        return Err(reject_cmrpc(CmrpcReason::WrongBlockCount));
                    }
                    ar = Some(ArBlockReq::parse(body).map_err(|e| body_reject(block, e))?);
                }
                ConnectBlock::IocrBlock => {
                    iocrs.push(IocrBlockReq::parse(body).map_err(|e| body_reject(block, e))?)
                }
                ConnectBlock::ExpectedSubmodule => expected.push(
                    super::ExpectedSubmoduleBlockReq::parse(body)
                        .map_err(|e| body_reject(block, e))?,
                ),
                ConnectBlock::AlarmCr => {
                    if alarm_cr.is_some() {
                        return Err(reject_cmrpc(CmrpcReason::WrongBlockCount));
                    }
                    alarm_cr =
                        Some(AlarmCrBlockReq::parse(body).map_err(|e| body_reject(block, e))?);
                }
            }
        }
        let ar = ar.ok_or_else(|| reject_cmrpc(CmrpcReason::WrongBlockCount))?;
        let alarm_cr = alarm_cr.ok_or_else(|| reject_cmrpc(CmrpcReason::WrongBlockCount))?;
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
    /// `WatchdogFactor`: the controller's consumer watchdog, in cycles. Parsed and stored only:
    /// this device's own consumer watchdog is derived from [`IocrParams::data_hold_factor`]
    /// (see `rt::layout`).
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
/// (else reject `(IocrBlock, NumberOfAPIs)`); every IODataObject must fit
/// `frame_offset + model_len + 1` (payload plus its trailing IOPS byte), and every
/// IOCS object must fit `frame_offset + 1` (else reject `(IocrBlock, DataLength)`).
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
        return Err(PnioStatus::connect_reject(
            ConnectBlock::IocrBlock,
            iocr_field::NUMBER_OF_APIS,
        ));
    }
    let too_small = PnioStatus::connect_reject(ConnectBlock::IocrBlock, iocr_field::DATA_LENGTH);
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
/// success. A rejection names the offending block and field (see
/// [`PnioStatus::connect_reject`]) or, for a missing or surplus IOCR, the CMRPC
/// reason (see [`PnioStatus::connect_reject_cmrpc`]).
pub fn validate(req: &ConnectReq, model: &DeviceModel) -> Result<ArParams, PnioStatus> {
    if req.ar.ar_type != 1 {
        return Err(PnioStatus::connect_reject(
            ConnectBlock::ArBlock,
            ar_field::AR_TYPE,
        ));
    }
    if req.ar.ar_uuid == Uuid::NIL {
        return Err(PnioStatus::connect_reject(
            ConnectBlock::ArBlock,
            ar_field::AR_UUID,
        ));
    }

    // Exactly one Input and one Output CR: none of a kind is `IOCRMissing`, more than
    // one of a kind exceeds what this device (like a legacy AR) provides.
    let input = req.iocrs.iter().find(|c| c.iocr_type == 1);
    let output = req.iocrs.iter().find(|c| c.iocr_type == 2);
    let (input, output) = match (input, output) {
        (Some(i), Some(o)) => (i, o),
        _ => return Err(PnioStatus::connect_reject_cmrpc(CmrpcReason::IocrMissing)),
    };
    if req.iocrs.len() != 2 {
        return Err(PnioStatus::connect_reject_cmrpc(
            CmrpcReason::OutOfProviderConsumerOrAlarmResources,
        ));
    }
    if !FRAME_ID_RANGE.contains(&input.frame_id) {
        return Err(PnioStatus::connect_reject(
            ConnectBlock::IocrBlock,
            iocr_field::FRAME_ID,
        ));
    }
    if !FRAME_ID_RANGE.contains(&output.frame_id) && output.frame_id != FRAME_ID_DEVICE_SELECTS {
        return Err(PnioStatus::connect_reject(
            ConnectBlock::IocrBlock,
            iocr_field::FRAME_ID,
        ));
    }
    // Run before `check_iocr_data_length`: an identity or size mismatch against the
    // model is a more specific diagnosis (ExpectedSubmodule) than the data-length
    // guard's generic reject for the same missing lookup. Identity (module/submodule
    // ident) is checked before size, per field: `ModuleIdentNumber`, then
    // `SubslotNumber` (no such submodule), `SubmoduleIdentNumber`, and
    // `SubmoduleDataLength` (found, but the wrong length).
    for block in &req.expected {
        for api in &block.apis {
            if let Some(slot_model) = model.slots.iter().find(|s| s.slot == api.slot) {
                if slot_model.module_ident != api.module_ident {
                    return Err(PnioStatus::connect_reject(
                        ConnectBlock::ExpectedSubmodule,
                        expected_submodule_field::MODULE_IDENT,
                    ));
                }
            }
            for sm in &api.submodules {
                let m = match model.find(api.slot, sm.subslot) {
                    Some(m) => m,
                    None => {
                        return Err(PnioStatus::connect_reject(
                            ConnectBlock::ExpectedSubmodule,
                            expected_submodule_field::SUBSLOT,
                        ))
                    }
                };
                if m.submodule_ident != sm.submodule_ident {
                    return Err(PnioStatus::connect_reject(
                        ConnectBlock::ExpectedSubmodule,
                        expected_submodule_field::SUBMODULE_IDENT,
                    ));
                }
                let input_len = sm.input.map(|d| d.data_length).unwrap_or(0);
                let output_len = sm.output.map(|d| d.data_length).unwrap_or(0);
                if m.input_len != input_len || m.output_len != output_len {
                    return Err(PnioStatus::connect_reject(
                        ConnectBlock::ExpectedSubmodule,
                        expected_submodule_field::SUBMODULE_DATA_LENGTH,
                    ));
                }
            }
        }
    }

    check_iocr_data_length(input, model, false)?;
    check_iocr_data_length(output, model, true)?;

    if req.alarm_cr.alarm_cr_type != 1 {
        return Err(PnioStatus::connect_reject(
            ConnectBlock::AlarmCr,
            alarm_cr_field::ALARM_CR_TYPE,
        ));
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
            PnioStatus::connect_reject(
                ConnectBlock::ExpectedSubmodule,
                expected_submodule_field::SUBMODULE_DATA_LENGTH
            )
        );
        assert_eq!(err.to_u32(), 0xdb81_030e);
        let mut model = DeviceModel::pnet_sample(DEVICE_MAC);
        model.slots.pop(); // slot 4 missing
        let err = validate(&req(), &model).unwrap_err();
        assert_eq!(
            err,
            PnioStatus::connect_reject(
                ConnectBlock::ExpectedSubmodule,
                expected_submodule_field::SUBSLOT
            )
        );
        assert_eq!(err.to_u32(), 0xdb81_030a);
    }

    #[test]
    fn mismatching_module_ident_is_rejected() {
        let mut model = DeviceModel::pnet_sample(DEVICE_MAC);
        model.slots[1].module_ident = 0x99; // was 0x30
        let err = validate(&req(), &model).unwrap_err();
        assert_eq!(
            err,
            PnioStatus::connect_reject(
                ConnectBlock::ExpectedSubmodule,
                expected_submodule_field::MODULE_IDENT
            )
        );
        assert_eq!(err.to_u32(), 0xdb81_0307);
    }

    #[test]
    fn mismatching_submodule_ident_is_rejected() {
        let mut model = DeviceModel::pnet_sample(DEVICE_MAC);
        model.slots[4].submodules[0].submodule_ident = 0x999; // was 0x140
        let err = validate(&req(), &model).unwrap_err();
        assert_eq!(
            err,
            PnioStatus::connect_reject(
                ConnectBlock::ExpectedSubmodule,
                expected_submodule_field::SUBMODULE_IDENT
            )
        );
        assert_eq!(err.to_u32(), 0xdb81_030b);
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
            PnioStatus::connect_reject(ConnectBlock::IocrBlock, iocr_field::FRAME_ID)
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
        let err = validate(&r, &DeviceModel::pnet_sample(DEVICE_MAC)).unwrap_err();
        assert_eq!(
            err,
            PnioStatus::connect_reject(ConnectBlock::IocrBlock, iocr_field::FRAME_ID)
        );
        assert_eq!(err.to_u32(), 0xdb81_0209);
        let mut r = req();
        r.ar.ar_type = 6;
        let err = validate(&r, &DeviceModel::pnet_sample(DEVICE_MAC)).unwrap_err();
        assert_eq!(
            err,
            PnioStatus::connect_reject(ConnectBlock::ArBlock, ar_field::AR_TYPE)
        );
        assert_eq!(err.to_u32(), 0xdb81_0104);
    }

    #[test]
    fn nil_ar_uuid_is_rejected() {
        let mut r = req();
        r.ar.ar_uuid = Uuid::NIL;
        let err = validate(&r, &DeviceModel::pnet_sample(DEVICE_MAC)).unwrap_err();
        assert_eq!(
            err,
            PnioStatus::connect_reject(ConnectBlock::ArBlock, ar_field::AR_UUID)
        );
        assert_eq!(err.to_u32(), 0xdb81_0105);
    }

    #[test]
    fn wrong_alarm_cr_type_is_rejected() {
        let mut r = req();
        r.alarm_cr.alarm_cr_type = 2;
        let err = validate(&r, &DeviceModel::pnet_sample(DEVICE_MAC)).unwrap_err();
        assert_eq!(
            err,
            PnioStatus::connect_reject(ConnectBlock::AlarmCr, alarm_cr_field::ALARM_CR_TYPE)
        );
        assert_eq!(err.to_u32(), 0xdb81_0404);
    }

    #[test]
    fn data_length_too_small_is_rejected() {
        let mut r = req();
        r.iocrs[0].data_length = 10;
        let err = validate(&r, &DeviceModel::pnet_sample(DEVICE_MAC)).unwrap_err();
        assert_eq!(
            err,
            PnioStatus::connect_reject(ConnectBlock::IocrBlock, iocr_field::DATA_LENGTH)
        );
        assert_eq!(err.to_u32(), 0xdb81_0208);
    }

    #[test]
    fn iocr_without_api_is_rejected() {
        let mut r = req();
        r.iocrs[0].apis.clear();
        let err = validate(&r, &DeviceModel::pnet_sample(DEVICE_MAC)).unwrap_err();
        assert_eq!(
            err,
            PnioStatus::connect_reject(ConnectBlock::IocrBlock, iocr_field::NUMBER_OF_APIS)
        );
        assert_eq!(err.to_u32(), 0xdb81_0213);
    }

    #[test]
    fn iocr_with_two_apis_is_rejected() {
        let mut r = req();
        let api = r.iocrs[0].apis[0].clone();
        r.iocrs[0].apis.push(api);
        assert_eq!(
            validate(&r, &DeviceModel::pnet_sample(DEVICE_MAC)).unwrap_err(),
            PnioStatus::connect_reject(ConnectBlock::IocrBlock, iocr_field::NUMBER_OF_APIS)
        );
    }

    #[test]
    fn missing_input_or_output_cr_is_iocr_missing() {
        let model = DeviceModel::pnet_sample(DEVICE_MAC);
        let mut r = req();
        r.iocrs.retain(|c| c.iocr_type == 1);
        let err = validate(&r, &model).unwrap_err();
        assert_eq!(
            err,
            PnioStatus::connect_reject_cmrpc(CmrpcReason::IocrMissing)
        );
        assert_eq!(err.to_u32(), 0xdb81_4002);
        // Two Input CRs and no Output CR is still "missing", not "too many".
        let mut r = req();
        let input = r.iocrs.iter().find(|c| c.iocr_type == 1).unwrap().clone();
        r.iocrs = vec![input.clone(), input];
        assert_eq!(
            validate(&r, &model).unwrap_err(),
            PnioStatus::connect_reject_cmrpc(CmrpcReason::IocrMissing)
        );
    }

    #[test]
    fn a_third_cr_exceeds_the_devices_cr_resources() {
        let mut r = req();
        let extra = r.iocrs[0].clone();
        r.iocrs.push(extra);
        let err = validate(&r, &DeviceModel::pnet_sample(DEVICE_MAC)).unwrap_err();
        assert_eq!(
            err,
            PnioStatus::connect_reject_cmrpc(CmrpcReason::OutOfProviderConsumerOrAlarmResources)
        );
        assert_eq!(err.to_u32(), 0xdb81_4007);
    }

    // AlarmCRBlockReq = 6 + 20 bytes, the last block of the golden request.
    const ALARM_CR_BLOCK_LEN: usize = 26;

    fn blocks() -> Vec<u8> {
        golden("connect_req")[REQ_BLOCKS..].to_vec()
    }

    /// On-wire length of the golden request's first block, the ARBlockReq (its body
    /// length varies with the station name).
    fn ar_block_len(b: &[u8]) -> usize {
        let (header, body) = BlockHeader::parse(b).unwrap();
        assert_eq!(header.block_type, ty::AR_BLOCK_REQ);
        BlockHeader::LEN + body.len()
    }

    #[test]
    fn missing_alarm_cr_block_is_a_wrong_block_count() {
        let b = blocks();
        let without_alarm = &b[..b.len() - ALARM_CR_BLOCK_LEN];
        assert_eq!(
            ConnectReq::parse(without_alarm).unwrap_err(),
            CmError::Reject(PnioStatus::connect_reject_cmrpc(
                CmrpcReason::WrongBlockCount
            ))
        );
    }

    #[test]
    fn duplicated_alarm_cr_or_ar_block_is_a_wrong_block_count() {
        let b = blocks();
        let mut twice_alarm = b.clone();
        twice_alarm.extend_from_slice(&b[b.len() - ALARM_CR_BLOCK_LEN..]);
        assert_eq!(
            ConnectReq::parse(&twice_alarm).unwrap_err(),
            CmError::Reject(PnioStatus::connect_reject_cmrpc(
                CmrpcReason::WrongBlockCount
            ))
        );
        let ar_len = ar_block_len(&b);
        let mut twice_ar = b[..ar_len].to_vec();
        twice_ar.extend_from_slice(&b);
        assert_eq!(
            ConnectReq::parse(&twice_ar).unwrap_err(),
            CmError::Reject(PnioStatus::connect_reject_cmrpc(
                CmrpcReason::WrongBlockCount
            ))
        );
        let without_ar = &b[ar_len..];
        assert_eq!(
            ConnectReq::parse(without_ar).unwrap_err(),
            CmError::Reject(PnioStatus::connect_reject_cmrpc(
                CmrpcReason::WrongBlockCount
            ))
        );
    }

    #[test]
    fn unknown_block_type_is_rejected_as_unknown_blocks() {
        let mut b = blocks();
        // Retype the AlarmCRBlockReq (last block) to a type this crate does not know.
        let at = b.len() - ALARM_CR_BLOCK_LEN;
        b[at..at + 2].copy_from_slice(&0x0109u16.to_be_bytes());
        assert_eq!(
            ConnectReq::parse(&b).unwrap_err(),
            CmError::Reject(PnioStatus::connect_reject_cmrpc(CmrpcReason::UnknownBlocks))
        );
    }

    #[test]
    fn truncated_args_are_args_length_invalid() {
        let b = blocks();
        // Cut inside the last block's body: its header promises more than remains.
        assert_eq!(
            ConnectReq::parse(&b[..b.len() - 4]).unwrap_err(),
            CmError::Reject(PnioStatus::connect_reject_cmrpc(
                CmrpcReason::ArgsLengthInvalid
            ))
        );
        // Cut inside a block header.
        assert_eq!(
            ConnectReq::parse(&b[..b.len() - ALARM_CR_BLOCK_LEN + 3]).unwrap_err(),
            CmError::Reject(PnioStatus::connect_reject_cmrpc(
                CmrpcReason::ArgsLengthInvalid
            ))
        );
    }

    #[test]
    fn bad_block_version_names_the_block_and_version_field() {
        let b = blocks();
        let at = b.len() - ALARM_CR_BLOCK_LEN;
        let mut high = b.clone();
        high[at + 4] = 2;
        assert_eq!(
            ConnectReq::parse(&high).unwrap_err(),
            CmError::Reject(PnioStatus::connect_reject(
                ConnectBlock::AlarmCr,
                block_field::BLOCK_VERSION_HIGH
            ))
        );
        let mut low = b.clone();
        low[at + 5] = 1;
        assert_eq!(
            ConnectReq::parse(&low).unwrap_err(),
            CmError::Reject(PnioStatus::connect_reject(
                ConnectBlock::AlarmCr,
                block_field::BLOCK_VERSION_LOW
            ))
        );
        // Same on a block this crate does not know: the type decides first.
        let mut unknown = low;
        unknown[at..at + 2].copy_from_slice(&0x0109u16.to_be_bytes());
        assert_eq!(
            ConnectReq::parse(&unknown).unwrap_err(),
            CmError::Reject(PnioStatus::connect_reject_cmrpc(CmrpcReason::UnknownBlocks))
        );
    }

    #[test]
    fn unparsable_block_body_names_the_blocks_length_field() {
        // Shorten the AlarmCRBlockReq to 10 body bytes (BlockLength 12): the header
        // is consistent with the args, but the body lacks the block's fields.
        let b = blocks();
        let at = b.len() - ALARM_CR_BLOCK_LEN;
        let mut short = b[..at + 6 + 10].to_vec();
        short[at + 2..at + 4].copy_from_slice(&12u16.to_be_bytes());
        assert_eq!(
            ConnectReq::parse(&short).unwrap_err(),
            CmError::Reject(PnioStatus::connect_reject(
                ConnectBlock::AlarmCr,
                block_field::BLOCK_LENGTH
            ))
        );
        // An ExpectedSubmoduleBlockReq whose DataDescription tag is wrong.
        let mut bad_tag = b.clone();
        let (exp_at, exp_body) = BlockHeader::read_all(&b)
            .unwrap()
            .into_iter()
            .scan(0usize, |pos, (h, body)| {
                let at = *pos;
                *pos += BlockHeader::LEN + body.len();
                Some((at, h, body))
            })
            .find(|(_, h, _)| h.block_type == ty::EXPECTED_SUBMODULE_BLOCK_REQ)
            .map(|(at, _, body)| (at, body.to_vec()))
            .unwrap();
        // Body: NumberOfAPIs(2) API(4) Slot(2) ModuleIdent(4) ModuleProperties(2)
        // NumberOfSubmodules(2) Subslot(2) SubmoduleIdent(4) SubmoduleProperties(2),
        // then the DataDescription tag at body offset 24.
        assert_eq!(exp_body[24..26], [0, 1], "first DataDescription is Input");
        bad_tag[exp_at + BlockHeader::LEN + 25] = 9;
        assert_eq!(
            ConnectReq::parse(&bad_tag).unwrap_err(),
            CmError::Reject(PnioStatus::connect_reject(
                ConnectBlock::ExpectedSubmodule,
                block_field::BLOCK_LENGTH
            ))
        );
    }
}

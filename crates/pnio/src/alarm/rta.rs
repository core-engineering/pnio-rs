//! RTA-PDU codec: the alarm transport (RTA) frame carried over Ethernet `0x8892`
//! (FrameID `0xFC01` High priority / `0xFE01` Low priority), and the two PNIO
//! blocks it carries in its DATA variant — AlarmNotification and AlarmAck.
//! Byte-exact against the p-net goldens in `testdata/alarm/`
//! (see `docs/alarm-golden-frames.md` for the wire reference).

use crate::cm::block::{BlockHeader, Cursor};
use crate::cm::{BlockError, PnioStatus};
use crate::eth::{EthError, EthHeader, MacAddr, ETHERTYPE_PROFINET};
use thiserror::Error;

// ---------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------

/// FrameID for the High priority alarm channel.
pub const FRAME_ID_HIGH: u16 = 0xFC01;
/// FrameID for the Low priority alarm channel.
pub const FRAME_ID_LOW: u16 = 0xFE01;
/// VLAN TCI (priority 6) for the High priority alarm channel.
pub const TCI_HIGH: u16 = 0xC000;
/// VLAN TCI (priority 5) for the Low priority alarm channel.
pub const TCI_LOW: u16 = 0xA000;
/// `SendSeqNum`/`AckSeqNum` value before any DATA has been sent/accepted on a fresh AR.
pub const SEQ_INIT: u16 = 0xFFFF;
/// `AckSeqNum` value before any DATA has been accepted from the peer.
pub const SEQ_NONE: u16 = 0xFFFE;
/// USI for a `ChannelDiagnosis` payload (6 bytes).
pub const USI_CHANNEL_DIAG: u16 = 0x8000;
/// USI for an `ExtChannelDiagnosis` payload (12 bytes).
pub const USI_EXT_CHANNEL_DIAG: u16 = 0x8002;

// ---------------------------------------------------------------------------------
// Priority / PduType / RtaHeader
// ---------------------------------------------------------------------------------

/// Which of the two alarm channels (VLAN priority / FrameID) a PDU travels on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    High,
    Low,
}

impl Priority {
    pub fn frame_id(self) -> u16 {
        match self {
            Priority::High => FRAME_ID_HIGH,
            Priority::Low => FRAME_ID_LOW,
        }
    }

    pub fn tci(self) -> u16 {
        match self {
            Priority::High => TCI_HIGH,
            Priority::Low => TCI_LOW,
        }
    }
}

/// RTA-PDU `PDUType` (low nibble of the type/version byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PduType {
    Data = 1,
    Nack = 2,
    Ack = 3,
    Err = 4,
}

/// The 12-byte RTA-PDU header that follows the alarm FrameID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtaHeader {
    pub dst_ref: u16,
    pub src_ref: u16,
    pub pdu_type: PduType,
    pub tack: bool,
    pub send_seq: u16,
    pub ack_seq: u16,
}

impl RtaHeader {
    pub const LEN: usize = 12;

    /// Parses the 12-byte header; returns `(header, var_part_len)`.
    pub fn parse(buf: &[u8]) -> Result<(RtaHeader, u16), RtaError> {
        if buf.len() < Self::LEN {
            return Err(RtaError::TooShort);
        }
        let u16at = |o: usize| u16::from_be_bytes([buf[o], buf[o + 1]]);
        let (version, ty) = (buf[4] >> 4, buf[4] & 0x0F);
        if version != 1 {
            return Err(RtaError::BadVersion(version));
        }
        let pdu_type = match ty {
            1 => PduType::Data,
            2 => PduType::Nack,
            3 => PduType::Ack,
            4 => PduType::Err,
            t => return Err(RtaError::BadPduType(t)),
        };
        Ok((
            RtaHeader {
                dst_ref: u16at(0),
                src_ref: u16at(2),
                pdu_type,
                tack: buf[5] & 0x10 != 0,
                send_seq: u16at(6),
                ack_seq: u16at(8),
            },
            u16at(10),
        ))
    }

    /// Serializes the header (`var_part_len` is the length of what follows).
    pub fn write(&self, out: &mut Vec<u8>, var_part_len: u16) {
        out.extend_from_slice(&self.dst_ref.to_be_bytes());
        out.extend_from_slice(&self.src_ref.to_be_bytes());
        out.push(0x10 | self.pdu_type as u8);
        out.push(0x01 | if self.tack { 0x10 } else { 0 }); // window size 1
        out.extend_from_slice(&self.send_seq.to_be_bytes());
        out.extend_from_slice(&self.ack_seq.to_be_bytes());
        out.extend_from_slice(&var_part_len.to_be_bytes());
    }
}

// ---------------------------------------------------------------------------------
// AlarmType
// ---------------------------------------------------------------------------------

/// `AlarmType` field of an AlarmNotification/AlarmAck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlarmType {
    Diagnosis,
    Process,
    Pull,
    Plug,
    Status,
    Update,
    Redundancy,
    ControlledBySupervisor,
    Released,
    PlugWrongSubmodule,
    ReturnOfSubmodule,
    DiagnosisDisappears,
    Other(u16),
}

impl AlarmType {
    pub fn from_u16(v: u16) -> Self {
        match v {
            0x0001 => AlarmType::Diagnosis,
            0x0002 => AlarmType::Process,
            0x0003 => AlarmType::Pull,
            0x0004 => AlarmType::Plug,
            0x0005 => AlarmType::Status,
            0x0006 => AlarmType::Update,
            0x0007 => AlarmType::Redundancy,
            0x0008 => AlarmType::ControlledBySupervisor,
            0x0009 => AlarmType::Released,
            0x000A => AlarmType::PlugWrongSubmodule,
            0x000B => AlarmType::ReturnOfSubmodule,
            0x000C => AlarmType::DiagnosisDisappears,
            other => AlarmType::Other(other),
        }
    }

    pub fn to_u16(self) -> u16 {
        match self {
            AlarmType::Diagnosis => 0x0001,
            AlarmType::Process => 0x0002,
            AlarmType::Pull => 0x0003,
            AlarmType::Plug => 0x0004,
            AlarmType::Status => 0x0005,
            AlarmType::Update => 0x0006,
            AlarmType::Redundancy => 0x0007,
            AlarmType::ControlledBySupervisor => 0x0008,
            AlarmType::Released => 0x0009,
            AlarmType::PlugWrongSubmodule => 0x000A,
            AlarmType::ReturnOfSubmodule => 0x000B,
            AlarmType::DiagnosisDisappears => 0x000C,
            AlarmType::Other(v) => v,
        }
    }
}

// ---------------------------------------------------------------------------------
// AlarmSpecifier
// ---------------------------------------------------------------------------------

/// `AlarmSpecifier` field: per-AR sequence number plus diagnosis-state flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AlarmSpecifier {
    pub sequence: u16,
    pub channel_diag: bool,
    pub manufacturer_diag: bool,
    pub submodule_diag: bool,
    pub ar_diag: bool,
}

impl AlarmSpecifier {
    pub fn from_u16(raw: u16) -> Self {
        AlarmSpecifier {
            sequence: raw & 0x07FF,
            channel_diag: raw & 0x0800 != 0,
            manufacturer_diag: raw & 0x1000 != 0,
            submodule_diag: raw & 0x2000 != 0,
            ar_diag: raw & 0x8000 != 0,
        }
    }

    pub fn to_u16(self) -> u16 {
        (self.sequence & 0x07FF)
            | (self.channel_diag as u16) << 11
            | (self.manufacturer_diag as u16) << 12
            | (self.submodule_diag as u16) << 13
            | (self.ar_diag as u16) << 15
    }
}

// ---------------------------------------------------------------------------------
// ChannelProperties / ChannelDiagnosis / ExtChannelDiagnosis
// ---------------------------------------------------------------------------------

/// `ChannelProperties.Maintenance` (bits 9-10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Maintenance {
    Fault = 0,
    Required = 1,
    Demanded = 2,
    Qualified = 3,
}

/// `ChannelProperties.Specifier` (bits 11-12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Specifier {
    AllDisappear = 0,
    Appears = 1,
    Disappears = 2,
    DisappearsOthersRemain = 3,
}

/// `ChannelProperties` u16 bitfield carried by `ChannelDiagnosis`/`ExtChannelDiagnosis`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelProperties {
    pub type_: u8,
    pub accumulative: bool,
    pub maintenance: Maintenance,
    pub specifier: Specifier,
    pub direction: u8,
}

impl ChannelProperties {
    pub fn from_u16(raw: u16) -> Self {
        let maintenance = match (raw >> 9) & 0x3 {
            0 => Maintenance::Fault,
            1 => Maintenance::Required,
            2 => Maintenance::Demanded,
            _ => Maintenance::Qualified,
        };
        let specifier = match (raw >> 11) & 0x3 {
            0 => Specifier::AllDisappear,
            1 => Specifier::Appears,
            2 => Specifier::Disappears,
            _ => Specifier::DisappearsOthersRemain,
        };
        ChannelProperties {
            type_: (raw & 0x00FF) as u8,
            accumulative: raw & 0x0100 != 0,
            maintenance,
            specifier,
            direction: ((raw >> 13) & 0x7) as u8,
        }
    }

    pub fn to_u16(self) -> u16 {
        self.type_ as u16
            | (self.accumulative as u16) << 8
            | (self.maintenance as u16) << 9
            | (self.specifier as u16) << 11
            | (self.direction as u16) << 13
    }
}

/// `ChannelDiagnosis` USI `0x8000` payload (6 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelDiagnosis {
    pub channel: u16,
    pub properties: ChannelProperties,
    pub error_type: u16,
}

impl ChannelDiagnosis {
    fn parse(b: &[u8]) -> Self {
        ChannelDiagnosis {
            channel: u16::from_be_bytes([b[0], b[1]]),
            properties: ChannelProperties::from_u16(u16::from_be_bytes([b[2], b[3]])),
            error_type: u16::from_be_bytes([b[4], b[5]]),
        }
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.channel.to_be_bytes());
        out.extend_from_slice(&self.properties.to_u16().to_be_bytes());
        out.extend_from_slice(&self.error_type.to_be_bytes());
    }
}

/// `ExtChannelDiagnosis` USI `0x8002` payload (12 bytes): `ChannelDiagnosis` plus
/// `ExtChannelErrorType` and `ExtChannelAddValue`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtChannelDiagnosis {
    pub channel: u16,
    pub properties: ChannelProperties,
    pub error_type: u16,
    pub ext_error_type: u16,
    pub ext_add_value: u32,
}

impl ExtChannelDiagnosis {
    fn parse(b: &[u8]) -> Self {
        ExtChannelDiagnosis {
            channel: u16::from_be_bytes([b[0], b[1]]),
            properties: ChannelProperties::from_u16(u16::from_be_bytes([b[2], b[3]])),
            error_type: u16::from_be_bytes([b[4], b[5]]),
            ext_error_type: u16::from_be_bytes([b[6], b[7]]),
            ext_add_value: u32::from_be_bytes([b[8], b[9], b[10], b[11]]),
        }
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.channel.to_be_bytes());
        out.extend_from_slice(&self.properties.to_u16().to_be_bytes());
        out.extend_from_slice(&self.error_type.to_be_bytes());
        out.extend_from_slice(&self.ext_error_type.to_be_bytes());
        out.extend_from_slice(&self.ext_add_value.to_be_bytes());
    }
}

/// The USI-specific user data carried by an `AlarmNotification`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserData {
    Channel(ChannelDiagnosis),
    ExtChannel(ExtChannelDiagnosis),
    Raw(Vec<u8>),
}

// ---------------------------------------------------------------------------------
// AlarmNotification / AlarmAck
// ---------------------------------------------------------------------------------

/// AlarmNotification block body (BlockType `0x0001` High / `0x0002` Low).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlarmNotification {
    pub alarm_type: AlarmType,
    pub api: u32,
    pub slot: u16,
    pub subslot: u16,
    pub module_ident: u32,
    pub submodule_ident: u32,
    pub specifier: AlarmSpecifier,
    pub usi: u16,
    pub data: UserData,
}

/// AlarmAck block body (BlockType `0x8001` High / `0x8002` Low).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlarmAck {
    pub alarm_type: AlarmType,
    pub api: u32,
    pub slot: u16,
    pub subslot: u16,
    pub specifier: AlarmSpecifier,
    pub status: PnioStatus,
}

/// The parsed content of an RTA DATA PDU's single PNIO block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtaData {
    Notification(AlarmNotification),
    Ack(AlarmAck),
    Unknown { block_type: u16, body: Vec<u8> },
}

/// The RTA-PDU body, keyed by `PduType`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtaBody {
    Data(RtaData),
    Ack,
    Nack,
    Err(PnioStatus),
}

/// A fully parsed/to-be-built alarm frame: channel, RTA header, and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtaPdu {
    pub priority: Priority,
    pub header: RtaHeader,
    pub body: RtaBody,
}

// ---------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RtaError {
    #[error("ethernet header: {0}")]
    Eth(#[from] EthError),
    #[error("not an alarm frame")]
    NotAlarm,
    #[error("frame too short")]
    TooShort,
    #[error("bad RTA PDU type {0:#x}")]
    BadPduType(u8),
    #[error("bad RTA-PDU version {0}")]
    BadVersion(u8),
    #[error("block error: {0}")]
    Block(#[from] BlockError),
    #[error("bad var part length: declared {declared}, available {available}")]
    BadVarPartLen { declared: u16, available: usize },
}

// ---------------------------------------------------------------------------------
// parse_frame / build_frame / is_alarm_frame
// ---------------------------------------------------------------------------------

/// Parses a complete Ethernet frame (VLAN-tagged, `0x8892`, alarm FrameID) into an
/// `RtaPdu`.
pub fn parse_frame(frame: &[u8]) -> Result<RtaPdu, RtaError> {
    let (eth, off) = EthHeader::parse(frame)?;
    if eth.ethertype != ETHERTYPE_PROFINET {
        return Err(RtaError::NotAlarm);
    }
    if frame.len() < off + 2 {
        return Err(RtaError::TooShort);
    }
    let frame_id = u16::from_be_bytes([frame[off], frame[off + 1]]);
    let priority = match frame_id {
        FRAME_ID_HIGH => Priority::High,
        FRAME_ID_LOW => Priority::Low,
        _ => return Err(RtaError::NotAlarm),
    };

    let rta_off = off + 2;
    let (header, var_part_len) = RtaHeader::parse(&frame[rta_off..])?;
    let var_off = rta_off + RtaHeader::LEN;
    let available = frame.len() - var_off;
    if var_part_len as usize > available {
        return Err(RtaError::BadVarPartLen {
            declared: var_part_len,
            available,
        });
    }
    let var_part = &frame[var_off..var_off + var_part_len as usize];

    let body = match header.pdu_type {
        PduType::Ack => RtaBody::Ack,
        PduType::Nack => RtaBody::Nack,
        PduType::Err => {
            if var_part.len() < 4 {
                return Err(RtaError::TooShort);
            }
            RtaBody::Err(PnioStatus(u32::from_be_bytes([
                var_part[0],
                var_part[1],
                var_part[2],
                var_part[3],
            ])))
        }
        PduType::Data => {
            let (bh, body_bytes) = BlockHeader::parse(var_part)?;
            RtaBody::Data(parse_rta_data(bh.block_type, body_bytes)?)
        }
    };

    Ok(RtaPdu {
        priority,
        header,
        body,
    })
}

fn parse_rta_data(block_type: u16, body: &[u8]) -> Result<RtaData, RtaError> {
    match block_type {
        0x0001 | 0x0002 => Ok(RtaData::Notification(parse_notification(body)?)),
        0x8001 | 0x8002 => Ok(RtaData::Ack(parse_ack(body)?)),
        other => Ok(RtaData::Unknown {
            block_type: other,
            body: body.to_vec(),
        }),
    }
}

fn parse_notification(body: &[u8]) -> Result<AlarmNotification, RtaError> {
    let mut c = Cursor::new(body);
    let alarm_type = AlarmType::from_u16(c.u16()?);
    let api = c.u32()?;
    let slot = c.u16()?;
    let subslot = c.u16()?;
    let module_ident = c.u32()?;
    let submodule_ident = c.u32()?;
    let specifier = AlarmSpecifier::from_u16(c.u16()?);
    let usi = c.u16()?;
    let n = c.remaining();
    let data_bytes = c.bytes(n)?;
    let data = match usi {
        USI_CHANNEL_DIAG if data_bytes.len() == 6 => {
            UserData::Channel(ChannelDiagnosis::parse(data_bytes))
        }
        USI_EXT_CHANNEL_DIAG if data_bytes.len() == 12 => {
            UserData::ExtChannel(ExtChannelDiagnosis::parse(data_bytes))
        }
        _ => UserData::Raw(data_bytes.to_vec()),
    };
    Ok(AlarmNotification {
        alarm_type,
        api,
        slot,
        subslot,
        module_ident,
        submodule_ident,
        specifier,
        usi,
        data,
    })
}

fn parse_ack(body: &[u8]) -> Result<AlarmAck, RtaError> {
    let mut c = Cursor::new(body);
    let alarm_type = AlarmType::from_u16(c.u16()?);
    let api = c.u32()?;
    let slot = c.u16()?;
    let subslot = c.u16()?;
    let specifier = AlarmSpecifier::from_u16(c.u16()?);
    let status = PnioStatus(c.u32()?);
    Ok(AlarmAck {
        alarm_type,
        api,
        slot,
        subslot,
        specifier,
        status,
    })
}

/// Builds a complete Ethernet frame from an `RtaPdu`. Does not pad to 60 bytes: the
/// goldens are unpadded sender-side captures (the NIC pads on the wire).
pub fn build_frame(dst: MacAddr, src: MacAddr, pdu: &RtaPdu) -> Vec<u8> {
    let mut var_part = Vec::new();
    match &pdu.body {
        RtaBody::Ack | RtaBody::Nack => {}
        RtaBody::Err(status) => var_part.extend_from_slice(&status.to_u32().to_be_bytes()),
        RtaBody::Data(data) => write_rta_data(&mut var_part, data, pdu.priority),
    }

    let mut out = Vec::new();
    let eth = EthHeader {
        dst,
        src,
        vlan: Some(pdu.priority.tci()),
        ethertype: ETHERTYPE_PROFINET,
    };
    eth.write(&mut out);
    out.extend_from_slice(&pdu.priority.frame_id().to_be_bytes());
    pdu.header.write(&mut out, var_part.len() as u16);
    out.extend_from_slice(&var_part);
    out
}

fn write_rta_data(out: &mut Vec<u8>, data: &RtaData, priority: Priority) {
    match data {
        RtaData::Notification(n) => write_notification(out, n, priority),
        RtaData::Ack(a) => write_ack(out, a, priority),
        RtaData::Unknown { block_type, body } => {
            BlockHeader::write(out, *block_type, body.len() as u16);
            out.extend_from_slice(body);
        }
    }
}

fn write_notification(out: &mut Vec<u8>, n: &AlarmNotification, priority: Priority) {
    let block_type = match priority {
        Priority::High => 0x0001,
        Priority::Low => 0x0002,
    };
    let mut body = Vec::new();
    body.extend_from_slice(&n.alarm_type.to_u16().to_be_bytes());
    body.extend_from_slice(&n.api.to_be_bytes());
    body.extend_from_slice(&n.slot.to_be_bytes());
    body.extend_from_slice(&n.subslot.to_be_bytes());
    body.extend_from_slice(&n.module_ident.to_be_bytes());
    body.extend_from_slice(&n.submodule_ident.to_be_bytes());
    body.extend_from_slice(&n.specifier.to_u16().to_be_bytes());
    body.extend_from_slice(&n.usi.to_be_bytes());
    match &n.data {
        UserData::Channel(d) => d.write(&mut body),
        UserData::ExtChannel(d) => d.write(&mut body),
        UserData::Raw(bytes) => body.extend_from_slice(bytes),
    }
    BlockHeader::write(out, block_type, body.len() as u16);
    out.extend_from_slice(&body);
}

fn write_ack(out: &mut Vec<u8>, a: &AlarmAck, priority: Priority) {
    let block_type = match priority {
        Priority::High => 0x8001,
        Priority::Low => 0x8002,
    };
    let mut body = Vec::new();
    body.extend_from_slice(&a.alarm_type.to_u16().to_be_bytes());
    body.extend_from_slice(&a.api.to_be_bytes());
    body.extend_from_slice(&a.slot.to_be_bytes());
    body.extend_from_slice(&a.subslot.to_be_bytes());
    body.extend_from_slice(&a.specifier.to_u16().to_be_bytes());
    body.extend_from_slice(&a.status.to_u32().to_be_bytes());
    BlockHeader::write(out, block_type, body.len() as u16);
    out.extend_from_slice(&body);
}

/// True if `frame` is a PROFINET alarm frame (tagged or untagged `0x8892` with
/// FrameID `0xFC01`/`0xFE01`).
pub fn is_alarm_frame(frame: &[u8]) -> bool {
    match EthHeader::parse(frame) {
        Ok((eth, off)) => {
            eth.ethertype == ETHERTYPE_PROFINET
                && frame.len() >= off + 2
                && matches!(
                    u16::from_be_bytes([frame[off], frame[off + 1]]),
                    FRAME_ID_HIGH | FRAME_ID_LOW
                )
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::golden_alarm;

    const DEV: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);
    const CPU: MacAddr = MacAddr([0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3c]);

    #[test]
    fn process_notification_parses_and_rebuilds_byte_exact() {
        let g = golden_alarm("alarm_process_notif");
        let pdu = parse_frame(&g).unwrap();
        assert_eq!(pdu.priority, Priority::High);
        assert_eq!(
            pdu.header,
            RtaHeader {
                dst_ref: 0,
                src_ref: 0,
                pdu_type: PduType::Data,
                tack: true,
                send_seq: 0xFFFF,
                ack_seq: 0xFFFE
            }
        );
        let RtaBody::Data(RtaData::Notification(n)) = &pdu.body else {
            panic!("not a notification")
        };
        assert_eq!(n.alarm_type, AlarmType::Process);
        assert_eq!((n.api, n.slot, n.subslot), (0, 1, 1));
        assert_eq!((n.module_ident, n.submodule_ident), (0x30, 0x130));
        assert_eq!(
            n.specifier,
            AlarmSpecifier {
                sequence: 0,
                channel_diag: false,
                manufacturer_diag: false,
                submodule_diag: false,
                ar_diag: false
            }
        );
        assert_eq!(n.usi, 0x0010);
        assert_eq!(n.data, UserData::Raw(vec![0x01]));
        assert_eq!(build_frame(CPU, DEV, &pdu), g);
    }

    #[test]
    fn diagnosis_notification_parses_ext_channel_and_rebuilds() {
        let g = golden_alarm("alarm_diag_notif");
        let pdu = parse_frame(&g).unwrap();
        assert_eq!(pdu.priority, Priority::Low);
        let RtaBody::Data(RtaData::Notification(n)) = &pdu.body else {
            panic!()
        };
        assert_eq!(n.alarm_type, AlarmType::Diagnosis);
        assert_eq!(
            n.specifier,
            AlarmSpecifier {
                sequence: 0,
                channel_diag: true,
                manufacturer_diag: false,
                submodule_diag: true,
                ar_diag: true
            }
        );
        assert_eq!(n.usi, USI_EXT_CHANNEL_DIAG);
        let UserData::ExtChannel(d) = &n.data else {
            panic!()
        };
        assert_eq!(d.channel, 4);
        assert_eq!(
            d.properties,
            ChannelProperties {
                type_: 1,
                accumulative: false,
                maintenance: Maintenance::Fault,
                specifier: Specifier::Appears,
                direction: 1
            }
        );
        assert_eq!(d.properties.to_u16(), 0x2801);
        assert_eq!(d.error_type, 0x0001);
        assert_eq!((d.ext_error_type, d.ext_add_value), (0, 0));
        assert_eq!(build_frame(CPU, DEV, &pdu), g);
    }

    #[test]
    fn channel_properties_bit_fields_round_trip() {
        for raw in [0x2801u16, 0x3801, 0x2001, 0x0000, 0x6A00] {
            assert_eq!(ChannelProperties::from_u16(raw).to_u16(), raw);
        }
        assert_eq!(
            ChannelProperties::from_u16(0x3801).specifier,
            Specifier::DisappearsOthersRemain
        );
        assert_eq!(
            ChannelProperties::from_u16(0x2001).specifier,
            Specifier::AllDisappear
        );
    }

    #[test]
    fn cpu_ack_rta_and_alarm_ack_parse() {
        let ack = parse_frame(&golden_alarm("alarm_ack_rta_high_cpu")).unwrap();
        assert_eq!(ack.body, RtaBody::Ack);
        assert_eq!(
            (ack.header.send_seq, ack.header.ack_seq, ack.header.tack),
            (0xFFFE, 0xFFFF, false)
        );
        let aa = parse_frame(&golden_alarm("alarm_ack_high_cpu")).unwrap();
        let RtaBody::Data(RtaData::Ack(a)) = &aa.body else {
            panic!()
        };
        assert_eq!(a.alarm_type, AlarmType::Process);
        assert_eq!((a.slot, a.subslot), (1, 1));
        assert_eq!(a.status, PnioStatus::OK);
        assert_eq!((aa.header.send_seq, aa.header.ack_seq), (0xFFFF, 0xFFFF));
    }

    #[test]
    fn our_ack_rta_rebuilds_byte_exact() {
        for name in ["alarm_ack_rta_high_dev", "alarm_ack_rta_low_dev"] {
            let g = golden_alarm(name);
            let pdu = parse_frame(&g).unwrap();
            assert_eq!(pdu.body, RtaBody::Ack);
            assert_eq!(build_frame(CPU, DEV, &pdu), g, "{name}");
        }
    }

    #[test]
    fn err_rta_both_ways() {
        let dev = parse_frame(&golden_alarm("alarm_err_rta_dev")).unwrap();
        assert_eq!(
            dev.body,
            RtaBody::Err(PnioStatus::new(0xCF, 0x81, 0xFD, 0x00))
        );
        assert_eq!((dev.header.send_seq, dev.header.ack_seq), (6, 5));
        assert_eq!(
            build_frame(CPU, DEV, &dev),
            golden_alarm("alarm_err_rta_dev")
        );
        let cpu = parse_frame(&golden_alarm("alarm_err_rta_cpu")).unwrap();
        assert_eq!(
            cpu.body,
            RtaBody::Err(PnioStatus::new(0xCF, 0x81, 0xFD, 0x0B))
        );
        let removed = parse_frame(&golden_alarm("alarm_err_rta_cpu_removed")).unwrap();
        assert_eq!(
            removed.body,
            RtaBody::Err(PnioStatus::new(0xCF, 0x81, 0xFD, 0x11))
        );
        assert_eq!(
            (removed.header.send_seq, removed.header.ack_seq),
            (0xFFFE, 0xFFFE)
        );
        let removed_reply = parse_frame(&golden_alarm("alarm_err_rta_dev_removed_reply")).unwrap();
        assert_eq!(removed_reply.priority, Priority::Low);
        assert_eq!(
            removed_reply.body,
            RtaBody::Err(PnioStatus::new(0xCF, 0x81, 0xFD, 0x11))
        );
        assert_eq!(
            (removed_reply.header.send_seq, removed_reply.header.ack_seq),
            (0xFFFF, 0xFFFE)
        );
        assert_eq!(
            build_frame(CPU, DEV, &removed_reply),
            golden_alarm("alarm_err_rta_dev_removed_reply")
        );
    }

    #[test]
    fn disappears_and_std_remove_goldens_rebuild() {
        for name in [
            "alarm_diag_usi_disappears",
            "alarm_diag_std_remove",
            "alarm_diag_update_appears",
            "alarm_diag_update_others_remain",
        ] {
            let g = golden_alarm(name);
            let pdu = parse_frame(&g).unwrap();
            assert_eq!(build_frame(CPU, DEV, &pdu), g, "{name}");
        }
        let RtaBody::Data(RtaData::Notification(n)) =
            parse_frame(&golden_alarm("alarm_diag_usi_disappears"))
                .unwrap()
                .body
        else {
            panic!()
        };
        assert_eq!(n.alarm_type, AlarmType::DiagnosisDisappears);
        assert_eq!(n.usi, 0x1234);
        assert_eq!(n.specifier.sequence, 5);
    }

    #[test]
    fn is_alarm_frame_discriminates() {
        assert!(is_alarm_frame(&golden_alarm("alarm_process_notif")));
        assert!(!is_alarm_frame(&golden_alarm("im0_read_req")));
        assert!(!is_alarm_frame(&crate::testutil::golden("dcp_set_req")));
    }

    #[test]
    fn malformed_frames_are_errors_not_panics() {
        let g = golden_alarm("alarm_process_notif");
        assert!(matches!(parse_frame(&g[..25]), Err(RtaError::TooShort)));
        let mut bad = g.clone();
        bad[24] = 0x15; // pdu type 5
        assert!(matches!(parse_frame(&bad), Err(RtaError::BadPduType(5))));
        let mut short = g.clone();
        short[30] = 0x00;
        short[31] = 0x40; // var_part_len 64 > available
        assert!(matches!(
            parse_frame(&short),
            Err(RtaError::BadVarPartLen { .. })
        ));
    }
}

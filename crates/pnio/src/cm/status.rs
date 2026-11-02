//! PNIOStatus: the 4-byte `(Code, Decode, Code1, Code2)` status word carried in RPC
//! response `status` fields and in PNIO block error indications (IEC 61158-6-10).

/// Identifies which Connect-request block a `connect_reject` status refers to
/// (goes into `PNIOStatus.Code1` for a Connect-problem rejection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ConnectBlock {
    /// ARBlockReq — the controller's AR parameters.
    ArBlock = 1,
    /// IOCRBlockReq — one of the controller's CR parameters.
    IocrBlock = 2,
    /// ExpectedSubmoduleBlockReq — the controller's expected slot/submodule layout.
    ExpectedSubmodule = 3,
    /// AlarmCRBlockReq — the controller's alarm CR parameters.
    AlarmCr = 4,
}

/// `ErrorCode1 = 64` (CMRPC) Connect rejections: problems with the request as a
/// whole rather than with one field of one block (goes into `PNIOStatus.Code2` of
/// [`PnioStatus::connect_reject_cmrpc`]). Values per IEC 61158-6-10.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CmrpcReason {
    /// The NDR args do not hold the blocks they claim to (truncated or overlong).
    ArgsLengthInvalid = 0,
    /// A block of a type this device does not know.
    UnknownBlocks = 1,
    /// No Input or no Output IOCRBlockReq.
    IocrMissing = 2,
    /// A required block is missing or duplicated (ARBlockReq, AlarmCRBlockReq).
    WrongBlockCount = 3,
    /// No AR resource left on the device.
    OutOfArResources = 4,
    /// The device is in a state in which it cannot take this Connect.
    StateConflict = 6,
    /// More provider, consumer or alarm CRs than the device supports (e.g. more than
    /// one Input and one Output CR on a legacy AR).
    OutOfProviderConsumerOrAlarmResources = 7,
    /// No memory left to take the AR.
    OutOfMemory = 8,
}

// ---------------------------------------------------------------------------------
// ErrorCode2 field positions of a Connect-request block.
// ---------------------------------------------------------------------------------
//
// `ErrorCode2` of a `(block, field)` Connect rejection names the offending field by
// its position in the block, counted from the block header: `BlockType` = 0,
// `BlockLength` = 1, `BlockVersionHigh` = 2, `BlockVersionLow` = 3, then the block's
// own fields in wire order, one number per field whatever its size. This is how
// IEC 61158-6-10 numbers them and how a controller's diagnostic buffer decodes them.
// Within a repeated group (an API, a submodule) the number is that of the field's
// first occurrence.

/// `ErrorCode2` positions shared by every block: the 6-byte block header.
pub mod block_field {
    /// `BlockType`: the block's type is not the one expected at this position.
    pub const BLOCK_TYPE: u8 = 0;
    /// `BlockLength`: the block's body does not hold the fields its type promises.
    pub const BLOCK_LENGTH: u8 = 1;
    /// `BlockVersionHigh`.
    pub const BLOCK_VERSION_HIGH: u8 = 2;
    /// `BlockVersionLow`.
    pub const BLOCK_VERSION_LOW: u8 = 3;
}

/// `ErrorCode2` positions of an `(ArBlock, …)` reject: the fields of `ARBlockReq`.
pub mod ar_field {
    /// `ARType`.
    pub const AR_TYPE: u8 = 4;
    /// `ARUUID`.
    pub const AR_UUID: u8 = 5;
    /// `SessionKey`.
    pub const SESSION_KEY: u8 = 6;
    /// `CMInitiatorMacAdd`.
    pub const INITIATOR_MAC: u8 = 7;
    /// `CMInitiatorObjectUUID`.
    pub const INITIATOR_OBJECT_UUID: u8 = 8;
    /// `ARProperties`.
    pub const AR_PROPERTIES: u8 = 9;
    /// `CMInitiatorActivityTimeoutFactor`.
    pub const ACTIVITY_TIMEOUT_FACTOR: u8 = 10;
    /// `CMInitiatorUDPRTPort`.
    pub const INITIATOR_UDP_RT_PORT: u8 = 11;
    /// `StationNameLength`.
    pub const STATION_NAME_LENGTH: u8 = 12;
    /// `CMInitiatorStationName`.
    pub const STATION_NAME: u8 = 13;
}

/// `ErrorCode2` positions of an `(IocrBlock, …)` reject: the fields of `IOCRBlockReq`.
pub mod iocr_field {
    /// `IOCRType`.
    pub const IOCR_TYPE: u8 = 4;
    /// `IOCRReference`.
    pub const REFERENCE: u8 = 5;
    /// `LT` (LengthType/EtherType).
    pub const LT: u8 = 6;
    /// `IOCRProperties`.
    pub const PROPERTIES: u8 = 7;
    /// `DataLength`.
    pub const DATA_LENGTH: u8 = 8;
    /// `FrameID`.
    pub const FRAME_ID: u8 = 9;
    /// `SendClockFactor`.
    pub const SEND_CLOCK_FACTOR: u8 = 10;
    /// `ReductionRatio`.
    pub const REDUCTION_RATIO: u8 = 11;
    /// `Phase`.
    pub const PHASE: u8 = 12;
    /// `Sequence`.
    pub const SEQUENCE: u8 = 13;
    /// `FrameSendOffset`.
    pub const FRAME_SEND_OFFSET: u8 = 14;
    /// `WatchdogFactor`.
    pub const WATCHDOG_FACTOR: u8 = 15;
    /// `DataHoldFactor`.
    pub const DATA_HOLD_FACTOR: u8 = 16;
    /// `IOCRTagHeader`.
    pub const TAG_HEADER: u8 = 17;
    /// `IOCRMulticastMACAdd`.
    pub const MULTICAST_MAC: u8 = 18;
    /// `NumberOfAPIs`.
    pub const NUMBER_OF_APIS: u8 = 19;
}

/// `ErrorCode2` positions of an `(ExpectedSubmodule, …)` reject: the fields of
/// `ExpectedSubmoduleBlockReq`.
pub mod expected_submodule_field {
    /// `NumberOfAPIs`.
    pub const NUMBER_OF_APIS: u8 = 4;
    /// `API`.
    pub const API: u8 = 5;
    /// `SlotNumber`.
    pub const SLOT: u8 = 6;
    /// `ModuleIdentNumber`.
    pub const MODULE_IDENT: u8 = 7;
    /// `ModuleProperties`.
    pub const MODULE_PROPERTIES: u8 = 8;
    /// `NumberOfSubmodules`.
    pub const NUMBER_OF_SUBMODULES: u8 = 9;
    /// `SubslotNumber`.
    pub const SUBSLOT: u8 = 10;
    /// `SubmoduleIdentNumber`.
    pub const SUBMODULE_IDENT: u8 = 11;
    /// `SubmoduleProperties`.
    pub const SUBMODULE_PROPERTIES: u8 = 12;
    /// `DataDescription`.
    pub const DATA_DESCRIPTION: u8 = 13;
    /// `SubmoduleDataLength`.
    pub const SUBMODULE_DATA_LENGTH: u8 = 14;
    /// `LengthIOPS`.
    pub const LENGTH_IOPS: u8 = 15;
    /// `LengthIOCS`.
    pub const LENGTH_IOCS: u8 = 16;
}

/// `ErrorCode2` positions of an `(AlarmCr, …)` reject: the fields of `AlarmCRBlockReq`.
pub mod alarm_cr_field {
    /// `AlarmCRType`.
    pub const ALARM_CR_TYPE: u8 = 4;
    /// `LT` (LengthType/EtherType).
    pub const LT: u8 = 5;
    /// `AlarmCRProperties`.
    pub const PROPERTIES: u8 = 6;
    /// `RTATimeoutFactor`.
    pub const RTA_TIMEOUT_FACTOR: u8 = 7;
    /// `RTARetries`.
    pub const RTA_RETRIES: u8 = 8;
    /// `LocalAlarmReference`.
    pub const LOCAL_ALARM_REFERENCE: u8 = 9;
    /// `MaxAlarmDataLength`.
    pub const MAX_ALARM_DATA_LENGTH: u8 = 10;
    /// `AlarmCRTagHeaderHigh`.
    pub const TAG_HEADER_HIGH: u8 = 11;
    /// `AlarmCRTagHeaderLow`.
    pub const TAG_HEADER_LOW: u8 = 12;
}

/// The 4-byte PNIOStatus word: `Code` (high byte) / `ErrorDecode` / `ErrorCode1` /
/// `ErrorCode2` (low byte), packed big-endian into a `u32` as it appears on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PnioStatus(pub u32);

impl PnioStatus {
    /// PNIOStatus.Code = 0: no error.
    pub const OK: PnioStatus = PnioStatus(0);

    /// `ErrorCode1` of a CMRPC Connect rejection (see [`CmrpcReason`]).
    const CODE1_CMRPC: u8 = 64;

    /// Packs the four status bytes big-endian into one `u32`.
    pub fn new(code: u8, decode: u8, code1: u8, code2: u8) -> PnioStatus {
        PnioStatus(u32::from_be_bytes([code, decode, code1, code2]))
    }

    /// `PNIOStatus.Code` (bits 24-31): the error class, `0` = OK.
    pub fn code(&self) -> u8 {
        (self.0 >> 24) as u8
    }

    /// `PNIOStatus.ErrorDecode` (bits 16-23): how `code1`/`code2` are to be interpreted.
    pub fn decode(&self) -> u8 {
        (self.0 >> 16) as u8
    }

    /// `PNIOStatus.ErrorCode1` (bits 8-15).
    pub fn code1(&self) -> u8 {
        (self.0 >> 8) as u8
    }

    /// `PNIOStatus.ErrorCode2` (bits 0-7).
    pub fn code2(&self) -> u8 {
        self.0 as u8
    }

    /// True if `Code == 0` (no error).
    pub fn is_ok(&self) -> bool {
        self.0 == 0
    }

    /// The raw big-endian-packed `u32` as it appears on the wire.
    pub fn to_u32(&self) -> u32 {
        self.0
    }

    /// Connect problem, error in the given block (`Code = 0xDB`, `Decode = PNIO
    /// 0x81`, `Code1` = block type, `Code2` = offending field). `field` is the field's
    /// position counted from the block header (`BlockType` = 0); use the per-block
    /// constants ([`ar_field`], [`iocr_field`], [`expected_submodule_field`],
    /// [`alarm_cr_field`], [`block_field`]) rather than literal numbers. Numbering per
    /// IEC 61158-6-10.
    pub fn connect_reject(block: ConnectBlock, field: u8) -> PnioStatus {
        PnioStatus::new(0xDB, 0x81, block as u8, field)
    }

    /// Connect problem with the request as a whole (`Code = 0xDB`, `Decode = PNIO
    /// 0x81`, `Code1 = 64` CMRPC, `Code2` = [`CmrpcReason`]): wrong block count,
    /// unknown block, missing IOCR, bad args length. Values per IEC 61158-6-10.
    pub fn connect_reject_cmrpc(reason: CmrpcReason) -> PnioStatus {
        PnioStatus::new(0xDB, 0x81, Self::CODE1_CMRPC, reason as u8)
    }

    /// CMDEV: Connect problem, AR already exists (state conflict). Convention as used
    /// by open PROFINET stacks; re-verify against the standard (FOLLOWUPS).
    pub fn connect_ar_already_exists() -> PnioStatus {
        PnioStatus::new(0xDB, 0x81, 0x3d, 0x0e)
    }

    /// PNIORW: read/write problem, access error, invalid index. Convention as used by
    /// open PROFINET stacks; re-verify against the standard (FOLLOWUPS).
    pub fn write_index_unsupported() -> PnioStatus {
        PnioStatus::new(0xDF, 0x80, 0xB0, 0x00)
    }

    /// CMDEV: Write problem, state conflict — the record's `ar_uuid` does not match
    /// the established AR's. Convention as used by open PROFINET stacks; re-verify
    /// against the standard (FOLLOWUPS).
    pub fn write_wrong_ar() -> PnioStatus {
        PnioStatus::new(0xDF, 0x81, 0x3d, 0x03)
    }

    /// PNIORW: write problem, resource unavailable — the AR's accumulated
    /// parameter-write records would exceed the per-AR cap. Convention as used by
    /// open PROFINET stacks; re-verify against the standard (FOLLOWUPS).
    pub fn write_resource_unavailable() -> PnioStatus {
        PnioStatus::new(0xDF, 0x80, 0xC3, 0x00)
    }

    /// CMDEV: Control problem, wrong state. Convention as used by open PROFINET
    /// stacks; re-verify against the standard (FOLLOWUPS).
    pub fn control_wrong_state() -> PnioStatus {
        PnioStatus::new(0xDD, 0x81, 0x3d, 0x03)
    }

    /// RPC: requested service not supported. Convention as used by open PROFINET
    /// stacks; re-verify against the standard (FOLLOWUPS).
    pub fn service_unsupported() -> PnioStatus {
        PnioStatus::new(0x81, 0x81, 0x05, 0x00)
    }

    /// PNIORW: read/write problem, access error, invalid index. Used for `Read`/
    /// `ReadImplicit` requests, which are deliberately out of scope: notably the
    /// CPU's periodic probe of index `0xfbff` ("RPC connection monitoring") when it
    /// isn't receiving cyclic data. Kept distinct from `service_unsupported` (whose
    /// `(Code, Decode) = (0x81, 0x81)` Wireshark decodes as "Connect: Faulty
    /// PrmServerBlockReq", which misleads for a Read). Convention as used by open
    /// PROFINET stacks; re-verify against the standard (FOLLOWUPS).
    pub fn read_index_unsupported() -> PnioStatus {
        PnioStatus::new(0xDE, 0x80, 0xB0, 0x00)
    }

    /// PNIORW: read problem, state conflict — the request has no established AR, or
    /// its `ar_uuid` does not match the established one's. Same `(Code1, Code2)` tail
    /// as `write_wrong_ar` (`0x3d, 0x03`), mirrored for the Read direction: `Code =
    /// 0xDE`, the same family byte `read_index_unsupported` uses. Convention as used
    /// by open PROFINET stacks; re-verify against the standard (FOLLOWUPS).
    pub fn read_wrong_ar() -> PnioStatus {
        PnioStatus::new(0xDE, 0x81, 0x3d, 0x03)
    }

    /// PNIORW: write problem, invalid parameter — a Write record on an I&M1-3 index
    /// (`0xAFF1..=0xAFF3`) whose shape [`crate::im::ImStore::write`] rejects, or whose
    /// `(slot, subslot)` is absent from the device model. `Code = 0xDF` (the Write
    /// family byte, as in `write_wrong_ar`/`write_index_unsupported`), `Code1 = 0xB0`
    /// (the same index/parameter family byte as `write_index_unsupported`), `Code2 =
    /// 0x02` ("invalid parameter", distinct from `write_index_unsupported`'s "invalid
    /// index" `0x00`). Not currently placed on the wire (per-record Write statuses are
    /// out of scope; the Write response keeps the AR's own OK status), used only to
    /// log the rejection. Convention as used by open PROFINET stacks; re-verify
    /// against the standard (FOLLOWUPS).
    pub fn write_invalid_parameter() -> PnioStatus {
        PnioStatus::new(0xDF, 0x81, 0xB0, 0x02)
    }

    /// `CF 81 FD xx`: RTA error, PNIO, RTA_ERR_CLS_PROTOCOL, `code2` per spec §4.3.
    pub fn rta_abort(code2: u8) -> PnioStatus {
        PnioStatus::new(0xCF, 0x81, 0xFD, code2)
    }

    /// RTA_ERR_ABORT: device HmiTimeout (DHT) expired.
    pub const RTA_ABORT_DHT_EXPIRED: u8 = 1;
    /// RTA_ERR_ABORT: an alarm send failed.
    pub const RTA_ABORT_ALARM_SEND_FAILED: u8 = 3;
    /// RTA_ERR_ABORT: device HmiTimeout watchdog expired.
    pub const RTA_ABORT_DHT_WDT_EXPIRED: u8 = 5;
    /// RTA_ERR_ABORT: alarm indication returned an error.
    pub const RTA_ABORT_ALARM_IND_ERR: u8 = 11;
    /// RTA_ERR_ABORT: the AR was removed.
    pub const RTA_ABORT_AR_REMOVED: u8 = 17;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_and_unpack() {
        let s = PnioStatus::new(0xdb, 0x81, 0x03, 0x0e);
        assert_eq!(s.to_u32(), 0xdb81_030e);
        assert_eq!(
            (s.code(), s.decode(), s.code1(), s.code2()),
            (0xdb, 0x81, 0x03, 0x0e)
        );
        assert!(!s.is_ok());
        assert!(PnioStatus::OK.is_ok());
        assert_eq!(
            PnioStatus::connect_reject(
                ConnectBlock::ExpectedSubmodule,
                expected_submodule_field::SUBMODULE_DATA_LENGTH
            ),
            s
        );
    }

    #[test]
    fn cmrpc_reject_uses_code1_64() {
        let s = PnioStatus::connect_reject_cmrpc(CmrpcReason::WrongBlockCount);
        assert_eq!(s.to_u32(), 0xdb81_4003);
        assert_eq!(
            PnioStatus::connect_reject_cmrpc(CmrpcReason::ArgsLengthInvalid).to_u32(),
            0xdb81_4000
        );
        assert_eq!(
            PnioStatus::connect_reject_cmrpc(CmrpcReason::OutOfProviderConsumerOrAlarmResources)
                .code2(),
            7
        );
    }

    #[test]
    fn field_positions_count_from_the_block_header() {
        // One number per field, in wire order, after the 4 header positions.
        assert_eq!(block_field::BLOCK_VERSION_LOW + 1, ar_field::AR_TYPE);
        assert_eq!(ar_field::AR_TYPE, iocr_field::IOCR_TYPE);
        assert_eq!(iocr_field::IOCR_TYPE, alarm_cr_field::ALARM_CR_TYPE);
        assert_eq!(iocr_field::FRAME_ID, iocr_field::DATA_LENGTH + 1);
        assert_eq!(iocr_field::NUMBER_OF_APIS, 19);
        assert_eq!(expected_submodule_field::SUBMODULE_DATA_LENGTH, 14);
        assert_eq!(alarm_cr_field::MAX_ALARM_DATA_LENGTH, 10);
    }

    #[test]
    fn read_index_unsupported_round_trips() {
        let s = PnioStatus::read_index_unsupported();
        assert_eq!(s.to_u32(), 0xde80_b000);
        assert_eq!(
            (s.code(), s.decode(), s.code1(), s.code2()),
            (0xde, 0x80, 0xb0, 0x00)
        );
        assert_ne!(s, PnioStatus::service_unsupported());
    }
}

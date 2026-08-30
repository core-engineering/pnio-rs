//! PNIOStatus: the 4-byte `(Code, Decode, Code1, Code2)` status word carried in RPC
//! response `status` fields and in PNIO block error indications (IEC 61158-6-10).

/// Identifies which Connect-request block a `connect_reject` status refers to
/// (goes into `PNIOStatus.Code1` for a Connect-problem rejection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ConnectBlock {
    ArBlock = 1,
    IocrBlock = 2,
    ExpectedSubmodule = 3,
    AlarmCr = 4,
}

/// The 4-byte PNIOStatus word: `Code` (high byte) / `ErrorDecode` / `ErrorCode1` /
/// `ErrorCode2` (low byte), packed big-endian into a `u32` as it appears on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PnioStatus(pub u32);

impl PnioStatus {
    /// PNIOStatus.Code = 0: no error.
    pub const OK: PnioStatus = PnioStatus(0);

    pub fn new(code: u8, decode: u8, code1: u8, code2: u8) -> PnioStatus {
        PnioStatus(u32::from_be_bytes([code, decode, code1, code2]))
    }

    pub fn code(&self) -> u8 {
        (self.0 >> 24) as u8
    }

    pub fn decode(&self) -> u8 {
        (self.0 >> 16) as u8
    }

    pub fn code1(&self) -> u8 {
        (self.0 >> 8) as u8
    }

    pub fn code2(&self) -> u8 {
        self.0 as u8
    }

    pub fn is_ok(&self) -> bool {
        self.0 == 0
    }

    pub fn to_u32(&self) -> u32 {
        self.0
    }

    /// CMRPC: Connect problem, error in the given block (`Code = 0xDB`,
    /// `Decode = PNIORW 0x81`, `Code1` = block type, `Code2` = offending field).
    /// Convention as used by open PROFINET stacks (e.g. p-net); re-verify against the
    /// purchased IEC 61158-6-10 text (tracked in FOLLOWUPS).
    pub fn connect_reject(block: ConnectBlock, field: u8) -> PnioStatus {
        PnioStatus::new(0xDB, 0x81, block as u8, field)
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
    /// `(slot, subslot)` is not the writable one (the DAP). `Code = 0xDF` (the Write
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
        let s = PnioStatus::new(0xdb, 0x81, 0x03, 0x07);
        assert_eq!(s.to_u32(), 0xdb81_0307);
        assert_eq!(
            (s.code(), s.decode(), s.code1(), s.code2()),
            (0xdb, 0x81, 0x03, 0x07)
        );
        assert!(!s.is_ok());
        assert!(PnioStatus::OK.is_ok());
        assert_eq!(
            PnioStatus::connect_reject(ConnectBlock::ExpectedSubmodule, 7),
            s
        );
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

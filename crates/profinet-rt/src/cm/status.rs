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

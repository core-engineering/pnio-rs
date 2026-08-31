//! DCE-RPC v4 connectionless (CL) codec used by PROFINET IO (UDP port 34964).

pub mod header;
pub mod ndr;
pub mod transport;
pub mod udp;
pub mod uuid;

pub use header::{
    Opnum, PacketType, RpcHeader, FLAG1_FRAG, FLAG1_IDEMPOTENT, FLAG1_LAST_FRAG, FLAG1_NO_FACK,
};
pub use ndr::{NdrRequest, NdrResponse};
pub use transport::{MockRpcTransport, RpcTransport};
pub use udp::UdpRpcTransport;
pub use uuid::{Uuid, PNIO_CONTROLLER_INTERFACE, PNIO_DEVICE_INTERFACE};

use thiserror::Error;

/// UDP port of the PNIO context manager (device side listens here; controllers too).
pub const PNIO_UDP_PORT: u16 = 34964;

/// Errors from parsing/building DCE-RPC PDUs and their NDR payloads, or from the
/// underlying transport.
#[derive(Debug, Error)]
pub enum RpcError {
    /// Fewer bytes available than the RPC header or NDR shape being parsed needs.
    #[error("rpc buffer too short: need {need}, have {have}")]
    TooShort {
        /// Bytes the parse needs.
        need: usize,
        /// Bytes actually available.
        have: usize,
    },
    /// `rpc_vers` is not 4.
    #[error("unsupported DCE-RPC version {0} (expected 4)")]
    BadVersion(u8),
    /// `ptype` is not one this crate handles (Request/Response).
    #[error("unsupported DCE-RPC packet type {0}")]
    UnsupportedPtype(u8),
    /// The PDU declares fragmentation, which this crate does not support.
    #[error("fragmented DCE-RPC PDU not supported (frag_num {frag_num}, flags1 {flags1:#04x})")]
    Fragmented {
        /// The PDU's `frag_num` field.
        frag_num: u16,
        /// The PDU's `flags1` byte.
        flags1: u8,
    },
    /// The PDU's `interface` UUID is not the one the caller expected.
    #[error("unexpected interface UUID {0}")]
    BadInterface(Uuid),
    /// The NDR payload's structure does not match what the opnum requires.
    #[error("NDR mismatch: {0}")]
    NdrMismatch(&'static str),
    /// The underlying transport (socket) failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

// `std::io::Error` has no `PartialEq`, so this can't be derived; compare `Io` by `ErrorKind`
// (good enough for tests, which only need to distinguish variants) and every other variant
// field-by-field.
impl PartialEq for RpcError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                RpcError::TooShort { need: n1, have: h1 },
                RpcError::TooShort { need: n2, have: h2 },
            ) => n1 == n2 && h1 == h2,
            (RpcError::BadVersion(a), RpcError::BadVersion(b)) => a == b,
            (RpcError::UnsupportedPtype(a), RpcError::UnsupportedPtype(b)) => a == b,
            (
                RpcError::Fragmented {
                    frag_num: f1,
                    flags1: fl1,
                },
                RpcError::Fragmented {
                    frag_num: f2,
                    flags1: fl2,
                },
            ) => f1 == f2 && fl1 == fl2,
            (RpcError::BadInterface(a), RpcError::BadInterface(b)) => a == b,
            (RpcError::NdrMismatch(a), RpcError::NdrMismatch(b)) => a == b,
            (RpcError::Io(a), RpcError::Io(b)) => a.kind() == b.kind(),
            _ => false,
        }
    }
}

/// NDR data representation: only the byte order matters for PNIO (char = ASCII, float = IEEE).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Drep {
    /// `true` = little-endian (as sent by a controller, e.g. `drep[0] = 0x10`),
    /// `false` = big-endian (as sent by this device's responses, `drep[0] = 0x00`).
    pub little_endian: bool,
}

impl Drep {
    /// Big-endian representation (`drep[0] = 0x00`), as used by this device's responses.
    pub const BIG: Drep = Drep {
        little_endian: false,
    };
    /// Little-endian representation (`drep[0] = 0x10`), as sent by controllers on requests.
    pub const LITTLE: Drep = Drep {
        little_endian: true,
    };

    /// Decodes `drep[0]`: bit 4 set means little-endian.
    pub fn from_byte(b: u8) -> Drep {
        Drep {
            little_endian: b & 0x10 != 0,
        }
    }
    /// Encodes the 3-byte `drep` field (only byte 0 carries information for PNIO).
    pub fn to_bytes(self) -> [u8; 3] {
        [if self.little_endian { 0x10 } else { 0x00 }, 0, 0]
    }
    /// Reads a `u16` from `b[0..2]` in this representation's byte order.
    pub fn u16(self, b: &[u8]) -> u16 {
        let a = [b[0], b[1]];
        if self.little_endian {
            u16::from_le_bytes(a)
        } else {
            u16::from_be_bytes(a)
        }
    }
    /// Reads a `u32` from `b[0..4]` in this representation's byte order.
    pub fn u32(self, b: &[u8]) -> u32 {
        let a = [b[0], b[1], b[2], b[3]];
        if self.little_endian {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        }
    }
    /// Appends `v` to `out` in this representation's byte order.
    pub fn put_u16(self, out: &mut Vec<u8>, v: u16) {
        out.extend_from_slice(&if self.little_endian {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        });
    }
    /// Appends `v` to `out` in this representation's byte order.
    pub fn put_u32(self, out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&if self.little_endian {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        });
    }
}

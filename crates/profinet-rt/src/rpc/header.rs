//! DCE-RPC v4 connectionless (CL) header codec (80 bytes, byte order given by DREP).

use super::{Drep, RpcError, Uuid};
#[cfg(test)]
use super::{PNIO_CONTROLLER_INTERFACE, PNIO_DEVICE_INTERFACE};

/// DCE-RPC version this codec speaks; anything else is rejected as [`RpcError::BadVersion`].
const VERSION: u8 = 4;

pub const FLAG1_LAST_FRAG: u8 = 0x02;
pub const FLAG1_FRAG: u8 = 0x04;
pub const FLAG1_NO_FACK: u8 = 0x08;
pub const FLAG1_IDEMPOTENT: u8 = 0x20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketType {
    Request = 0,
    Ping = 1,
    Response = 2,
    Fault = 3,
    Working = 4,
    Nocall = 5,
    Reject = 6,
    Ack = 7,
    ClCancel = 8,
    Fack = 9,
    CancelAck = 10,
}

impl PacketType {
    pub fn from_u8(b: u8) -> Result<PacketType, RpcError> {
        match b {
            0 => Ok(PacketType::Request),
            1 => Ok(PacketType::Ping),
            2 => Ok(PacketType::Response),
            3 => Ok(PacketType::Fault),
            4 => Ok(PacketType::Working),
            5 => Ok(PacketType::Nocall),
            6 => Ok(PacketType::Reject),
            7 => Ok(PacketType::Ack),
            8 => Ok(PacketType::ClCancel),
            9 => Ok(PacketType::Fack),
            10 => Ok(PacketType::CancelAck),
            other => Err(RpcError::UnsupportedPtype(other)),
        }
    }

    pub fn to_u8(self) -> u8 {
        self as u8
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opnum {
    Connect = 0,
    Release = 1,
    Read = 2,
    Write = 3,
    Control = 4,
    ReadImplicit = 5,
}

impl Opnum {
    pub fn from_u16(v: u16) -> Option<Opnum> {
        match v {
            0 => Some(Opnum::Connect),
            1 => Some(Opnum::Release),
            2 => Some(Opnum::Read),
            3 => Some(Opnum::Write),
            4 => Some(Opnum::Control),
            5 => Some(Opnum::ReadImplicit),
            _ => None,
        }
    }

    pub fn to_u16(self) -> u16 {
        self as u16
    }
}

/// The 80-byte DCE-RPC v4 connectionless header, PROFINET IO's PDU envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcHeader {
    pub ptype: PacketType,
    pub flags1: u8,
    pub flags2: u8,
    pub drep: Drep,
    pub serial_hi: u8,
    pub object: Uuid,
    pub interface: Uuid,
    pub activity: Uuid,
    pub server_boot: u32,
    pub if_version: u32,
    pub seq_num: u32,
    pub opnum: u16,
    pub ihint: u16,
    pub ahint: u16,
    pub frag_len: u16,
    pub frag_num: u16,
    pub auth_proto: u8,
    pub serial_lo: u8,
}

impl RpcHeader {
    pub const LEN: usize = 80;

    pub fn parse(buf: &[u8]) -> Result<RpcHeader, RpcError> {
        if buf.len() < Self::LEN {
            return Err(RpcError::TooShort {
                need: Self::LEN,
                have: buf.len(),
            });
        }
        if buf[0] != VERSION {
            return Err(RpcError::BadVersion(buf[0]));
        }
        let ptype = PacketType::from_u8(buf[1])?;
        let flags1 = buf[2];
        let flags2 = buf[3];
        let drep = Drep::from_byte(buf[4]);
        let serial_hi = buf[7];
        let object = Uuid::read(&buf[8..24], drep).unwrap();
        let interface = Uuid::read(&buf[24..40], drep).unwrap();
        let activity = Uuid::read(&buf[40..56], drep).unwrap();
        let server_boot = drep.u32(&buf[56..60]);
        let if_version = drep.u32(&buf[60..64]);
        let seq_num = drep.u32(&buf[64..68]);
        let opnum = drep.u16(&buf[68..70]);
        let ihint = drep.u16(&buf[70..72]);
        let ahint = drep.u16(&buf[72..74]);
        let frag_len = drep.u16(&buf[74..76]);
        let frag_num = drep.u16(&buf[76..78]);
        let auth_proto = buf[78];
        let serial_lo = buf[79];

        if frag_num != 0 || flags1 & FLAG1_FRAG != 0 {
            return Err(RpcError::Fragmented { frag_num, flags1 });
        }

        Ok(RpcHeader {
            ptype,
            flags1,
            flags2,
            drep,
            serial_hi,
            object,
            interface,
            activity,
            server_boot,
            if_version,
            seq_num,
            opnum,
            ihint,
            ahint,
            frag_len,
            frag_num,
            auth_proto,
            serial_lo,
        })
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(VERSION);
        out.push(self.ptype.to_u8());
        out.push(self.flags1);
        out.push(self.flags2);
        out.extend_from_slice(&self.drep.to_bytes());
        out.push(self.serial_hi);
        self.object.write(out, self.drep);
        self.interface.write(out, self.drep);
        self.activity.write(out, self.drep);
        self.drep.put_u32(out, self.server_boot);
        self.drep.put_u32(out, self.if_version);
        self.drep.put_u32(out, self.seq_num);
        self.drep.put_u16(out, self.opnum);
        self.drep.put_u16(out, self.ihint);
        self.drep.put_u16(out, self.ahint);
        self.drep.put_u16(out, self.frag_len);
        self.drep.put_u16(out, self.frag_num);
        out.push(self.auth_proto);
        out.push(self.serial_lo);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{golden, RPC_OFF};

    #[test]
    fn parse_connect_request_header_little_endian() {
        let f = golden("connect_req");
        let h = RpcHeader::parse(&f[RPC_OFF..]).unwrap();
        assert_eq!(h.ptype, PacketType::Request);
        assert_eq!(h.flags1, FLAG1_IDEMPOTENT);
        assert!(h.drep.little_endian);
        assert_eq!(h.object.to_string(), "dea00000-6c97-11d1-8271-000100020493");
        assert_eq!(h.interface, PNIO_DEVICE_INTERFACE);
        assert_eq!(
            h.activity.to_string(),
            "d2aca1ae-0032-1010-b058-ec1c5d61e73f"
        );
        assert_eq!(h.if_version, 1);
        assert_eq!(h.seq_num, 0);
        assert_eq!(h.opnum, Opnum::Connect.to_u16());
        assert_eq!((h.ihint, h.ahint), (0xffff, 0xffff));
        assert_eq!(h.frag_len, 577);
        assert_eq!(h.frag_num, 0);
    }

    #[test]
    fn parse_appready_request_header_big_endian() {
        let f = golden("appready_req");
        let h = RpcHeader::parse(&f[RPC_OFF..]).unwrap();
        assert!(!h.drep.little_endian);
        assert_eq!(h.interface, PNIO_CONTROLLER_INTERFACE);
        assert_eq!(h.object.to_string(), "dea00000-6c97-11d1-8271-1064010e002a");
        assert_eq!(h.opnum, 4);
        assert_eq!(h.frag_len, 52);
    }

    #[test]
    fn header_roundtrip_byte_exact_both_dreps() {
        for name in ["connect_req", "connect_res", "appready_req", "appready_res"] {
            let f = golden(name);
            let h = RpcHeader::parse(&f[RPC_OFF..]).unwrap();
            let mut out = Vec::new();
            h.write(&mut out);
            assert_eq!(out, &f[RPC_OFF..RPC_OFF + RpcHeader::LEN], "{name}");
        }
    }

    #[test]
    fn rejects_short_bad_version_and_fragments() {
        assert!(matches!(
            RpcHeader::parse(&[4u8; 10]),
            Err(RpcError::TooShort { need: 80, have: 10 })
        ));
        let mut f = golden("connect_req")[RPC_OFF..].to_vec();
        f[0] = 5;
        assert!(matches!(RpcHeader::parse(&f), Err(RpcError::BadVersion(5))));
        let mut f = golden("connect_req")[RPC_OFF..].to_vec();
        f[2] |= FLAG1_FRAG;
        assert!(matches!(
            RpcHeader::parse(&f),
            Err(RpcError::Fragmented { .. })
        ));
        let mut f = golden("connect_req")[RPC_OFF..].to_vec();
        f[76] = 1; // frag_num (LE low byte)
        assert!(matches!(
            RpcHeader::parse(&f),
            Err(RpcError::Fragmented { .. })
        ));
    }

    #[test]
    fn packet_type_roundtrip_and_unknown() {
        assert_eq!(PacketType::from_u8(2), Ok(PacketType::Response));
        assert_eq!(PacketType::from_u8(42), Err(RpcError::UnsupportedPtype(42)));
        assert_eq!(PacketType::Fault.to_u8(), 3);
    }
}

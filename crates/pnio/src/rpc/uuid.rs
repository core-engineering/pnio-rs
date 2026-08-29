//! 128-bit UUID with DREP-aware wire encoding (first three fields byte-order dependent).

use super::Drep;
use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Uuid(pub [u8; 16]);

/// PNIO device interface (the controller calls us on it).
pub const PNIO_DEVICE_INTERFACE: Uuid = Uuid([
    0xde, 0xa0, 0x00, 0x01, 0x6c, 0x97, 0x11, 0xd1, 0x82, 0x71, 0x00, 0xa0, 0x24, 0x42, 0xdf, 0x7d,
]);
/// PNIO controller interface (we call the controller on it for ApplicationReady).
pub const PNIO_CONTROLLER_INTERFACE: Uuid = Uuid([
    0xde, 0xa0, 0x00, 0x02, 0x6c, 0x97, 0x11, 0xd1, 0x82, 0x71, 0x00, 0xa0, 0x24, 0x42, 0xdf, 0x7d,
]);

impl Uuid {
    pub const NIL: Uuid = Uuid([0; 16]);

    /// PNIO object UUID: `dea00000-6c97-11d1-8271-{instance}{device_id}{vendor_id}`.
    pub fn pnio_object(instance: u16, device_id: u16, vendor_id: u16) -> Uuid {
        let mut b = [
            0xde, 0xa0, 0x00, 0x00, 0x6c, 0x97, 0x11, 0xd1, 0x82, 0x71, 0, 0, 0, 0, 0, 0,
        ];
        b[10..12].copy_from_slice(&instance.to_be_bytes());
        b[12..14].copy_from_slice(&device_id.to_be_bytes());
        b[14..16].copy_from_slice(&vendor_id.to_be_bytes());
        Uuid(b)
    }

    pub fn parse_str(s: &str) -> Option<Uuid> {
        let hex: String = s.split('-').collect();
        if hex.len() != 32 || s.split('-').map(str::len).ne([8, 4, 4, 4, 12]) {
            return None;
        }
        let mut b = [0u8; 16];
        for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
            b[i] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
        }
        Some(Uuid(b))
    }

    /// Read 16 bytes in wire form: `time_low`, `time_mid`, `time_hi` follow `drep`.
    pub fn read(buf: &[u8], drep: Drep) -> Option<Uuid> {
        if buf.len() < 16 {
            return None;
        }
        let mut b = [0u8; 16];
        b[0..4].copy_from_slice(&drep.u32(&buf[0..4]).to_be_bytes());
        b[4..6].copy_from_slice(&drep.u16(&buf[4..6]).to_be_bytes());
        b[6..8].copy_from_slice(&drep.u16(&buf[6..8]).to_be_bytes());
        b[8..16].copy_from_slice(&buf[8..16]);
        Some(Uuid(b))
    }

    pub fn write(&self, out: &mut Vec<u8>, drep: Drep) {
        drep.put_u32(
            out,
            u32::from_be_bytes([self.0[0], self.0[1], self.0[2], self.0[3]]),
        );
        drep.put_u16(out, u16::from_be_bytes([self.0[4], self.0[5]]));
        drep.put_u16(out, u16::from_be_bytes([self.0[6], self.0[7]]));
        out.extend_from_slice(&self.0[8..16]);
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = &self.0;
        write!(
            f,
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
        )
    }
}

impl fmt::Debug for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Uuid({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::Drep;

    #[test]
    fn parse_and_display_roundtrip() {
        let u = Uuid::parse_str("dea00001-6c97-11d1-8271-00a02442df7d").unwrap();
        assert_eq!(u.to_string(), "dea00001-6c97-11d1-8271-00a02442df7d");
        assert_eq!(u, PNIO_DEVICE_INTERFACE);
    }

    #[test]
    fn little_endian_wire_form_swaps_first_three_fields() {
        // object UUID as it appears in the CPU Connect request (DREP LE), connect_req.hex[50..66]
        let le = [
            0x00, 0x00, 0xa0, 0xde, 0x97, 0x6c, 0xd1, 0x11, 0x82, 0x71, 0x00, 0x01, 0x00, 0x02,
            0x04, 0x93,
        ];
        let u = Uuid::read(&le, Drep::LITTLE).unwrap();
        assert_eq!(u.to_string(), "dea00000-6c97-11d1-8271-000100020493");
        assert_eq!(u, Uuid::pnio_object(0x0001, 0x0002, 0x0493));
        let mut out = Vec::new();
        u.write(&mut out, Drep::LITTLE);
        assert_eq!(out, le);
        let mut be = Vec::new();
        u.write(&mut be, Drep::BIG);
        assert_eq!(&be[..4], &[0xde, 0xa0, 0x00, 0x00]);
    }

    #[test]
    fn rejects_bad_text() {
        assert!(Uuid::parse_str("not-a-uuid").is_none());
    }
}

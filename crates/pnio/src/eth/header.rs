use thiserror::Error;

pub const ETHERTYPE_PROFINET: u16 = 0x8892;
pub const ETHERTYPE_VLAN: u16 = 0x8100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacAddr(pub [u8; 6]);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EthHeader {
    pub dst: MacAddr,
    pub src: MacAddr,
    pub vlan: Option<u16>,
    pub ethertype: u16,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EthError {
    #[error("frame too short")]
    TooShort,
}

impl EthHeader {
    /// Parses the L2 header; returns (header, payload offset).
    pub fn parse(buf: &[u8]) -> Result<(Self, usize), EthError> {
        if buf.len() < 14 {
            return Err(EthError::TooShort);
        }
        let mut dst = [0u8; 6];
        let mut src = [0u8; 6];
        dst.copy_from_slice(&buf[0..6]);
        src.copy_from_slice(&buf[6..12]);

        let first = u16::from_be_bytes([buf[12], buf[13]]);
        let (vlan, ethertype, off) = if first == ETHERTYPE_VLAN {
            if buf.len() < 18 {
                return Err(EthError::TooShort);
            }
            let tci = u16::from_be_bytes([buf[14], buf[15]]);
            let et = u16::from_be_bytes([buf[16], buf[17]]);
            (Some(tci), et, 18)
        } else {
            (None, first, 14)
        };

        Ok((
            Self {
                dst: MacAddr(dst),
                src: MacAddr(src),
                vlan,
                ethertype,
            },
            off,
        ))
    }

    /// Serializes the header (without payload) into `out`.
    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.dst.0);
        out.extend_from_slice(&self.src.0);
        if let Some(tci) = self.vlan {
            out.extend_from_slice(&ETHERTYPE_VLAN.to_be_bytes());
            out.extend_from_slice(&tci.to_be_bytes());
        }
        out.extend_from_slice(&self.ethertype.to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // dst=01:0e:cf:00:00:00, src=00:11:22:33:44:55, ethertype=0x8892, payload=[0xfe,0xfe]
    const FRAME_NO_VLAN: [u8; 16] = [
        0x01, 0x0e, 0xcf, 0x00, 0x00, 0x00, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x88, 0x92, 0xfe,
        0xfe,
    ];

    // same frame with VLAN tag 0x8100, TCI=0xE000 (prio 7), before the ethertype
    const FRAME_VLAN: [u8; 20] = [
        0x01, 0x0e, 0xcf, 0x00, 0x00, 0x00, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x81, 0x00, 0xe0,
        0x00, 0x88, 0x92, 0xfe, 0xfe,
    ];

    #[test]
    fn parse_without_vlan() {
        let (h, off) = EthHeader::parse(&FRAME_NO_VLAN).unwrap();
        assert_eq!(h.dst, MacAddr([0x01, 0x0e, 0xcf, 0x00, 0x00, 0x00]));
        assert_eq!(h.src, MacAddr([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]));
        assert_eq!(h.vlan, None);
        assert_eq!(h.ethertype, ETHERTYPE_PROFINET);
        assert_eq!(off, 14);
        assert_eq!(&FRAME_NO_VLAN[off..], &[0xfe, 0xfe]);
    }

    #[test]
    fn parse_with_vlan() {
        let (h, off) = EthHeader::parse(&FRAME_VLAN).unwrap();
        assert_eq!(h.vlan, Some(0xe000));
        assert_eq!(h.ethertype, ETHERTYPE_PROFINET);
        assert_eq!(off, 18);
    }

    #[test]
    fn round_trip_no_vlan() {
        let (h, _) = EthHeader::parse(&FRAME_NO_VLAN).unwrap();
        let mut out = Vec::new();
        h.write(&mut out);
        assert_eq!(out, &FRAME_NO_VLAN[..14]);
    }

    #[test]
    fn too_short_is_error() {
        assert!(matches!(
            EthHeader::parse(&[0u8; 8]),
            Err(EthError::TooShort)
        ));
    }
}

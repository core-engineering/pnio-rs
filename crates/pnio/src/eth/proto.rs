//! Which EtherType (and multicast group) an [`AfPacketTransport`] is bound to.

use super::{ETHERTYPE_LLDP, ETHERTYPE_PROFINET, ETHERTYPE_VLAN, LLDP_MULTICAST_MAC};
use crate::dcp::DCP_MULTICAST_MAC;

/// What an `AF_PACKET` socket is bound to and what it accepts at receive time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EthProto {
    /// The EtherType the socket is bound to (`sll_protocol`) and filters on.
    pub ethertype: u16,
    /// A multicast group to join (`PACKET_ADD_MEMBERSHIP`), if any.
    pub multicast: Option<[u8; 6]>,
}

impl EthProto {
    /// PROFINET (`0x8892`) with the DCP multicast group.
    pub const PROFINET: EthProto = EthProto {
        ethertype: ETHERTYPE_PROFINET,
        multicast: Some(DCP_MULTICAST_MAC.0),
    };
    /// LLDP (`0x88CC`) with the nearest-bridge group `01:80:c2:00:00:0e`.
    pub const LLDP: EthProto = EthProto {
        ethertype: ETHERTYPE_LLDP,
        multicast: Some(LLDP_MULTICAST_MAC.0),
    };

    /// True for a frame of this EtherType, untagged or behind one 802.1Q tag.
    ///
    /// In practice, live frames delivered to a protocol-bound `AF_PACKET` socket
    /// always arrive with any 802.1Q tag already stripped by the kernel
    /// (`__netif_receive_skb_core` untags before protocol-keyed dispatch, and
    /// `packet_rcv` does not reinsert it) -- the tag is only observable via
    /// `PACKET_AUXDATA` + `recvmsg`, which the transport does not use. The
    /// VLAN-tagged branch therefore never matches live `AfPacketTransport`
    /// traffic; it exists for the mock/replay path, where captured frames can
    /// still carry the tag, and as a hook for a possible future `ETH_P_ALL`-bound
    /// fallback that would see tagged frames.
    pub fn accepts(&self, buf: &[u8]) -> bool {
        if buf.len() < 14 {
            return false;
        }
        let et = u16::from_be_bytes([buf[12], buf[13]]);
        if et == self.ethertype {
            return true;
        }
        et == ETHERTYPE_VLAN
            && buf.len() >= 18
            && u16::from_be_bytes([buf[16], buf[17]]) == self.ethertype
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(et: u16, tagged: bool) -> Vec<u8> {
        let mut f = vec![0u8; 12];
        if tagged {
            f.extend_from_slice(&[0x81, 0x00, 0xc0, 0x00]);
        }
        f.extend_from_slice(&et.to_be_bytes());
        f.extend_from_slice(&[0u8; 46]);
        f
    }

    #[test]
    fn accepts_matches_ethertype_tagged_or_not() {
        assert!(EthProto::PROFINET.accepts(&frame(0x8892, false)));
        assert!(EthProto::PROFINET.accepts(&frame(0x8892, true)));
        assert!(!EthProto::PROFINET.accepts(&frame(0x88CC, false)));
        assert!(EthProto::LLDP.accepts(&frame(0x88CC, false)));
        assert!(!EthProto::LLDP.accepts(&frame(0x8892, true)));
        assert!(!EthProto::LLDP.accepts(&[0u8; 13]));
    }
}

//! PDEV records: the read-side builders (`PDPortDataReal`, `PDInterfaceDataReal`,
//! `PDRealData`, `PDExpectedData`) and the write-side `PDPortDataCheck` parser.
//! Wire layouts: docs/pdev-golden-frames.md §2.

use super::{CheckRecord, ExpectedPeer, Pdev, IFACE_SUBSLOT, PORT_SUBSLOT};
use crate::cm::{BlockError, BlockHeader, Cursor};

/// Record indices, docs/pdev-golden-frames.md §2.9.
pub mod index {
    /// `PDPortDataReal` for one subslot (read, port).
    pub const PD_PORT_DATA_REAL: u16 = 0x802A;
    /// `PDPortDataCheck` for one subslot (write + read-back, port).
    pub const PD_PORT_DATA_CHECK: u16 = 0x802B;
    /// `PDPortDataAdjust` for one subslot (write + read-back, port).
    pub const PD_PORT_DATA_ADJUST: u16 = 0x802F;
    /// `PDInterfaceDataReal` (read, interface).
    pub const PD_INTERFACE_DATA_REAL: u16 = 0x8080;
    /// `PDInterfaceAdjust` for one subslot (write, interface).
    pub const PD_INTERFACE_ADJUST: u16 = 0x8071;
    /// `PDRealData`: every real-data block of the device, MultipleBlockHeader-wrapped.
    pub const PD_REAL_DATA: u16 = 0xF841;
    /// `PDExpectedData`: every written engineering block, MultipleBlockHeader-wrapped.
    pub const PD_EXPECTED_DATA: u16 = 0xF842;
}

/// BlockTypes, docs/pdev-golden-frames.md §2.2 (all confirmed by the dissector's table).
pub mod ty {
    /// `PDPortDataCheck`.
    pub const PD_PORT_DATA_CHECK: u16 = 0x0200;
    /// `PDPortDataAdjust`.
    pub const PD_PORT_DATA_ADJUST: u16 = 0x0202;
    /// `CheckPeers` (sub-block of `PDPortDataCheck`).
    pub const CHECK_PEERS: u16 = 0x020A;
    /// `Checking MAUType` (sub-block of `PDPortDataCheck`).
    pub const CHECK_MAU_TYPE: u16 = 0x020C;
    /// `PDPortDataReal`.
    pub const PD_PORT_DATA_REAL: u16 = 0x020F;
    /// `PDInterfaceDataReal`.
    pub const PD_INTERFACE_DATA_REAL: u16 = 0x0240;
    /// `PDInterfaceAdjust`.
    pub const PD_INTERFACE_ADJUST: u16 = 0x0250;
    /// `MultipleBlockHeader`.
    pub const MULTIPLE_BLOCK_HEADER: u16 = 0x0400;
}

/// Start a block: 6-byte header (version 1.0) with a placeholder BlockLength.
fn begin(block_type: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(96);
    BlockHeader::write(&mut b, block_type, 0);
    b
}

/// Pad with zeros to a multiple of 4 counted from the block's first header byte.
fn align4(b: &mut Vec<u8>) {
    while b.len() % 4 != 0 {
        b.push(0);
    }
}

/// Patch the BlockLength of a block built with [`begin`]: everything after the
/// type/length fields (so the 2 version bytes count).
fn finish(mut b: Vec<u8>) -> Vec<u8> {
    let len = (b.len() - 4) as u16;
    b[2..4].copy_from_slice(&len.to_be_bytes());
    b
}

/// Write a u8 length then the bytes, cut to their first 255 bytes so the length byte
/// and the text always agree (a neighbour's LLDP Chassis/Port ID can be longer; the cut
/// may split a UTF-8 sequence — it is wire text, read back byte-wise).
fn push_len_prefixed(b: &mut Vec<u8>, s: &[u8]) {
    let s = &s[..s.len().min(u8::MAX as usize)];
    b.push(s.len() as u8);
    b.extend_from_slice(s);
}

/// `PDPortDataReal` (`0x020F`) for our port, §2.3.
pub fn port_data_real(p: &Pdev, slot: u16, subslot: u16) -> Vec<u8> {
    let mut b = begin(ty::PD_PORT_DATA_REAL);
    b.extend_from_slice(&[0, 0]); // padding: header is 6 bytes
    b.extend_from_slice(&slot.to_be_bytes());
    b.extend_from_slice(&subslot.to_be_bytes());
    let own = p.own_port_id().as_bytes();
    b.push(own.len() as u8); // LengthOwnPortID
    b.extend_from_slice(own); // OwnPortID (no padding before NumberOfPeers)
    let peer = p.peer();
    b.push(u8::from(peer.is_some())); // NumberOfPeers
    align4(&mut b);
    if let Some(peer) = peer {
        push_len_prefixed(&mut b, peer.port_id.as_bytes()); // LengthPeerPortID, PeerPortID
        push_len_prefixed(&mut b, peer.chassis_id.as_bytes()); // LengthPeerChassisID, PeerChassisID
        align4(&mut b);
        b.extend_from_slice(&0u32.to_be_bytes()); // LineDelay: format 0, value 0 = unknown
        b.extend_from_slice(&peer.mac); // PeerMACAddress
        align4(&mut b);
    }
    b.extend_from_slice(&0x0010u16.to_be_bytes()); // MAUType 100BASETXFD
    align4(&mut b);
    b.extend_from_slice(&0u32.to_be_bytes()); // DomainBoundary
    b.extend_from_slice(&0u32.to_be_bytes()); // MulticastBoundary
    b.push(0); // LinkState.Port: 0 unknown (what the 1515 puts here is capture-only)
    b.push(if p.link_up() { 1 } else { 2 }); // LinkState.Link: 1 up, 2 down
    align4(&mut b);
    b.extend_from_slice(&1u32.to_be_bytes()); // MediaType: copper
    finish(b)
}

/// `PDInterfaceDataReal` (`0x0240`), §2.4: no slot/subslot fields, no padding before
/// the name length.
pub fn interface_data_real(p: &Pdev) -> Vec<u8> {
    let net = p.net();
    let mut b = begin(ty::PD_INTERFACE_DATA_REAL);
    b.push(net.name.len() as u8); // LengthOwnStationName
    b.extend_from_slice(net.name.as_bytes());
    align4(&mut b);
    b.extend_from_slice(&p.mac()); // MACAddressValue
    align4(&mut b);
    b.extend_from_slice(&net.ip);
    b.extend_from_slice(&net.subnet);
    b.extend_from_slice(&net.gateway);
    finish(b)
}

/// `MultipleBlockHeader` (`0x0400`), §2.8: padding 2, API 0, slot, subslot, then the
/// nested blocks, which its BlockLength covers.
fn multiple_block_header(slot: u16, subslot: u16, nested: &[u8]) -> Vec<u8> {
    let mut b = begin(ty::MULTIPLE_BLOCK_HEADER);
    b.extend_from_slice(&[0, 0]);
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&slot.to_be_bytes());
    b.extend_from_slice(&subslot.to_be_bytes());
    b.extend_from_slice(nested);
    finish(b)
}

/// `PDRealData` (`0xF841`): the interface block, then the port block, each wrapped in a
/// MultipleBlockHeader (wrapping order capture-only, §2.9).
pub fn pd_real_data(p: &Pdev) -> Vec<u8> {
    let mut out = multiple_block_header(0, IFACE_SUBSLOT, &interface_data_real(p));
    out.extend(multiple_block_header(
        0,
        PORT_SUBSLOT,
        &port_data_real(p, 0, PORT_SUBSLOT),
    ));
    out
}

/// `PDExpectedData` (`0xF842`): what TIA wrote, wrapped per subslot (port: Check then
/// Adjust; interface: PDInterfaceAdjust); empty when nothing was written.
pub fn pd_expected_data(p: &Pdev) -> Vec<u8> {
    let mut port = Vec::new();
    if let Some(c) = p.check() {
        port.extend_from_slice(&c.raw);
    }
    if let Some(a) = p.adjust() {
        port.extend_from_slice(a);
    }
    let mut out = Vec::new();
    if !port.is_empty() {
        out.extend(multiple_block_header(0, PORT_SUBSLOT, &port));
    }
    if let Some(a) = p.iface_adjust() {
        out.extend(multiple_block_header(0, IFACE_SUBSLOT, a));
    }
    out
}

/// Parse a `PDPortDataCheck` record (the whole record data, block header included), §2.5:
/// padding 2, slot, subslot, then sub-blocks until the end or a `0x0000` BlockType (§2.1).
/// `CheckPeers` and `Checking MAUType` are read; other sub-blocks are skipped. Truncated
/// or inconsistent input is an error, never a panic.
pub fn parse_check(record: &[u8]) -> Result<CheckRecord, BlockError> {
    let (h, body) = BlockHeader::parse(record)?;
    if h.block_type != ty::PD_PORT_DATA_CHECK {
        return Err(BlockError::UnexpectedType {
            expected: ty::PD_PORT_DATA_CHECK,
            got: h.block_type,
        });
    }
    let mut c = Cursor::new(body);
    let _padding = c.u16()?;
    let _slot = c.u16()?;
    let _subslot = c.u16()?;
    let mut out = CheckRecord {
        peers: Vec::new(),
        mau_type: None,
        // The outer block only: bytes past its BlockLength are not part of the record
        // (they would read back inside PDExpectedData as a further block).
        raw: record[..BlockHeader::LEN + body.len()].to_vec(),
    };
    let mut rest = &body[6..];
    while rest.len() >= BlockHeader::LEN && rest[..2] != [0, 0] {
        let (sh, sb) = BlockHeader::parse(rest)?;
        rest = &rest[BlockHeader::LEN + sb.len()..];
        let mut s = Cursor::new(sb);
        match sh.block_type {
            ty::CHECK_PEERS => {
                for _ in 0..s.u8()? {
                    let n = s.u8()? as usize;
                    let port_id = String::from_utf8_lossy(s.bytes(n)?).into_owned();
                    let n = s.u8()? as usize;
                    let chassis_id = String::from_utf8_lossy(s.bytes(n)?).into_owned();
                    out.peers.push(ExpectedPeer {
                        port_id,
                        chassis_id,
                    });
                }
            }
            ty::CHECK_MAU_TYPE => out.mau_type = Some(s.u16()?),
            _ => {}
        }
    }
    Ok(out)
}

/// Serve a PDEV read: `Some(record)` (possibly empty) for an index/subslot this module
/// owns, `None` otherwise (the caller falls through to I&M, then "invalid index").
/// The written records (`0x802B`, `0x802F` on the port, `0x8071` on the interface) read
/// back byte-identical, or empty when nothing is stored.
pub fn read(p: &Pdev, idx: u16, slot: u16, subslot: u16) -> Option<Vec<u8>> {
    match (idx, slot, subslot) {
        (index::PD_PORT_DATA_REAL, 0, PORT_SUBSLOT) => Some(port_data_real(p, slot, subslot)),
        (index::PD_PORT_DATA_CHECK, 0, PORT_SUBSLOT) => {
            Some(p.check().map(|c| c.raw.clone()).unwrap_or_default())
        }
        (index::PD_PORT_DATA_ADJUST, 0, PORT_SUBSLOT) => {
            Some(p.adjust().map(<[u8]>::to_vec).unwrap_or_default())
        }
        (index::PD_INTERFACE_DATA_REAL, 0, IFACE_SUBSLOT) => Some(interface_data_real(p)),
        (index::PD_INTERFACE_ADJUST, 0, IFACE_SUBSLOT) => {
            Some(p.iface_adjust().map(<[u8]>::to_vec).unwrap_or_default())
        }
        (index::PD_REAL_DATA, _, _) => Some(pd_real_data(p)),
        (index::PD_EXPECTED_DATA, _, _) => Some(pd_expected_data(p)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cm::{BlockError, BlockHeader, ReadReq, WriteReq};
    use crate::dcp::NetParams;
    use crate::pdev::{MockLink, Pdev, IFACE_SUBSLOT, PORT_SUBSLOT};
    use crate::rpc::Uuid;
    use crate::testutil::{synthetic_check_record, synthetic_read_req, synthetic_write_record};
    use std::time::Instant;

    fn pdev() -> Pdev {
        pdev_with_link(true)
    }

    fn pdev_with_link(up: bool) -> Pdev {
        let mut p = Pdev::new(
            [2, 0, 0, 0, 0, 1],
            Box::new(MockLink(up.into())),
            Instant::now(),
        );
        p.set_identity(
            &NetParams {
                name: "pnio-dev".into(),
                ip: [172, 16, 2, 10],
                subnet: [255, 255, 255, 0],
                gateway: [172, 16, 2, 10],
            },
            Instant::now(),
        );
        p
    }

    /// `(subslot, nested BlockTypes)` of a MultipleBlockHeader body: padding 2, API 4,
    /// slot 2, subslot 2, then the nested blocks (docs/pdev-golden-frames.md §2.8).
    fn mbh(body: &[u8]) -> (u16, Vec<u16>) {
        let subslot = u16::from_be_bytes([body[8], body[9]]);
        let nested = BlockHeader::read_all(&body[10..])
            .unwrap()
            .iter()
            .map(|(h, _)| h.block_type)
            .collect();
        (subslot, nested)
    }

    /// A `PDPortDataCheck` record (slot 0, subslot `0x8001`) around the given sub-block bytes.
    fn check_with(subs: &[u8]) -> Vec<u8> {
        let mut body = vec![0, 0, 0, 0, 0x80, 0x01];
        body.extend_from_slice(subs);
        let mut out = Vec::new();
        BlockHeader::write(&mut out, ty::PD_PORT_DATA_CHECK, body.len() as u16);
        out.extend_from_slice(&body);
        out
    }

    /// One sub-block: header (version 1.0) + body.
    fn sub(block_type: u16, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        BlockHeader::write(&mut out, block_type, body.len() as u16);
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn parses_a_check_peers_record() {
        // synthetic — replace by the Task 2 golden (`golden_in("pdev", "connect_write_check")`).
        let check = synthetic_check_record("port-001.plcxb1d0ed", "plcxb1d0ed");
        let w = WriteReq::parse(&synthetic_write_record(
            Uuid([0; 16]),
            0,
            PORT_SUBSLOT,
            index::PD_PORT_DATA_CHECK,
            &check,
        ))
        .unwrap();
        let rec = w
            .records
            .iter()
            .find(|r| r.index == index::PD_PORT_DATA_CHECK)
            .unwrap();
        assert_eq!((rec.slot, rec.subslot), (0, PORT_SUBSLOT));
        let c = parse_check(&rec.data).unwrap();
        assert_eq!(c.peers.len(), 1);
        assert_eq!(c.peers[0].chassis_id, "plcxb1d0ed");
        assert!(c.peers[0].port_id.starts_with("port-001"));
        assert_eq!(c.mau_type, Some(0x0010));
        assert_eq!(c.raw, rec.data);
    }

    #[test]
    fn check_sub_block_list_stops_at_a_zero_block_type() {
        // docs/pdev-golden-frames.md §2.1: a block list ends early at a 0x0000 BlockType.
        let mut rec = synthetic_check_record("port-001.plc", "plc");
        rec.extend_from_slice(&[0, 0, 0, 0]);
        let len = u16::from_be_bytes([rec[2], rec[3]]) + 4;
        rec[2..4].copy_from_slice(&len.to_be_bytes());
        assert_eq!(parse_check(&rec).unwrap().peers.len(), 1);
    }

    #[test]
    fn check_with_garbage_is_an_error() {
        assert!(parse_check(&[0x02, 0x00, 0x00, 0x03, 0x01, 0x00, 0x00]).is_err());
    }

    #[test]
    fn check_skips_unknown_sub_blocks_and_reads_alternative_peers() {
        // CheckLineDelay (§2.5: padding 2, LineDelay u32) before a two-peer CheckPeers.
        let mut subs = sub(0x020B, &[0, 0, 0, 0, 0, 0]);
        subs.extend(sub(
            ty::CHECK_PEERS,
            &[2, 1, b'a', 1, b'b', 1, b'c', 1, b'd'],
        ));
        let c = parse_check(&check_with(&subs)).unwrap();
        let ids: Vec<_> = c
            .peers
            .iter()
            .map(|p| (p.port_id.as_str(), p.chassis_id.as_str()))
            .collect();
        assert_eq!(ids, [("a", "b"), ("c", "d")]);
        assert_eq!(c.mau_type, None);
    }

    #[test]
    fn check_rejects_hostile_sub_blocks_without_panicking() {
        // Wrong outer BlockType.
        let mut rec = synthetic_check_record("p", "c");
        rec[1] = 0x02; // 0x0202 PDPortDataAdjust
        assert!(matches!(
            parse_check(&rec),
            Err(BlockError::UnexpectedType {
                expected: ty::PD_PORT_DATA_CHECK,
                got: 0x0202
            })
        ));
        // NumberOfPeers 255 with no peer data.
        assert!(parse_check(&check_with(&sub(ty::CHECK_PEERS, &[255]))).is_err());
        // LengthPeerPortID beyond the sub-block.
        assert!(parse_check(&check_with(&sub(ty::CHECK_PEERS, &[1, 200, b'a']))).is_err());
        // LengthPeerChassisID missing.
        assert!(parse_check(&check_with(&sub(ty::CHECK_PEERS, &[1, 1, b'a']))).is_err());
        // Empty CheckPeers / CheckMAUType bodies.
        assert!(parse_check(&check_with(&sub(ty::CHECK_PEERS, &[]))).is_err());
        assert!(parse_check(&check_with(&sub(ty::CHECK_MAU_TYPE, &[0]))).is_err());
        // A sub-block whose BlockLength runs past the record.
        let mut bad = sub(ty::CHECK_MAU_TYPE, &[0, 0x10]);
        bad[3] = 0x40;
        assert!(parse_check(&check_with(&bad)).is_err());
        // A sub-block with BlockLength 0 (less than its own version bytes) is skipped.
        let mut zero = vec![0x02, 0x0B, 0x00, 0x00, 0x01, 0x00];
        zero.extend(sub(ty::CHECK_MAU_TYPE, &[0, 0x10]));
        assert_eq!(
            parse_check(&check_with(&zero)).unwrap().mau_type,
            Some(0x0010)
        );
        // Zero-length strings are accepted (they will simply never match a neighbour).
        let c = parse_check(&check_with(&sub(ty::CHECK_PEERS, &[1, 0, 0]))).unwrap();
        assert_eq!(
            c.peers,
            [ExpectedPeer {
                port_id: String::new(),
                chassis_id: String::new()
            }]
        );
        // NumberOfPeers 0: no peer check.
        assert!(parse_check(&check_with(&sub(ty::CHECK_PEERS, &[0])))
            .unwrap()
            .peers
            .is_empty());
    }

    #[test]
    fn check_truncated_anywhere_never_panics() {
        // synthetic — replace by the Task 2 golden (`connect_write_check`'s 0x802B record).
        let rec = synthetic_check_record("port-001.plcxb1d0ed", "plcxb1d0ed");
        for n in 0..rec.len() {
            assert!(parse_check(&rec[..n]).is_err(), "cut at {n}");
        }
        // Same cuts with the outer BlockLength patched to the cut, so the truncation
        // lands inside the sub-blocks instead of failing at the outer header.
        for n in BlockHeader::LEN..rec.len() {
            let mut cut = rec[..n].to_vec();
            cut[2..4].copy_from_slice(&((n - 4) as u16).to_be_bytes());
            let _ = parse_check(&cut);
        }
    }

    #[test]
    fn port_data_real_without_peer_then_with_peer() {
        let mut p = pdev();
        let b = port_data_real(&p, 0, PORT_SUBSLOT);
        let (h, body) = BlockHeader::parse(&b).unwrap();
        assert_eq!(h.block_type, ty::PD_PORT_DATA_REAL);
        assert_eq!(h.block_length as usize + 4, b.len()); // BlockLength counts the version bytes, not type/length
        assert_eq!(b.len() % 4, 0);
        // Padding 2, SlotNumber, SubslotNumber, LengthOwnPortID, OwnPortID, NumberOfPeers = 0 (§2.3)
        assert_eq!(&body[0..2], &[0, 0]);
        assert_eq!(&body[2..6], &[0, 0, 0x80, 0x01]);
        let own = p.own_port_id().as_bytes().to_vec();
        assert_eq!(body[6] as usize, own.len());
        assert_eq!(&body[7..7 + own.len()], &own[..]);
        assert_eq!(body[7 + own.len()], 0);
        let frame = crate::lldp::build_device_pdu("plc", [0xec; 6], "port-001.plc", 20)
            .frame(crate::eth::MacAddr([0xec; 6]));
        p.on_rx(&frame, Instant::now());
        let b2 = port_data_real(&p, 0, PORT_SUBSLOT);
        assert!(b2.len() > b.len());
        assert_eq!(b2.len() % 4, 0);
        assert_eq!(b2[BlockHeader::LEN + 7 + own.len()], 1, "NumberOfPeers");
        let s = String::from_utf8_lossy(&b2);
        assert!(s.contains("port-001.plc") && s.contains("plc"));
        assert!(b2.windows(6).any(|w| w == [0xec; 6]), "peer MAC present");
    }

    #[test]
    fn port_data_real_is_byte_exact() {
        // docs/pdev-golden-frames.md §2.3, offsets from the block start.
        let tail = |link: u8| {
            let mut t = vec![0x00, 0x10, 0, 0]; // MAUType 100BASETXFD, padding 2
            t.extend_from_slice(&[0; 4]); // DomainBoundary
            t.extend_from_slice(&[0; 4]); // MulticastBoundary
            t.extend_from_slice(&[0x00, link, 0, 0]); // LinkState.Port unknown, LinkState.Link, padding 2
            t.extend_from_slice(&[0, 0, 0, 1]); // MediaType copper
            t
        };
        let head = |len: u8, peers: u8| {
            let mut h = vec![
                0x02, 0x0F, 0x00, len, 0x01, 0x00, 0, 0, 0x00, 0x00, 0x80, 0x01, 17,
            ];
            h.extend_from_slice(b"port-001.pnio-dev"); // 13..30
            h.extend_from_slice(&[peers, 0]); // NumberOfPeers at 30, padding 1 -> 32
            h
        };
        let mut want = head(0x30, 0);
        want.extend(tail(1));
        assert_eq!(port_data_real(&pdev(), 0, PORT_SUBSLOT), want);
        let mut want = head(0x30, 0);
        want.extend(tail(2)); // link down
        assert_eq!(
            port_data_real(&pdev_with_link(false), 0, PORT_SUBSLOT),
            want
        );

        let mut p = pdev();
        let frame = crate::lldp::build_device_pdu("plc", [0xec; 6], "port-001.plc", 20)
            .frame(crate::eth::MacAddr([0xec; 6]));
        p.on_rx(&frame, Instant::now());
        let mut want = head(0x50, 1);
        want.push(12);
        want.extend_from_slice(b"port-001.plc"); // 33..45
        want.push(3);
        want.extend_from_slice(b"plc"); // 46..49
        want.extend_from_slice(&[0, 0, 0]); // padding -> 52
        want.extend_from_slice(&[0; 4]); // LineDelay: format 0, value 0
        want.extend_from_slice(&[0xec; 6]); // PeerMACAddress
        want.extend_from_slice(&[0, 0]); // padding -> 64
        want.extend(tail(1));
        assert_eq!(port_data_real(&p, 0, PORT_SUBSLOT), want);
    }

    #[test]
    fn interface_data_real_carries_chassis_mac_and_ip() {
        let p = pdev();
        let b = interface_data_real(&p);
        let (h, body) = BlockHeader::parse(&b).unwrap();
        assert_eq!(h.block_type, ty::PD_INTERFACE_DATA_REAL);
        assert_eq!(b.len() % 4, 0);
        // LengthOwnStationName right after the header, no padding before it (§2.4).
        assert_eq!(body[0] as usize, "pnio-dev".len());
        assert_eq!(&body[1..9], b"pnio-dev");
        assert!(body.windows(6).any(|w| w == [2, 0, 0, 0, 0, 1]));
        assert_eq!(
            &body[body.len() - 12..],
            &[172, 16, 2, 10, 255, 255, 255, 0, 172, 16, 2, 10]
        );
    }

    #[test]
    fn interface_data_real_is_byte_exact() {
        // docs/pdev-golden-frames.md §2.4, offsets from the block start.
        let mut want = vec![0x02, 0x40, 0x00, 0x20, 0x01, 0x00, 8];
        want.extend_from_slice(b"pnio-dev"); // 7..15
        want.push(0); // padding -> 16
        want.extend_from_slice(&[2, 0, 0, 0, 0, 1, 0, 0]); // MACAddressValue, padding 2 -> 24
        want.extend_from_slice(&[172, 16, 2, 10, 255, 255, 255, 0, 172, 16, 2, 10]);
        assert_eq!(interface_data_real(&pdev()), want);
    }

    #[test]
    fn multiple_block_header_is_byte_exact_and_nested() {
        // docs/pdev-golden-frames.md §2.8: header, padding 2, API, slot, subslot, nested
        // blocks inside BlockLength = 2 + 2 + 8 + nested.
        let p = pdev();
        let real = pd_real_data(&p);
        let iface = interface_data_real(&p);
        assert_eq!(
            &real[..16],
            &[
                0x04,
                0x00,
                0x00,
                12 + iface.len() as u8,
                0x01,
                0x00,
                0,
                0,
                0,
                0,
                0,
                0,
                0x00,
                0x00,
                0x80,
                0x00
            ]
        );
        assert_eq!(&real[16..16 + iface.len()], &iface[..]);
        let port = port_data_real(&p, 0, PORT_SUBSLOT);
        let second = &real[16 + iface.len()..];
        assert_eq!(
            &second[..16],
            &[
                0x04,
                0x00,
                0x00,
                12 + port.len() as u8,
                0x01,
                0x00,
                0,
                0,
                0,
                0,
                0,
                0,
                0x00,
                0x00,
                0x80,
                0x01
            ]
        );
        assert_eq!(&second[16..], &port[..]);
    }

    #[test]
    fn aggregates_and_dispatch() {
        let mut p = pdev();
        assert!(pd_expected_data(&p).is_empty());
        // MultipleBlockHeader is nested: its BlockLength covers the blocks it wraps (§2.8),
        // so the top level holds two MBHs, each wrapping one data block.
        let real = pd_real_data(&p);
        let top = BlockHeader::read_all(&real).unwrap();
        assert_eq!(top.len(), 2);
        assert!(top
            .iter()
            .all(|(h, _)| h.block_type == ty::MULTIPLE_BLOCK_HEADER));
        assert_eq!(
            mbh(top[0].1),
            (IFACE_SUBSLOT, vec![ty::PD_INTERFACE_DATA_REAL])
        );
        assert_eq!(mbh(top[1].1), (PORT_SUBSLOT, vec![ty::PD_PORT_DATA_REAL]));
        // synthetic — replace by the Task 2 golden (`connect_write_check`'s 0x802B record).
        p.set_check(
            Some(
                parse_check(&synthetic_check_record("port-001.plcxb1d0ed", "plcxb1d0ed")).unwrap(),
            ),
            Instant::now(),
        );
        let exp = pd_expected_data(&p);
        let top = BlockHeader::read_all(&exp).unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].0.block_type, ty::MULTIPLE_BLOCK_HEADER);
        assert_eq!(mbh(top[0].1), (PORT_SUBSLOT, vec![ty::PD_PORT_DATA_CHECK]));
        assert!(read(&p, 0x802A, 0, PORT_SUBSLOT).is_some());
        assert!(read(&p, 0x802A, 0, IFACE_SUBSLOT).is_none());
        assert!(read(&p, 0x8080, 0, IFACE_SUBSLOT).is_some());
        assert!(read(&p, 0x802B, 0, PORT_SUBSLOT).is_some());
        assert!(read(&p, 0x802F, 0, PORT_SUBSLOT).unwrap().is_empty());
        assert!(read(&p, 0xF841, 0, 0).is_some());
        assert!(read(&p, 0xAFF0, 0, 1).is_none());
    }

    #[test]
    fn expected_data_wraps_check_and_adjust_per_subslot() {
        let mut p = pdev();
        // PDPortDataAdjust / PDInterfaceAdjust stored raw (§2.6, §2.7: the 1515's
        // MultipleInterfaceMode.NameOfDevice = 1 block from the Connect golden).
        let adjust = vec![
            0x02, 0x02, 0x00, 0x08, 0x01, 0x00, 0, 0, 0x00, 0x00, 0x80, 0x01,
        ];
        let iface_adjust = vec![0x02, 0x50, 0x00, 0x08, 0x01, 0x00, 0, 0, 0, 0, 0, 1];
        p.set_adjust(Some(adjust.clone()));
        p.set_iface_adjust(Some(iface_adjust.clone()));
        let exp = pd_expected_data(&p);
        let top = BlockHeader::read_all(&exp).unwrap();
        assert_eq!(mbh(top[0].1), (PORT_SUBSLOT, vec![ty::PD_PORT_DATA_ADJUST]));
        assert_eq!(
            mbh(top[1].1),
            (IFACE_SUBSLOT, vec![ty::PD_INTERFACE_ADJUST])
        );
        assert_eq!(&top[1].1[10..], &iface_adjust[..]);
        // Check + Adjust share the port's MultipleBlockHeader, Check first; read-back is
        // byte-identical.
        let check = synthetic_check_record("port-001.plc", "plc");
        p.set_check(Some(parse_check(&check).unwrap()), Instant::now());
        let exp = pd_expected_data(&p);
        let top = BlockHeader::read_all(&exp).unwrap();
        assert_eq!(top.len(), 2);
        assert_eq!(
            mbh(top[0].1),
            (
                PORT_SUBSLOT,
                vec![ty::PD_PORT_DATA_CHECK, ty::PD_PORT_DATA_ADJUST]
            )
        );
        assert_eq!(
            read(&p, index::PD_PORT_DATA_CHECK, 0, PORT_SUBSLOT),
            Some(check)
        );
        assert_eq!(
            read(&p, index::PD_PORT_DATA_ADJUST, 0, PORT_SUBSLOT),
            Some(adjust)
        );
        assert_eq!(
            read(&p, index::PD_EXPECTED_DATA, 0, IFACE_SUBSLOT),
            Some(exp)
        );
    }

    #[test]
    fn port_data_real_clamps_peer_strings_to_255_bytes() {
        // A length byte cannot say more than 255: the string is cut to match it, so the
        // block stays self-consistent.
        let mut p = pdev();
        let long = "c".repeat(300);
        let frame = crate::lldp::build_device_pdu(&long, [0xec; 6], "port-001.plc", 20)
            .frame(crate::eth::MacAddr([0xec; 6]));
        p.on_rx(&frame, Instant::now());
        assert_eq!(
            p.peer().unwrap().chassis_id.len(),
            300,
            "the neighbour really sent 300 bytes"
        );
        let b = port_data_real(&p, 0, PORT_SUBSLOT);
        let chassis_len_at = 32 + 1 + "port-001.plc".len(); // peers start at 32 (§2.3)
        assert_eq!(b[chassis_len_at], 255);
        assert_eq!(
            &b[chassis_len_at + 1..chassis_len_at + 256],
            &long.as_bytes()[..255]
        );
        // Padding to 4, then LineDelay and the peer MAC right where the clamped length says.
        let after = (chassis_len_at + 256 + 3) & !3;
        assert_eq!(&b[after..after + 4], &[0; 4]);
        assert_eq!(&b[after + 4..after + 10], &[0xec; 6]);
        let (h, _) = BlockHeader::parse(&b).unwrap();
        assert_eq!(h.block_length as usize + 4, b.len());
    }

    #[test]
    fn check_raw_is_the_outer_block_only() {
        // Bytes past the outer BlockLength are not part of the record we read back.
        let rec = synthetic_check_record("port-001.plc", "plc");
        let block_length = u16::from_be_bytes([rec[2], rec[3]]) as usize;
        let mut with_garbage = rec.clone();
        with_garbage.extend_from_slice(&[0xde, 0xad, 0xbe]);
        let c = parse_check(&with_garbage).unwrap();
        assert_eq!(c.raw.len(), 4 + block_length);
        assert_eq!(c.raw, rec);
    }

    #[test]
    fn interface_adjust_reads_back() {
        let mut p = pdev();
        assert_eq!(
            read(&p, index::PD_INTERFACE_ADJUST, 0, IFACE_SUBSLOT),
            Some(Vec::new())
        );
        // The 1515's MultipleInterfaceMode.NameOfDevice = 1 block (§2.7).
        let iface_adjust = vec![0x02, 0x50, 0x00, 0x08, 0x01, 0x00, 0, 0, 0, 0, 0, 1];
        p.set_iface_adjust(Some(iface_adjust.clone()));
        assert_eq!(
            read(&p, index::PD_INTERFACE_ADJUST, 0, IFACE_SUBSLOT),
            Some(iface_adjust)
        );
        assert_eq!(read(&p, index::PD_INTERFACE_ADJUST, 0, PORT_SUBSLOT), None);
    }

    #[test]
    fn read_requests_target_the_indices_we_serve() {
        // synthetic — replace by the Task 2 goldens (`golden_in("pdev", "read_802a_req" |
        // "read_8080_req" | "read_f841_req" | "read_f842_req")`): indices per
        // docs/pdev-golden-frames.md §2.9; the (slot, subslot) TIA uses is capture-only.
        let p = pdev();
        for (idx, subslot) in [
            (0x802Au16, PORT_SUBSLOT),
            (0x8080, IFACE_SUBSLOT),
            (0xF841, IFACE_SUBSLOT),
            (0xF842, IFACE_SUBSLOT),
        ] {
            let r = ReadReq::parse(&synthetic_read_req(Uuid([0; 16]), 0, subslot, idx)).unwrap();
            assert_eq!((r.index, r.slot, r.subslot), (idx, 0, subslot));
            assert!(read(&p, r.index, r.slot, r.subslot).is_some(), "{idx:#06x}");
        }
    }
}

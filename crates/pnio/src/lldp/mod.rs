//! LLDP (IEEE 802.1AB) codec with the PROFINET and IEEE 802.3 organisationally
//! specific TLVs a PROFINET device sends and reads. Pure: no I/O, no clock.

use crate::eth::{EthHeader, MacAddr, ETHERTYPE_LLDP, LLDP_MULTICAST_MAC};
use std::time::{Duration, Instant};
use thiserror::Error;

/// TTL (seconds) the device announces: 4 × [`TX_PERIOD`].
pub const DEFAULT_TTL: u16 = 20;
/// Transmit period of the device's LLDPDU.
pub const TX_PERIOD: Duration = Duration::from_secs(5);
/// PROFIBUS Nutzerorganisation OUI of the PROFINET TLVs (docs/pdev-golden-frames.md §3.2).
pub const OUI_PROFINET: [u8; 3] = [0x00, 0x0e, 0xcf];
/// IEEE 802.3 OUI of the MAC/PHY TLV (docs/pdev-golden-frames.md §3.3).
pub const OUI_IEEE8023: [u8; 3] = [0x00, 0x12, 0x0f];

/// A Chassis ID or Port ID TLV: its subtype and the raw id bytes after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlvId {
    /// The id subtype (4 = MAC address for a Chassis ID, 7 = locally assigned for both).
    pub subtype: u8,
    /// The id itself, without the subtype byte.
    pub value: Vec<u8>,
}

impl TlvId {
    /// A "locally assigned" (subtype 7) id holding `s`.
    pub fn local(s: &str) -> Self {
        Self {
            subtype: 7,
            value: s.as_bytes().to_vec(),
        }
    }

    /// A "MAC address" (Chassis ID subtype 4) id.
    pub fn mac(m: [u8; 6]) -> Self {
        Self {
            subtype: 4,
            value: m.to_vec(),
        }
    }

    /// A display form: subtype 7 as (lossy) UTF-8, subtype 4 as `aa:bb:cc:dd:ee:ff`, any
    /// other subtype as text when it is printable UTF-8 (e.g. Port ID subtype 5
    /// "interface name"), else as colon-separated hex.
    pub fn as_text(&self) -> String {
        if self.subtype == 7 {
            return String::from_utf8_lossy(&self.value).into_owned();
        }
        if self.subtype != 4 {
            if let Ok(s) = std::str::from_utf8(&self.value) {
                if !s.chars().any(char::is_control) {
                    return s.to_string();
                }
            }
        }
        self.value
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    }
}

/// The IEEE 802.3 MAC/PHY Configuration/Status TLV (OUI `00-12-0F`, subtype 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacPhy {
    /// Auto-negotiation support/status: bit `0x01` supported, bit `0x02` enabled.
    pub autoneg: u8,
    /// PMD auto-negotiation advertised capability (`0x6C00` = 10/100 HD+FD).
    pub pmd: u16,
    /// Operational MAU type (`0x0010` = 100BASE-TX full duplex).
    pub mau_type: u16,
}

/// The TLVs of one LLDPDU that a PROFINET device sends or reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lldpdu {
    /// Chassis ID TLV (type 1).
    pub chassis_id: TlvId,
    /// Port ID TLV (type 2).
    pub port_id: TlvId,
    /// Time To Live in seconds (type 3); 0 announces a shutdown.
    pub ttl: u16,
    /// PROFINET "Measured Delay Values" (subtype 1): RxDelayLocal, RxDelayRemote,
    /// TxDelayLocal, TxDelayRemote, CableDelayLocal, in ns (0 = unknown).
    pub pnio_delay: Option<[u32; 5]>,
    /// PROFINET "Port Status" (subtype 2): (RTClass2 PortStatus, RTClass3 PortStatus).
    pub pnio_port_status: Option<(u16, u16)>,
    /// PROFINET "Chassis MAC" (subtype 5): the sender's interface MAC.
    pub pnio_chassis_mac: Option<[u8; 6]>,
    /// IEEE 802.3 MAC/PHY Configuration/Status.
    pub mac_phy: Option<MacPhy>,
    /// TLVs that were skipped while parsing: other types, other organisationally specific
    /// subtypes, and known TLVs too short (or of the wrong length) to decode.
    pub unknown_tlvs: usize,
}

/// Why an LLDPDU was not accepted.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LldpError {
    /// A TLV's declared length runs past the end of the frame, or the frame is shorter
    /// than an Ethernet header.
    #[error("truncated TLV")]
    Truncated,
    /// A mandatory TLV (chassis id, port id, ttl) is absent.
    #[error("missing mandatory TLV: {0}")]
    MissingMandatory(&'static str),
    /// The frame's EtherType is not LLDP (`0x88CC`).
    #[error("not an LLDP frame (EtherType {0:#06x})")]
    NotLldp(u16),
}

fn tlv(out: &mut Vec<u8>, ty: u8, body: &[u8]) {
    let hdr = ((ty as u16) << 9) | (body.len() as u16 & 0x1ff);
    out.extend_from_slice(&hdr.to_be_bytes());
    out.extend_from_slice(body);
}

fn org(out: &mut Vec<u8>, oui: [u8; 3], subtype: u8, data: &[u8]) {
    let mut body = Vec::with_capacity(4 + data.len());
    body.extend_from_slice(&oui);
    body.push(subtype);
    body.extend_from_slice(data);
    tlv(out, 127, &body);
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

impl Lldpdu {
    /// The TLVs (no Ethernet header), End TLV included, in the order of spec §4.2:
    /// Chassis ID, Port ID, TTL, PNIO delay, PNIO port status, PNIO chassis MAC,
    /// IEEE 802.3 MAC/PHY, End. Absent optional TLVs are left out.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(128);
        let id = |t: &TlvId| {
            let mut b = vec![t.subtype];
            b.extend_from_slice(&t.value);
            b
        };
        tlv(&mut out, 1, &id(&self.chassis_id));
        tlv(&mut out, 2, &id(&self.port_id));
        tlv(&mut out, 3, &self.ttl.to_be_bytes());
        if let Some(d) = &self.pnio_delay {
            let mut b = Vec::with_capacity(20);
            for v in d {
                b.extend_from_slice(&v.to_be_bytes());
            }
            org(&mut out, OUI_PROFINET, 1, &b);
        }
        if let Some((c2, c3)) = self.pnio_port_status {
            let mut b = c2.to_be_bytes().to_vec();
            b.extend_from_slice(&c3.to_be_bytes());
            org(&mut out, OUI_PROFINET, 2, &b);
        }
        if let Some(m) = self.pnio_chassis_mac {
            org(&mut out, OUI_PROFINET, 5, &m);
        }
        if let Some(p) = &self.mac_phy {
            let mut b = vec![p.autoneg];
            b.extend_from_slice(&p.pmd.to_be_bytes());
            b.extend_from_slice(&p.mau_type.to_be_bytes());
            org(&mut out, OUI_IEEE8023, 1, &b);
        }
        tlv(&mut out, 0, &[]);
        out
    }

    /// Parse the TLVs of an LLDPDU (payload after the Ethernet header). Lenient: unknown
    /// or undecodable TLVs are counted in [`Lldpdu::unknown_tlvs`], not rejected; a missing
    /// End TLV and anything after End (Ethernet padding) are accepted; a repeated
    /// mandatory TLV overrides the earlier one.
    pub fn parse(payload: &[u8]) -> Result<Lldpdu, LldpError> {
        let (mut chassis, mut port, mut ttl) = (None, None, None);
        let mut pdu = Lldpdu {
            chassis_id: TlvId {
                subtype: 0,
                value: vec![],
            },
            port_id: TlvId {
                subtype: 0,
                value: vec![],
            },
            ttl: 0,
            pnio_delay: None,
            pnio_port_status: None,
            pnio_chassis_mac: None,
            mac_phy: None,
            unknown_tlvs: 0,
        };
        let mut p = 0usize;
        while p + 2 <= payload.len() {
            let hdr = be16(&payload[p..]);
            let (ty, len) = ((hdr >> 9) as u8, (hdr & 0x1ff) as usize);
            let body = payload
                .get(p + 2..p + 2 + len)
                .ok_or(LldpError::Truncated)?;
            p += 2 + len;
            match ty {
                0 => break,
                1 | 2 if !body.is_empty() => {
                    let id = TlvId {
                        subtype: body[0],
                        value: body[1..].to_vec(),
                    };
                    if ty == 1 {
                        chassis = Some(id)
                    } else {
                        port = Some(id)
                    }
                }
                3 if body.len() >= 2 => ttl = Some(be16(body)),
                127 if body.len() >= 4 => {
                    let (oui, sub, data) = ([body[0], body[1], body[2]], body[3], &body[4..]);
                    match (oui, sub, data.len()) {
                        (OUI_PROFINET, 1, 20) => {
                            let mut d = [0u32; 5];
                            for (v, c) in d.iter_mut().zip(data.chunks_exact(4)) {
                                *v = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
                            }
                            pdu.pnio_delay = Some(d);
                        }
                        (OUI_PROFINET, 2, 4) => {
                            pdu.pnio_port_status = Some((be16(data), be16(&data[2..])))
                        }
                        (OUI_PROFINET, 5, 6) => {
                            let mut m = [0u8; 6];
                            m.copy_from_slice(data);
                            pdu.pnio_chassis_mac = Some(m);
                        }
                        (OUI_IEEE8023, 1, n) if n >= 5 => {
                            pdu.mac_phy = Some(MacPhy {
                                autoneg: data[0],
                                pmd: be16(&data[1..]),
                                mau_type: be16(&data[3..]),
                            })
                        }
                        _ => pdu.unknown_tlvs += 1,
                    }
                }
                _ => pdu.unknown_tlvs += 1,
            }
        }
        pdu.chassis_id = chassis.ok_or(LldpError::MissingMandatory("chassis id"))?;
        pdu.port_id = port.ok_or(LldpError::MissingMandatory("port id"))?;
        pdu.ttl = ttl.ok_or(LldpError::MissingMandatory("ttl"))?;
        Ok(pdu)
    }

    /// The full Ethernet frame from `src` (to [`LLDP_MULTICAST_MAC`], EtherType
    /// [`ETHERTYPE_LLDP`]), zero-padded to the 60-byte minimum.
    pub fn frame(&self, src: MacAddr) -> Vec<u8> {
        let mut f = Vec::with_capacity(128);
        EthHeader {
            dst: LLDP_MULTICAST_MAC,
            src,
            vlan: None,
            ethertype: ETHERTYPE_LLDP,
        }
        .write(&mut f);
        f.extend_from_slice(&self.encode());
        if f.len() < 60 {
            f.resize(60, 0);
        }
        f
    }
}

/// The device's own PDU: chassis id = NameOfStation (subtype 7), or the MAC (subtype 4)
/// while the name is blank; Port ID `port_id` (subtype 7); PROFINET delay (all 0 =
/// unknown), port status (OFF/OFF) and chassis MAC; IEEE 802.3 MAC/PHY.
///
/// With `ttl == 0` it is the shutdown PDU, which carries only Chassis ID, Port ID, TTL
/// and End (docs/pdev-golden-frames.md §3.1: any other TLV is flagged by the dissector).
pub fn build_device_pdu(name: &str, mac: [u8; 6], port_id: &str, ttl: u16) -> Lldpdu {
    let chassis_id = if name.is_empty() {
        TlvId::mac(mac)
    } else {
        TlvId::local(name)
    };
    let mut pdu = Lldpdu {
        chassis_id,
        port_id: TlvId::local(port_id),
        ttl,
        pnio_delay: None,
        pnio_port_status: None,
        pnio_chassis_mac: None,
        mac_phy: None,
        unknown_tlvs: 0,
    };
    if ttl != 0 {
        pdu.pnio_delay = Some([0; 5]);
        pdu.pnio_port_status = Some((0, 0));
        pdu.pnio_chassis_mac = Some(mac);
        // autoneg supported + enabled; PMD 10/100 HD+FD and MAU 100BASE-TX FD, provisional
        // values pinned in docs/pdev-golden-frames.md §3.3.
        pdu.mac_phy = Some(MacPhy {
            autoneg: 0x03,
            pmd: 0x6c00,
            mau_type: 0x0010,
        });
    }
    pdu
}

/// The device's LLDP Port ID: `port-001.<name>` (docs/pdev-golden-frames.md §2.7/§3.4:
/// the 1515 sets MultipleInterfaceMode.NameOfDevice = 1), bare `port-001` while unnamed.
pub fn port_id_string(name: &str) -> String {
    // Form provisional (§3.4, "capture only" for the port literal): Task 2 confirms.
    if name.is_empty() {
        "port-001".to_string()
    } else {
        format!("port-001.{name}")
    }
}

/// A neighbour learnt from its LLDPDU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// The neighbour's Chassis ID, in [`TlvId::as_text`] form.
    pub chassis_id: String,
    /// The neighbour's Port ID, in [`TlvId::as_text`] form.
    pub port_id: String,
    /// The neighbour's MAC: its PROFINET Chassis MAC TLV, else the frame's source MAC.
    pub mac: [u8; 6],
    /// The neighbour's operational MAU type, when it sent the IEEE 802.3 MAC/PHY TLV.
    pub mau_type: Option<u16>,
    /// When the LLDPDU was received.
    pub seen_at: Instant,
    /// How long the information stays valid after `seen_at` (the PDU's TTL).
    pub ttl: Duration,
}

impl Peer {
    /// The neighbour described by `pdu`, received from `src` at `now`.
    #[doc(hidden)]
    pub fn from_pdu(pdu: &Lldpdu, src: [u8; 6], now: Instant) -> Peer {
        Peer {
            chassis_id: pdu.chassis_id.as_text(),
            port_id: pdu.port_id.as_text(),
            mac: pdu.pnio_chassis_mac.unwrap_or(src),
            mau_type: pdu.mac_phy.as_ref().map(|m| m.mau_type),
            seen_at: now,
            ttl: Duration::from_secs(pdu.ttl as u64),
        }
    }

    /// Whether the information has aged out at `now` (a TTL-0 shutdown is expired at once).
    pub fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.seen_at) >= self.ttl
    }
}

/// Parse a whole Ethernet frame (VLAN tag allowed): the source MAC and the LLDPDU.
pub fn parse_frame(frame: &[u8]) -> Result<([u8; 6], Lldpdu), LldpError> {
    let (eth, off) = EthHeader::parse(frame).map_err(|_| LldpError::Truncated)?;
    if eth.ethertype != ETHERTYPE_LLDP {
        return Err(LldpError::NotLldp(eth.ethertype));
    }
    Ok((eth.src.0, Lldpdu::parse(&frame[off..])?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 1515's X1 MAC (the controller MAC of the DCP and Connect goldens).
    const CPU_MAC: [u8; 6] = [0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f];

    // synthetic — replace by the Task 2 golden (`golden_in("lldp", "cpu_x1")`).
    // Hand-assembled (not through `encode`) from docs/pdev-golden-frames.md §3: Chassis ID
    // and Port ID subtype 7 "Locally assigned" (§3.1, §3.4: `<port>.<NameOfStation>`),
    // TTL 20, PROFINET OUI 00-0E-CF subtype 2 Port Status (RTClass2 0, RTClass3 0) and
    // subtype 5 Chassis MAC (§3.2), IEEE 802.3 OUI 00-12-0F subtype 1 MAC/PHY: autoneg
    // 0x03, PMD 0x6C00, MAU 0x0010 (§3.3), End.
    fn cpu_x1_frame() -> Vec<u8> {
        fn tlv(f: &mut Vec<u8>, ty: u16, body: &[u8]) {
            f.extend_from_slice(&((ty << 9) | body.len() as u16).to_be_bytes());
            f.extend_from_slice(body);
        }
        let mut f = vec![0x01, 0x80, 0xc2, 0x00, 0x00, 0x0e];
        f.extend_from_slice(&CPU_MAC);
        f.extend_from_slice(&[0x88, 0xcc]);
        tlv(&mut f, 1, &[&[7u8][..], b"plcxb1d0ed"].concat());
        tlv(&mut f, 2, &[&[7u8][..], b"port-001.plcxb1d0ed"].concat());
        tlv(&mut f, 3, &20u16.to_be_bytes());
        tlv(&mut f, 127, &[0x00, 0x0e, 0xcf, 2, 0x00, 0x00, 0x00, 0x00]);
        tlv(
            &mut f,
            127,
            &[&[0x00, 0x0e, 0xcf, 5][..], &CPU_MAC[..]].concat(),
        );
        tlv(
            &mut f,
            127,
            &[0x00, 0x12, 0x0f, 1, 0x03, 0x6c, 0x00, 0x00, 0x10],
        );
        tlv(&mut f, 0, &[]);
        f
    }

    #[test]
    fn parses_the_cpu_lldpdu() {
        let (src, pdu) = parse_frame(&cpu_x1_frame()).unwrap();
        assert_eq!(src, CPU_MAC);
        assert_eq!(pdu.chassis_id.subtype, 7);
        assert_eq!(pdu.chassis_id.as_text(), "plcxb1d0ed");
        assert!(pdu.port_id.as_text().starts_with("port-001"));
        assert_eq!(pdu.ttl, 20);
        assert_eq!(pdu.pnio_port_status, Some((0, 0)));
        assert_eq!(pdu.pnio_chassis_mac, Some(CPU_MAC));
        assert_eq!(
            pdu.mac_phy,
            Some(MacPhy {
                autoneg: 0x03,
                pmd: 0x6c00,
                mau_type: 0x0010
            })
        );
        assert_eq!(pdu.unknown_tlvs, 0);
    }

    #[test]
    fn device_pdu_round_trips_and_has_the_pinned_tlv_order() {
        let pdu = build_device_pdu(
            "pnio-dev",
            [2, 0, 0, 0, 0, 1],
            &port_id_string("pnio-dev"),
            DEFAULT_TTL,
        );
        let bytes = pdu.encode();
        assert_eq!(&bytes[..2], &[0x02, 0x09]); // Chassis ID TLV: type 1, len 9 (subtype 1 byte + "pnio-dev" 8 bytes)
        assert_eq!(bytes[2], 7);
        assert_eq!(Lldpdu::parse(&bytes).unwrap(), pdu);
        assert_eq!(&bytes[bytes.len() - 2..], &[0, 0]); // End TLV
        let frame = pdu.frame(MacAddr([2, 0, 0, 0, 0, 1]));
        assert_eq!(&frame[..6], &[0x01, 0x80, 0xc2, 0x00, 0x00, 0x0e]);
        assert_eq!(&frame[12..14], &[0x88, 0xcc]);
        assert!(frame.len() >= 60);
    }

    #[test]
    fn blank_name_uses_the_mac_as_chassis_id() {
        let pdu = build_device_pdu("", [2, 0, 0, 0, 0, 1], &port_id_string(""), DEFAULT_TTL);
        assert_eq!(pdu.chassis_id, TlvId::mac([2, 0, 0, 0, 0, 1]));
        assert_eq!(pdu.chassis_id.as_text(), "02:00:00:00:00:01");
    }

    #[test]
    fn foreign_switch_pdu_with_unknown_tlvs_parses_into_a_peer() {
        // Chassis MAC, Port local "Gi1/0/3", TTL 120, System Name (type 5), End.
        let mut b = vec![0x02, 0x07, 0x04, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        b.extend_from_slice(&[0x04, 0x08, 0x07]);
        b.extend_from_slice(b"Gi1/0/3");
        b.extend_from_slice(&[0x06, 0x02, 0x00, 0x78]);
        b.extend_from_slice(&[0x0a, 0x03]);
        b.extend_from_slice(b"sw1");
        b.extend_from_slice(&[0, 0]);
        let pdu = Lldpdu::parse(&b).unwrap();
        assert_eq!(pdu.unknown_tlvs, 1);
        assert_eq!(pdu.ttl, 120);
        let now = Instant::now();
        let peer = Peer::from_pdu(&pdu, [1; 6], now);
        assert_eq!(peer.chassis_id, "aa:bb:cc:dd:ee:ff");
        assert_eq!(peer.port_id, "Gi1/0/3");
        assert_eq!(peer.mau_type, None);
        assert!(!peer.expired(now + Duration::from_secs(119)));
        assert!(peer.expired(now + Duration::from_secs(121)));
    }

    #[test]
    fn truncated_and_mandatory_missing_are_errors() {
        assert_eq!(
            Lldpdu::parse(&[0x02, 0x09, 0x07, b'a']),
            Err(LldpError::Truncated)
        );
        assert_eq!(
            Lldpdu::parse(&[0x06, 0x02, 0x00, 0x14, 0, 0]),
            Err(LldpError::MissingMandatory("chassis id"))
        );
    }

    #[test]
    fn ttl_zero_is_a_shutdown() {
        let mut pdu = build_device_pdu("pnio-dev", [2; 6], "port-001", 0);
        pdu.ttl = 0;
        let peer = Peer::from_pdu(&pdu, [2; 6], Instant::now());
        assert!(peer.expired(Instant::now()));
    }

    // ---- beyond the brief: Review Focus #3, §3.1 shutdown rule, hostile input ----

    /// A realistic non-PROFINET managed switch: Chassis ID subtype 4 (MAC), Port ID
    /// subtype 5 (interface name), TTL 120, Port Description (4), System Name (5),
    /// System Description (6), System Capabilities (7), Management Address (8), End.
    /// No organisationally specific TLV at all.
    fn foreign_switch_payload() -> Vec<u8> {
        fn tlv(f: &mut Vec<u8>, ty: u16, body: &[u8]) {
            f.extend_from_slice(&((ty << 9) | body.len() as u16).to_be_bytes());
            f.extend_from_slice(body);
        }
        let mut f = Vec::new();
        tlv(&mut f, 1, &[4, 0x00, 0x1b, 0x1b, 0x12, 0x34, 0x56]);
        tlv(&mut f, 2, &[&[5u8][..], b"GigabitEthernet1/0/3"].concat());
        tlv(&mut f, 3, &120u16.to_be_bytes());
        tlv(&mut f, 4, b"uplink to cabinet 2");
        tlv(&mut f, 5, b"sw-core-01");
        tlv(&mut f, 6, b"Managed switch, firmware 4.2.1");
        tlv(&mut f, 7, &[0x00, 0x14, 0x00, 0x04]);
        tlv(&mut f, 8, &[5, 1, 192, 168, 1, 2, 2, 0, 0, 0, 1, 0]);
        tlv(&mut f, 0, &[]);
        f
    }

    #[test]
    fn non_profinet_switch_is_a_peer_with_a_mac_chassis_id() {
        let mut frame = vec![0x01, 0x80, 0xc2, 0x00, 0x00, 0x0e];
        frame.extend_from_slice(&[0x00, 0x1b, 0x1b, 0x12, 0x34, 0x59]);
        frame.extend_from_slice(&[0x88, 0xcc]);
        frame.extend_from_slice(&foreign_switch_payload());
        let (src, pdu) = parse_frame(&frame).unwrap();
        assert_eq!(
            pdu.chassis_id,
            TlvId::mac([0x00, 0x1b, 0x1b, 0x12, 0x34, 0x56])
        );
        assert_eq!(pdu.unknown_tlvs, 5);
        assert_eq!(pdu.pnio_chassis_mac, None);
        assert_eq!(pdu.mac_phy, None);
        let now = Instant::now();
        let peer = Peer::from_pdu(&pdu, src, now);
        assert_eq!(peer.chassis_id, "00:1b:1b:12:34:56");
        assert_eq!(peer.port_id, "GigabitEthernet1/0/3");
        assert_eq!(peer.mac, [0x00, 0x1b, 0x1b, 0x12, 0x34, 0x59]); // no PNIO chassis MAC → frame source
        assert_eq!(peer.mau_type, None);
        assert_eq!(peer.ttl, Duration::from_secs(120));
    }

    #[test]
    fn device_shutdown_pdu_carries_only_the_mandatory_tlvs() {
        // docs/pdev-golden-frames.md §3.1: a TTL-0 PDU holds Chassis ID, Port ID, TTL, End.
        let pdu = build_device_pdu("pnio-dev", [2; 6], &port_id_string("pnio-dev"), 0);
        let mut expected = vec![0x02, 0x09, 7];
        expected.extend_from_slice(b"pnio-dev");
        expected.extend_from_slice(&[0x04, 0x12, 7]);
        expected.extend_from_slice(b"port-001.pnio-dev");
        expected.extend_from_slice(&[0x06, 0x02, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(pdu.encode(), expected);
    }

    #[test]
    fn port_id_string_is_the_pinned_format() {
        assert_eq!(port_id_string(""), "port-001");
        assert_eq!(port_id_string("pnio-dev"), "port-001.pnio-dev");
    }

    #[test]
    fn parse_frame_rejects_other_ethertypes_and_short_frames() {
        let mut frame = cpu_x1_frame();
        frame[12..14].copy_from_slice(&[0x88, 0x92]);
        assert_eq!(parse_frame(&frame), Err(LldpError::NotLldp(0x8892)));
        assert_eq!(
            parse_frame(&cpu_x1_frame()[..13]),
            Err(LldpError::Truncated)
        );
    }

    #[test]
    fn parse_frame_ignores_ethernet_padding() {
        let pdu = build_device_pdu("a", [2; 6], "port-001", 0); // shortest PDU: padded to 60
        let frame = pdu.frame(MacAddr([2; 6]));
        assert_eq!(frame.len(), 60);
        assert_eq!(parse_frame(&frame).unwrap(), ([2; 6], pdu));
    }

    #[test]
    fn hostile_tlvs_are_tolerated_or_rejected_without_panic() {
        let cpu = cpu_x1_frame()[14..].to_vec();
        let end = cpu.len() - 2;

        // TLV length beyond the buffer.
        assert_eq!(Lldpdu::parse(&[0x02, 0x40]), Err(LldpError::Truncated));
        // Zero-length Chassis ID / Port ID TLV: no id, so the mandatory TLV is missing.
        assert_eq!(
            Lldpdu::parse(&[0x02, 0x00, 0x04, 0x02, 0x07, b'p', 0x06, 0x02, 0, 20, 0, 0]),
            Err(LldpError::MissingMandatory("chassis id"))
        );
        assert_eq!(
            Lldpdu::parse(&[0x02, 0x02, 0x07, b'c', 0x04, 0x00, 0x06, 0x02, 0, 20, 0, 0]),
            Err(LldpError::MissingMandatory("port id"))
        );
        assert_eq!(
            Lldpdu::parse(&[0x02, 0x02, 0x07, b'c', 0x04, 0x02, 0x07, b'p', 0, 0]),
            Err(LldpError::MissingMandatory("ttl"))
        );
        // Org TLV shorter than OUI + subtype: counted, not decoded.
        let mut short_org = cpu[..end].to_vec();
        short_org.extend_from_slice(&[0xfe, 0x03, 0x00, 0x0e, 0xcf, 0, 0]);
        assert_eq!(Lldpdu::parse(&short_org).unwrap().unknown_tlvs, 1);
        // PROFINET / 802.3 TLVs with a wrong length: counted, not decoded.
        let mut bad_len = cpu[..end].to_vec();
        bad_len.extend_from_slice(&[0xfe, 0x06, 0x00, 0x0e, 0xcf, 5, 1, 2]);
        bad_len.extend_from_slice(&[0xfe, 0x05, 0x00, 0x12, 0x0f, 1, 3]);
        let p = Lldpdu::parse(&bad_len).unwrap();
        assert_eq!(p.unknown_tlvs, 2);
        assert_eq!(p.pnio_chassis_mac, Some(CPU_MAC)); // the well-formed one still counts
                                                       // Missing End TLV: accepted.
        assert_eq!(
            Lldpdu::parse(&cpu[..end]).unwrap(),
            Lldpdu::parse(&cpu).unwrap()
        );
        // Trailing garbage after End (and a dangling odd byte): ignored.
        let mut trailing = cpu.clone();
        trailing.extend_from_slice(&[0xff, 0xff, 0xff]);
        assert_eq!(
            Lldpdu::parse(&trailing).unwrap(),
            Lldpdu::parse(&cpu).unwrap()
        );
        let mut dangling = cpu[..end].to_vec();
        dangling.push(0x02);
        assert_eq!(
            Lldpdu::parse(&dangling).unwrap(),
            Lldpdu::parse(&cpu).unwrap()
        );
        // Duplicated mandatory TLVs: accepted, the last occurrence wins.
        let mut dup = cpu[..end].to_vec();
        dup.extend_from_slice(&[0x02, 0x02, 0x07, b'x', 0x06, 0x02, 0x00, 0x05, 0, 0]);
        let p = Lldpdu::parse(&dup).unwrap();
        assert_eq!(p.chassis_id, TlvId::local("x"));
        assert_eq!(p.ttl, 5);
        // Empty input.
        assert_eq!(
            Lldpdu::parse(&[]),
            Err(LldpError::MissingMandatory("chassis id"))
        );
    }

    #[test]
    fn no_panic_on_any_truncation_or_byte_corruption() {
        let frame = cpu_x1_frame();
        let switch = foreign_switch_payload();
        for pdu in [&frame[14..], &switch[..]] {
            for n in 0..pdu.len() {
                let _ = Lldpdu::parse(&pdu[..n]);
            }
            for i in 0..pdu.len() {
                for v in [0x00, 0x01, 0x7f, 0x80, 0xfe, 0xff] {
                    let mut p = pdu.to_vec();
                    p[i] = v;
                    if let Ok(parsed) = Lldpdu::parse(&p) {
                        let _ = Peer::from_pdu(&parsed, [0; 6], Instant::now());
                        let _ = parsed.chassis_id.as_text();
                    }
                }
            }
        }
        for n in 0..frame.len() {
            let _ = parse_frame(&frame[..n]);
        }
    }

    #[test]
    fn as_text_formats_by_subtype() {
        assert_eq!(TlvId::local("plc").as_text(), "plc");
        assert_eq!(
            TlvId {
                subtype: 7,
                value: vec![b'a', 0xff]
            }
            .as_text(),
            "a\u{fffd}"
        );
        // Not "locally assigned", not a MAC, but printable text (Port ID subtype 5
        // "Interface name"): shown as text.
        assert_eq!(
            TlvId {
                subtype: 5,
                value: b"Gi1/0/3".to_vec()
            }
            .as_text(),
            "Gi1/0/3"
        );
        // Binary of any other subtype: colon-separated hex.
        assert_eq!(
            TlvId {
                subtype: 3,
                value: vec![0x00, 0x1b, 0x1b, 0x12, 0x00, 0x01]
            }
            .as_text(),
            "00:1b:1b:12:00:01"
        );
    }
}

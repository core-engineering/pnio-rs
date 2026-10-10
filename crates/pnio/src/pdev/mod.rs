//! PDEV: the physical-device view of our one port and interface — LLDP neighbour
//! tracking, the engineering records TIA writes (`PDPortDataCheck`/`Adjust`,
//! `PDInterfaceAdjust`), and the expected-vs-actual peer evaluation that drives the
//! *remote mismatch* diagnosis. Pure: `tick`/`on_rx` return actions, the device loop
//! performs them.

use crate::dcp::NetParams;
use crate::eth::MacAddr;
use crate::lldp::{build_device_pdu, parse_frame, port_id_string, Peer, DEFAULT_TTL, TX_PERIOD};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

pub mod records;

/// Subslot of the device's one port (DAP submodule "Port 1").
pub const PORT_SUBSLOT: u16 = 0x8001;
/// Subslot of the device's interface (DAP submodule "Interface").
pub const IFACE_SUBSLOT: u16 = 0x8000;

/// Where the port's link state comes from.
pub trait LinkProbe: Send {
    /// Whether the port currently has a link.
    fn link_up(&self) -> bool;
}

/// Link state read from `/sys/class/net/<iface>/carrier`.
#[cfg(target_os = "linux")]
pub struct SysfsLink {
    /// The network interface name (e.g. `eno2`).
    pub iface: String,
}

#[cfg(target_os = "linux")]
impl LinkProbe for SysfsLink {
    /// `carrier == "1"`; an unreadable carrier file counts as up, so a missing sysfs
    /// never reports a false link-down.
    fn link_up(&self) -> bool {
        std::fs::read_to_string(format!("/sys/class/net/{}/carrier", self.iface))
            .map(|s| s.trim() == "1")
            .unwrap_or(true)
    }
}

/// A link state set by hand (tests, platforms without sysfs).
pub struct MockLink(pub AtomicBool);

impl LinkProbe for MockLink {
    fn link_up(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// One expected peer from CheckPeers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedPeer {
    /// The expected neighbour's Port ID.
    pub port_id: String,
    /// The expected neighbour's Chassis ID (its NameOfStation).
    pub chassis_id: String,
}

/// The `PDPortDataCheck` the controller wrote, as far as the evaluation needs it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CheckRecord {
    /// CheckPeers: the acceptable neighbours (several = TIA "alternative partners");
    /// empty = no peer check.
    pub peers: Vec<ExpectedPeer>,
    /// CheckMAUType, when present (stored, not evaluated).
    pub mau_type: Option<u16>,
    /// The record as written, for read-back.
    pub raw: Vec<u8>,
}

/// A remote mismatch on the port: actual neighbour vs `CheckPeers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mismatch {
    /// No neighbour while one is expected.
    NoPeer,
    /// The neighbour's Chassis ID (name of station) matches no expected peer.
    Chassis,
    /// The Chassis ID matches an expected peer but the Port ID does not.
    Port,
}

impl Mismatch {
    /// The ExtChannelErrorType under ChannelErrorType `0x8001` "Remote mismatch"
    /// (docs/pdev-golden-frames.md §2.10).
    pub fn ext_error_type(self) -> u16 {
        match self {
            // "Peer name of station mismatch"
            Mismatch::Chassis => 0x8000,
            // "Peer name of port mismatch"
            Mismatch::Port => 0x8001,
            // "No peer detected"
            Mismatch::NoPeer => 0x8005,
        }
    }
}

/// What the device loop must do on behalf of [`Pdev`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PdevAction {
    /// Transmit this full Ethernet frame on the port.
    Send(Vec<u8>),
    /// The remote mismatch changed: raise (`Some`) or clear (`None`) the diagnosis.
    Diag(Option<Mismatch>),
}

/// Our one port and interface: LLDP schedule, neighbour, engineering records, mismatch.
pub struct Pdev {
    mac: [u8; 6],
    net: NetParams,
    own_port_id: String,
    link: Box<dyn LinkProbe>,
    peer: Option<Peer>,
    ever_seen_peer: bool,
    check: Option<CheckRecord>,
    check_written_at: Option<Instant>,
    adjust: Option<Vec<u8>>,
    iface_adjust: Option<Vec<u8>>,
    next_tx: Instant,
    mismatch: Option<Mismatch>,
}

impl Pdev {
    /// A port with no identity yet (blank name), first LLDPDU due at `now`.
    pub fn new(mac: [u8; 6], link: Box<dyn LinkProbe>, now: Instant) -> Pdev {
        Pdev {
            mac,
            net: NetParams::default(),
            own_port_id: port_id_string(""),
            link,
            peer: None,
            ever_seen_peer: false,
            check: None,
            check_written_at: None,
            adjust: None,
            iface_adjust: None,
            next_tx: now,
            mismatch: None,
        }
    }

    /// Adopt a new name/IP suite; the next LLDPDU goes out at `now`.
    pub fn set_identity(&mut self, net: &NetParams, now: Instant) {
        self.net = net.clone();
        self.own_port_id = port_id_string(&net.name);
        self.next_tx = now;
    }

    /// The TTL-0 LLDPDU announcing our departure: Chassis ID, Port ID, TTL = 0 and End
    /// only (docs/pdev-golden-frames.md §3.1 — any other TLV is flagged in a shutdown PDU).
    pub fn shutdown_frame(&self) -> Vec<u8> {
        build_device_pdu(&self.net.name, self.mac, &self.own_port_id, 0).frame(MacAddr(self.mac))
    }

    /// Feed one received LLDP frame: learn (or forget, on TTL 0) the neighbour and
    /// re-evaluate the expected-peer check. A frame that does not parse is dropped.
    pub fn on_rx(&mut self, frame: &[u8], now: Instant) -> Vec<PdevAction> {
        match parse_frame(frame) {
            Ok((src, pdu)) => {
                let peer = Peer::from_pdu(&pdu, src, now);
                if peer.ttl.is_zero() {
                    log::info!("LLDP shutdown from {}", peer.chassis_id);
                    self.peer = None;
                } else {
                    self.ever_seen_peer = true;
                    self.peer = Some(peer);
                }
                self.reevaluate(now)
            }
            Err(e) => {
                log::debug!("dropping LLDP frame: {e}");
                Vec::new()
            }
        }
    }

    /// Timers: expire the neighbour, send the periodic LLDPDU, re-evaluate.
    pub fn tick(&mut self, now: Instant) -> Vec<PdevAction> {
        let mut out = Vec::new();
        if let Some(p) = &self.peer {
            if p.expired(now) {
                log::info!("LLDP peer {} expired", p.chassis_id);
                self.peer = None;
            }
        }
        if now >= self.next_tx {
            let pdu = build_device_pdu(&self.net.name, self.mac, &self.own_port_id, DEFAULT_TTL);
            out.push(PdevAction::Send(pdu.frame(MacAddr(self.mac))));
            self.next_tx = now + TX_PERIOD;
        }
        out.extend(self.reevaluate(now));
        out
    }

    /// The earliest instant `tick` has something to do: next transmit, neighbour
    /// expiry, or the end of the no-peer grace after a Check write (only while that
    /// grace is still pending, so a passed grace never leaves a deadline in the past).
    pub fn next_deadline(&self) -> Option<Instant> {
        let mut d = self.next_tx;
        if let Some(p) = &self.peer {
            d = d.min(p.seen_at + p.ttl);
        }
        if let Some(t) = self.grace_pending_since() {
            d = d.min(t + 2 * TX_PERIOD);
        }
        Some(d)
    }

    /// When the Check that opened a still-pending no-peer grace was written: a
    /// non-empty peer check, no neighbour ever seen, and NoPeer not raised yet.
    fn grace_pending_since(&self) -> Option<Instant> {
        let checking = self.check.as_ref().is_some_and(|c| !c.peers.is_empty());
        if checking && self.peer.is_none() && !self.ever_seen_peer && self.mismatch.is_none() {
            self.check_written_at
        } else {
            None
        }
    }

    fn evaluate(&self, now: Instant) -> Option<Mismatch> {
        let check = self.check.as_ref()?;
        if check.peers.is_empty() {
            return None;
        }
        match &self.peer {
            // Any expected peer matching is a match (alternative partners).
            Some(p) => {
                if check
                    .peers
                    .iter()
                    .any(|e| e.chassis_id == p.chassis_id && e.port_id == p.port_id)
                {
                    None
                } else if check.peers.iter().any(|e| e.chassis_id == p.chassis_id) {
                    Some(Mismatch::Port)
                } else {
                    Some(Mismatch::Chassis)
                }
            }
            // Never seen a neighbour: give it two transmit periods after the Check write.
            None => {
                let grace_over = self
                    .check_written_at
                    .is_some_and(|t| now.saturating_duration_since(t) >= 2 * TX_PERIOD);
                (self.ever_seen_peer || grace_over).then_some(Mismatch::NoPeer)
            }
        }
    }

    /// Re-evaluate; a `Diag` action only when the mismatch changed.
    fn reevaluate(&mut self, now: Instant) -> Vec<PdevAction> {
        let m = self.evaluate(now);
        if m != self.mismatch {
            self.mismatch = m;
            vec![PdevAction::Diag(m)]
        } else {
            Vec::new()
        }
    }

    /// Store (or drop) the expected-peer record TIA wrote; returns the diag change.
    pub fn set_check(&mut self, c: Option<CheckRecord>, now: Instant) -> Vec<PdevAction> {
        self.check = c;
        self.check_written_at = self.check.as_ref().map(|_| now);
        self.reevaluate(now)
    }

    /// Store (or drop) the raw `PDPortDataAdjust` record.
    pub fn set_adjust(&mut self, raw: Option<Vec<u8>>) {
        self.adjust = raw;
    }

    /// Store (or drop) the raw `PDInterfaceAdjust` record.
    pub fn set_iface_adjust(&mut self, raw: Option<Vec<u8>>) {
        self.iface_adjust = raw;
    }

    /// Forget every engineering record (AR loss, ResetToFactory); clears the diag.
    /// The topology context goes with the AR: "a neighbour was seen" is forgotten too,
    /// so the next Check gets its full no-peer grace (2 × TX_PERIOD) again.
    pub fn clear_engineering(&mut self) -> Vec<PdevAction> {
        self.adjust = None;
        self.iface_adjust = None;
        self.check = None;
        self.check_written_at = None;
        self.ever_seen_peer = false;
        match self.mismatch.take() {
            Some(_) => vec![PdevAction::Diag(None)],
            None => Vec::new(),
        }
    }

    /// The current LLDP neighbour.
    pub fn peer(&self) -> Option<&Peer> {
        self.peer.as_ref()
    }

    /// The stored `PDPortDataCheck`.
    pub fn check(&self) -> Option<&CheckRecord> {
        self.check.as_ref()
    }

    /// The stored `PDPortDataAdjust` record.
    pub fn adjust(&self) -> Option<&[u8]> {
        self.adjust.as_deref()
    }

    /// The stored `PDInterfaceAdjust` record.
    pub fn iface_adjust(&self) -> Option<&[u8]> {
        self.iface_adjust.as_deref()
    }

    /// Link state from the [`LinkProbe`].
    pub fn link_up(&self) -> bool {
        self.link.link_up()
    }

    /// The current remote mismatch, if any.
    pub fn mismatch(&self) -> Option<Mismatch> {
        self.mismatch
    }

    /// Our LLDP Port ID.
    pub fn own_port_id(&self) -> &str {
        &self.own_port_id
    }

    /// The interface MAC (chassis MAC TLV, PDInterfaceDataReal).
    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    /// The identity last given to [`Pdev::set_identity`].
    pub fn net(&self) -> &NetParams {
        &self.net
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dcp::NetParams;
    use crate::lldp::{build_device_pdu, parse_frame};
    use std::time::Duration;

    const MAC: [u8; 6] = [2, 0, 0, 0, 0, 1];

    fn net() -> NetParams {
        NetParams {
            name: "pnio-dev".into(),
            ip: [172, 16, 2, 10],
            subnet: [255, 255, 255, 0],
            gateway: [172, 16, 2, 10],
        }
    }

    fn pdev(now: Instant) -> Pdev {
        let mut p = Pdev::new(MAC, Box::new(MockLink(true.into())), now);
        p.set_identity(&net(), now);
        p
    }

    fn cpu_frame(chassis: &str, port: &str, ttl: u16) -> Vec<u8> {
        let mut pdu = build_device_pdu(chassis, [0xec, 0, 0, 0, 0, 1], port, ttl);
        pdu.ttl = ttl;
        pdu.frame(crate::eth::MacAddr([0xec, 0, 0, 0, 0, 1]))
    }

    fn check(chassis: &str, port: &str) -> CheckRecord {
        CheckRecord {
            peers: vec![ExpectedPeer {
                port_id: port.into(),
                chassis_id: chassis.into(),
            }],
            mau_type: None,
            raw: vec![],
        }
    }

    fn sends(a: &[PdevAction]) -> usize {
        a.iter()
            .filter(|x| matches!(x, PdevAction::Send(_)))
            .count()
    }
    fn diag(a: &[PdevAction]) -> Option<Option<Mismatch>> {
        a.iter().find_map(|x| {
            if let PdevAction::Diag(m) = x {
                Some(*m)
            } else {
                None
            }
        })
    }

    #[test]
    fn sends_immediately_then_every_period() {
        let t0 = Instant::now();
        let mut p = pdev(t0);
        let a = p.tick(t0);
        assert_eq!(sends(&a), 1);
        let (src, pdu) = parse_frame(match &a[0] {
            PdevAction::Send(f) => f,
            _ => panic!(),
        })
        .unwrap();
        assert_eq!(src, MAC);
        assert_eq!(pdu.chassis_id.as_text(), "pnio-dev");
        assert_eq!(pdu.ttl, 20);
        assert_eq!(p.next_deadline(), Some(t0 + Duration::from_secs(5)));
        assert_eq!(sends(&p.tick(t0 + Duration::from_secs(4))), 0);
        assert_eq!(sends(&p.tick(t0 + Duration::from_secs(5))), 1);
    }

    #[test]
    fn identity_change_sends_at_once_and_blank_name_uses_mac() {
        let t0 = Instant::now();
        let mut p = pdev(t0);
        p.tick(t0);
        p.set_identity(&NetParams::default(), t0 + Duration::from_secs(1));
        let a = p.tick(t0 + Duration::from_secs(1));
        assert_eq!(sends(&a), 1);
        let (_, pdu) = parse_frame(match &a[0] {
            PdevAction::Send(f) => f,
            _ => panic!(),
        })
        .unwrap();
        assert_eq!(pdu.chassis_id.subtype, 4);
    }

    #[test]
    fn peer_is_learned_served_and_expires() {
        let t0 = Instant::now();
        let mut p = pdev(t0);
        p.on_rx(&cpu_frame("plc", "port-001.plc", 20), t0);
        assert_eq!(p.peer().unwrap().chassis_id, "plc");
        assert!(p.next_deadline().unwrap() <= t0 + Duration::from_secs(5));
        p.tick(t0 + Duration::from_secs(19));
        assert!(p.peer().is_some());
        p.tick(t0 + Duration::from_secs(20));
        assert!(p.peer().is_none());
        p.on_rx(
            &cpu_frame("plc", "port-001.plc", 20),
            t0 + Duration::from_secs(21),
        );
        p.on_rx(
            &cpu_frame("plc", "port-001.plc", 0),
            t0 + Duration::from_secs(22),
        ); // shutdown PDU
        assert!(p.peer().is_none());
    }

    #[test]
    fn evaluation_matrix() {
        let t0 = Instant::now();
        let mut p = pdev(t0);
        // No check → never a diag, whatever the peer does.
        assert_eq!(
            diag(&p.on_rx(&cpu_frame("plc", "port-001.plc", 20), t0)),
            None
        );
        // Check matching the peer → explicit "no mismatch" is not emitted (nothing changed).
        assert_eq!(
            diag(&p.set_check(Some(check("plc", "port-001.plc")), t0)),
            None
        );
        assert_eq!(p.mismatch(), None);
        // Port differs → Port.
        assert_eq!(
            diag(&p.set_check(Some(check("plc", "port-002.plc")), t0)),
            Some(Some(Mismatch::Port))
        );
        // Chassis differs → Chassis (takes precedence over port).
        assert_eq!(
            diag(&p.set_check(Some(check("other", "port-002.plc")), t0)),
            Some(Some(Mismatch::Chassis))
        );
        // Peer changes to match → cleared.
        assert_eq!(
            diag(&p.on_rx(&cpu_frame("other", "port-002.plc", 20), t0)),
            Some(None)
        );
        // Peer expires → NoPeer (we had seen one).
        assert_eq!(
            diag(&p.tick(t0 + Duration::from_secs(20))),
            Some(Some(Mismatch::NoPeer))
        );
        // Clearing engineering clears the diag.
        assert_eq!(diag(&p.clear_engineering()), Some(None));
        assert_eq!(p.check(), None);
    }

    #[test]
    fn no_peer_waits_two_periods_after_the_check_when_none_was_ever_seen() {
        let t0 = Instant::now();
        let mut p = pdev(t0);
        assert_eq!(
            diag(&p.set_check(Some(check("plc", "port-001.plc")), t0)),
            None
        );
        assert_eq!(diag(&p.tick(t0 + Duration::from_secs(9))), None);
        assert_eq!(
            diag(&p.tick(t0 + Duration::from_secs(10))),
            Some(Some(Mismatch::NoPeer))
        );
        // The grace deadline (check written at t0, + 2 × TX_PERIOD) is visible to the loop:
        // move the transmit schedule past it without letting the grace expire (sends at
        // t0 and t0+7 s → next_tx = t0+12 s > t0+10 s); the grace term is then the minimum.
        let mut q = pdev(t0);
        q.tick(t0);
        q.set_check(Some(check("plc", "port-001.plc")), t0);
        assert_eq!(sends(&q.tick(t0 + Duration::from_secs(7))), 1);
        assert_eq!(q.next_deadline(), Some(t0 + Duration::from_secs(10)));
    }

    /// AR loss forgets the topology context: a neighbour seen under the old AR does
    /// not cut the no-peer grace of a Check written for the next one.
    #[test]
    fn clear_engineering_restores_the_no_peer_grace() {
        let t0 = Instant::now();
        let mut p = pdev(t0);
        p.on_rx(&cpu_frame("plc", "port-001.plc", 20), t0);
        p.clear_engineering();
        // New AR, new Check at t (peer still cached, matches); the peer then expires.
        let t = t0 + Duration::from_secs(19);
        assert_eq!(
            diag(&p.set_check(Some(check("plc", "port-001.plc")), t)),
            None
        );
        assert_eq!(diag(&p.tick(t + Duration::from_secs(1))), None);
        assert!(p.peer().is_none(), "expired at t0 + 20 s");
        assert_eq!(
            diag(&p.tick(t + Duration::from_secs(10))),
            Some(Some(Mismatch::NoPeer))
        );
    }

    #[test]
    fn two_expected_peers_match_if_any_matches() {
        let t0 = Instant::now();
        let mut p = pdev(t0);
        p.on_rx(&cpu_frame("plc", "port-002.plc", 20), t0);
        let c = CheckRecord {
            peers: vec![
                ExpectedPeer {
                    port_id: "port-001.plc".into(),
                    chassis_id: "plc".into(),
                },
                ExpectedPeer {
                    port_id: "port-002.plc".into(),
                    chassis_id: "plc".into(),
                },
            ],
            mau_type: None,
            raw: vec![],
        };
        assert_eq!(diag(&p.set_check(Some(c), t0)), None);
        assert_eq!(p.mismatch(), None);
        let empty = CheckRecord::default();
        assert_eq!(diag(&p.set_check(Some(empty), t0)), None);
        // Neither alternative matches: a known chassis on another port is Port, an
        // unknown chassis is Chassis; an empty peer list then clears the diag.
        let alt = CheckRecord {
            peers: vec![
                ExpectedPeer {
                    port_id: "port-001.plc".into(),
                    chassis_id: "plc".into(),
                },
                ExpectedPeer {
                    port_id: "port-001.sw".into(),
                    chassis_id: "sw".into(),
                },
            ],
            mau_type: None,
            raw: vec![],
        };
        assert_eq!(
            diag(&p.set_check(Some(alt), t0)),
            Some(Some(Mismatch::Port))
        );
        assert_eq!(
            diag(&p.on_rx(&cpu_frame("plc", "port-001.sw", 20), t0)),
            None
        ); // cross-pair: still Port
        assert_eq!(
            diag(&p.on_rx(&cpu_frame("other", "port-001.plc", 20), t0)),
            Some(Some(Mismatch::Chassis))
        );
        assert_eq!(
            diag(&p.on_rx(&cpu_frame("sw", "port-001.sw", 20), t0)),
            Some(None)
        );
        assert_eq!(
            diag(&p.on_rx(&cpu_frame("other", "port-001.plc", 20), t0)),
            Some(Some(Mismatch::Chassis))
        );
        assert_eq!(
            diag(&p.set_check(Some(CheckRecord::default()), t0)),
            Some(None)
        );
    }

    #[test]
    fn diag_is_emitted_once_per_change_and_garbage_is_dropped() {
        let t0 = Instant::now();
        let mut p = pdev(t0);
        assert_eq!(
            diag(&p.set_check(Some(check("plc", "port-001.plc")), t0)),
            None
        );
        assert_eq!(
            diag(&p.on_rx(&cpu_frame("plc", "port-002.plc", 20), t0)),
            Some(Some(Mismatch::Port))
        );
        for s in 1..=12 {
            let now = t0 + Duration::from_secs(s);
            assert_eq!(
                diag(&p.on_rx(&cpu_frame("plc", "port-002.plc", 20), now)),
                None
            );
            assert_eq!(diag(&p.tick(now)), None);
        }
        assert!(p.on_rx(&[0u8; 10], t0).is_empty());
        assert!(p.on_rx(&net_frame_not_lldp(), t0).is_empty());
        assert_eq!(p.mismatch(), Some(Mismatch::Port));
    }

    fn net_frame_not_lldp() -> Vec<u8> {
        let mut f = cpu_frame("plc", "port-001.plc", 20);
        f[12..14].copy_from_slice(&0x8892u16.to_be_bytes());
        f
    }

    #[test]
    fn grace_deadline_is_dropped_once_no_peer_is_raised() {
        let t0 = Instant::now();
        let mut p = pdev(t0);
        p.set_check(Some(check("plc", "port-001.plc")), t0);
        p.tick(t0);
        let a = p.tick(t0 + Duration::from_secs(10));
        assert_eq!((sends(&a), diag(&a)), (1, Some(Some(Mismatch::NoPeer))));
        // The grace end (t0+10 s) is past: the loop must not be woken for it again.
        assert_eq!(p.next_deadline(), Some(t0 + Duration::from_secs(15)));
    }

    #[test]
    fn ext_error_types_and_link_probe() {
        assert_eq!(
            [Mismatch::Chassis, Mismatch::Port, Mismatch::NoPeer].map(Mismatch::ext_error_type),
            [0x8000, 0x8001, 0x8005]
        );
        let link = std::sync::Arc::new(MockLink(true.into()));
        struct Shared(std::sync::Arc<MockLink>);
        impl LinkProbe for Shared {
            fn link_up(&self) -> bool {
                self.0.link_up()
            }
        }
        let p = Pdev::new(MAC, Box::new(Shared(link.clone())), Instant::now());
        assert!(p.link_up());
        link.0.store(false, Ordering::Relaxed);
        assert!(!p.link_up());
    }

    #[test]
    fn shutdown_frame_has_ttl_zero_and_own_port_id_is_pinned_format() {
        let p = pdev(Instant::now());
        let (_, pdu) = parse_frame(&p.shutdown_frame()).unwrap();
        assert_eq!(pdu.ttl, 0);
        // Shutdown PDU = Chassis ID, Port ID, TTL, End only (docs/pdev-golden-frames.md §3.1).
        assert_eq!(
            (
                pdu.pnio_delay,
                pdu.pnio_port_status,
                pdu.pnio_chassis_mac,
                pdu.mac_phy.clone()
            ),
            (None, None, None, None)
        );
        assert_eq!(p.own_port_id(), crate::lldp::port_id_string("pnio-dev"));
        assert_eq!((p.mac(), p.net().name.as_str()), (MAC, "pnio-dev"));
    }
}

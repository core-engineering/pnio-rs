//! PROFINET Context Manager (CM): PNIO status, PNIO block header, and the
//! Connect-request block parsers (ARBlockReq, IOCRBlockReq, ExpectedSubmoduleBlockReq,
//! AlarmCRBlockReq) needed to establish an Application Relationship (AR).

pub mod ar;
pub mod block;
pub mod connect;
pub mod control;
pub mod model;
pub mod status;
pub mod write;

pub use ar::{
    AbortReason, Action, Ar, ArContext, ArState, Event, ACTIVITY_TIMEOUT_UNIT,
    APP_READY_MAX_ATTEMPTS, APP_READY_TIMEOUT,
};
pub use block::{
    ty, AlarmCrBlockReq, ArBlockReq, BlockHeader, Cursor, DataDescription, ExpectedApi,
    ExpectedSubmodule, ExpectedSubmoduleBlockReq, IocrApi, IocrBlockReq, IocrObject,
};
pub use connect::{build_connect_res, validate, ArParams, ConnectReq, IocrParams};
pub use control::{app_ready_req, cmd, prm_end_done, release_done, ControlBlock};
pub use model::{DeviceModel, SlotModel, SubmoduleModel};
pub use status::{ConnectBlock, PnioStatus};
pub use write::{build_write_res, Record, WriteReq, INDEX_MULTIPLE_WRITE};

use crate::rpc::{
    Drep, NdrRequest, NdrResponse, Opnum, PacketType, RpcError, RpcHeader, Uuid, FLAG1_IDEMPOTENT,
    FLAG1_NO_FACK, PNIO_CONTROLLER_INTERFACE, PNIO_DEVICE_INTERFACE, PNIO_UDP_PORT,
};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Instant;
use thiserror::Error;

/// Errors from parsing/serializing PNIO blocks (the 6-byte header and the per-type bodies).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BlockError {
    #[error("block buffer too short: need {need}, have {have}")]
    TooShort { need: usize, have: usize },
    #[error("unexpected block type: expected {expected:#06x}, got {got:#06x}")]
    UnexpectedType { expected: u16, got: u16 },
    #[error("bad block version {0}.{1} (expected 1.0)")]
    BadVersion(u8, u8),
    #[error("bad block length: declared {declared}, available {available}")]
    BadLength { declared: u16, available: usize },
    #[error("malformed block: {0}")]
    Malformed(&'static str),
}

/// Errors from the Context Manager's AR establishment / lifecycle handling.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CmError {
    #[error("block error: {0}")]
    Block(#[from] BlockError),
    #[error("connect rejected: {0:?}")]
    Reject(PnioStatus),
}

// ---------------------------------------------------------------------------------
// Cm: RPC datagram <-> AR glue.
// ---------------------------------------------------------------------------------

/// Our advertised max NDR args length (`args_max`/`max_count`) for outgoing calls —
/// p-net's value, required byte-for-byte by the golden ApplicationReady request.
pub const RPC_ARGS_MAX: u32 = 1340;

/// Maximum number of cached full response PDUs kept for retransmission, FIFO eviction.
const CACHE_CAPACITY: usize = 4;

/// One outgoing RPC datagram: the raw bytes to send and the address to send them to.
#[derive(Debug, Clone, PartialEq)]
pub struct Outgoing {
    pub bytes: Vec<u8>,
    pub to: SocketAddr,
}

/// The effects of one `Cm::handle_datagram`/`Cm::tick` call: datagrams to send, and AR
/// state-change notifications to report (state plus the abort reason, when any).
#[derive(Debug, Default, PartialEq)]
pub struct CmOutput {
    pub send: Vec<Outgoing>,
    pub notify: Vec<(ArState, Option<AbortReason>)>,
}

/// The request-specific fields needed to build and cache a Response PDU, gathered
/// from the incoming request's RPC header plus its NDR `args_max`.
struct RespondCtx {
    object: Uuid,
    interface: Uuid,
    activity: Uuid,
    seq_num: u32,
    opnum: u16,
    args_max: u32,
    from: SocketAddr,
}

/// Maps a block-parsing failure to the PNIO status to answer with: `Reject(status)`
/// carries its own status through unchanged; a `Block` error logs and falls back to
/// `default` (the caller's per-opnum convention).
fn error_status(e: &CmError, default: PnioStatus) -> PnioStatus {
    match e {
        CmError::Reject(status) => *status,
        CmError::Block(be) => {
            log::warn!("PNIO block parse error: {be}");
            default
        }
    }
}

fn respond(status: PnioStatus) -> Vec<Action> {
    vec![Action::Respond {
        status,
        blocks: Vec::new(),
    }]
}

/// RPC datagram <-> AR glue: parses the DCE-RPC/NDR/PNIO envelope of an incoming UDP
/// datagram, feeds the pure [`Ar`] state machine the resulting [`Event`], and wraps its
/// [`Action`]s back into complete RPC PDUs — byte-exact against the golden p-net <-> CPU
/// capture. Also owns the small response cache (a retransmitted request is answered with
/// the identical cached bytes instead of re-running the AR machine) and the call sequence
/// number / controller address used for our outgoing ApplicationReady calls.
pub struct Cm {
    ar: Ar,
    /// Activity UUID used for calls we place to the controller (ApplicationReady).
    activity_seed: Uuid,
    /// Seq num to assign to the *next new* outgoing call to the controller.
    call_seq: u32,
    /// Seq num of the current outstanding call, reused unchanged by `tick`'s retries.
    current_call_seq: Option<u32>,
    /// Controller address for outgoing calls, learned from the Connect request's
    /// source address (its IP, PNIO's well-known UDP port).
    controller_addr: Option<SocketAddr>,
    /// Cached full response PDUs, keyed by `(activity, seq_num)`, FIFO eviction at
    /// [`CACHE_CAPACITY`] entries.
    cache: VecDeque<((Uuid, u32), Vec<u8>)>,
}

impl Cm {
    pub fn new(model: DeviceModel, activity_seed: Uuid) -> Cm {
        Cm {
            ar: Ar::new(model),
            activity_seed,
            call_seq: 0,
            current_call_seq: None,
            controller_addr: None,
            cache: VecDeque::new(),
        }
    }

    pub fn state(&self) -> ArState {
        self.ar.state()
    }

    pub fn context(&self) -> Option<&ArContext> {
        self.ar.context()
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.ar.next_deadline()
    }

    /// Parse one incoming UDP datagram and drive the AR machine, returning the PDUs to
    /// send and the AR notifications to report. RPC envelope errors ([`RpcHeader::parse`],
    /// [`NdrRequest::parse`]/[`NdrResponse::parse`], a foreign interface UUID) propagate to
    /// the caller to log and drop; PNIO-block-level parse failures are answered in-band
    /// with the appropriate PNIO status instead.
    pub fn handle_datagram(
        &mut self,
        buf: &[u8],
        from: SocketAddr,
        now: Instant,
    ) -> Result<CmOutput, RpcError> {
        let h = RpcHeader::parse(buf)?;
        let body = &buf[RpcHeader::LEN..];
        let mut output = CmOutput::default();

        if h.ptype == PacketType::Response {
            // The controller answering our ApplicationReady call.
            let (n, _blocks) = NdrResponse::parse(body, h.drep)?;
            let actions = self.ar.on(
                Event::AppReadyRsp {
                    status: PnioStatus(n.status),
                },
                now,
            );
            self.apply(actions, None, false, &mut output);
            return Ok(output);
        }
        if h.ptype != PacketType::Request {
            log::warn!(
                "dropping DCE-RPC packet type {:?}, only Request/Response handled",
                h.ptype
            );
            return Ok(output);
        }

        if h.interface != PNIO_DEVICE_INTERFACE {
            return Err(RpcError::BadInterface(h.interface));
        }

        let cache_key = (h.activity, h.seq_num);
        if let Some(bytes) = self.cache_lookup(&cache_key) {
            log::debug!(
                "cache hit for activity {} seq_num {}: resending cached response",
                h.activity,
                h.seq_num
            );
            output.send.push(Outgoing { bytes, to: from });
            return Ok(output);
        }

        let (req, blocks) = NdrRequest::parse(body, h.drep)?;

        let actions = match Opnum::from_u16(h.opnum) {
            Some(Opnum::Connect) => {
                let actions = match ConnectReq::parse(blocks) {
                    Ok(req) => self.ar.on(Event::ConnectReq(req), now),
                    Err(e) => respond(error_status(
                        &e,
                        PnioStatus::connect_reject(ConnectBlock::ArBlock, 0xfe),
                    )),
                };
                // Remember the controller's address (PrmEnd, later on the same AR, needs
                // it to place the ApplicationReady call, and `tick`'s retries need it with
                // no request in hand at all), and drop any stale outstanding-call seq_num,
                // whenever this Connect actually (re-)established the AR: the nominal
                // Idle -> Connected path, or a controller-reconnect takeover (`Ar` aborts
                // the stale AR and re-establishes a new one within the same `on()` call, so
                // both `Notify`s appear in `actions`). A stray Connect from another host
                // while an AR already exists (refused with `connect_ar_already_exists`) or
                // a rejected/malformed one produces no `Notify { state: Connected, .. }`
                // and must not redirect the real AR's outgoing calls.
                if actions.iter().any(|a| {
                    matches!(
                        a,
                        Action::Notify {
                            state: ArState::Connected,
                            ..
                        }
                    )
                }) {
                    self.controller_addr = Some(SocketAddr::new(from.ip(), PNIO_UDP_PORT));
                    // `call_seq` is NOT reset here: it's scoped to `activity_seed`, which
                    // is fixed for the device's lifetime, not per-AR. DCE-RPC CL duplicate
                    // detection is per `(activity, seq_num)` with a monotonically
                    // increasing seq_num — the same rule our own response cache relies on
                    // — so restarting it at 0 on a reconnect would re-send `(activity_seed,
                    // 0)`, a pair the controller already completed for the previous AR, and
                    // risk being discarded as a duplicate.
                    self.current_call_seq = None;
                }
                actions
            }
            Some(Opnum::Write) => match WriteReq::parse(blocks) {
                Ok(req) => self.ar.on(Event::WriteReq(req), now),
                Err(e) => respond(error_status(&e, PnioStatus::write_index_unsupported())),
            },
            Some(Opnum::Control) => match ControlBlock::parse(blocks) {
                Ok(cb) if cb.command == cmd::PRM_END => self.ar.on(Event::PrmEndReq(cb), now),
                Ok(_) => respond(PnioStatus::control_wrong_state()),
                Err(e) => respond(error_status(&e, PnioStatus::control_wrong_state())),
            },
            Some(Opnum::Release) => match ControlBlock::parse(blocks) {
                Ok(cb) => self.ar.on(Event::ReleaseReq(cb), now),
                Err(e) => respond(error_status(&e, PnioStatus::control_wrong_state())),
            },
            // Read/ReadImplicit are deliberately out of scope, but answered with the
            // PNIORW "invalid index" status rather than `service_unsupported` so the
            // CPU's `0xfbff` connection-monitoring probe decodes sensibly in a trace
            // instead of looking like a Connect-block error.
            Some(Opnum::Read) | Some(Opnum::ReadImplicit) => {
                respond(PnioStatus::read_index_unsupported())
            }
            None => respond(PnioStatus::service_unsupported()),
        };

        let ctx = RespondCtx {
            object: h.object,
            interface: h.interface,
            activity: h.activity,
            seq_num: h.seq_num,
            opnum: h.opnum,
            args_max: req.args_max,
            from,
        };
        self.apply(actions, Some((ctx, cache_key)), true, &mut output);
        Ok(output)
    }

    /// Drive the AR machine's timers (activity / ApplicationReady retry) and wrap any
    /// resulting retry call or notification.
    pub fn tick(&mut self, now: Instant) -> CmOutput {
        let mut output = CmOutput::default();
        let actions = self.ar.on(Event::Tick, now);
        self.apply(actions, None, false, &mut output);
        output
    }

    /// Force the AR to `Idle` for an externally-observed `reason` (e.g. the RT
    /// runner's consumer watchdog expiring), mapping the resulting actions exactly
    /// like [`Cm::tick`] does. A no-op (no `Notify`) if the AR is already `Idle`.
    pub fn abort(&mut self, reason: AbortReason, now: Instant) -> CmOutput {
        let mut output = CmOutput::default();
        let actions = self.ar.on(Event::Abort(reason), now);
        self.apply(actions, None, false, &mut output);
        output
    }

    /// Turn a batch of `Ar` actions into `CmOutput`: build+cache a Response PDU for each
    /// `Respond`, build a Request PDU to the controller for each `CallController`
    /// (`call_is_new` selects a fresh call sequence number vs. reusing the outstanding
    /// one, for `tick`'s retries), and forward `Notify`s unchanged.
    fn apply(
        &mut self,
        actions: Vec<Action>,
        resp_ctx: Option<(RespondCtx, (Uuid, u32))>,
        call_is_new: bool,
        output: &mut CmOutput,
    ) {
        for action in actions {
            match action {
                Action::Respond { status, blocks } => match &resp_ctx {
                    Some((ctx, cache_key)) => {
                        let pdu = build_response_pdu(ctx, status, blocks);
                        self.cache_insert(*cache_key, pdu.clone());
                        output.send.push(Outgoing {
                            bytes: pdu,
                            to: ctx.from,
                        });
                    }
                    None => {
                        log::warn!("Respond action with no request context; dropping");
                    }
                },
                Action::CallController { blocks } => {
                    let to = self
                        .controller_addr
                        .expect("controller_addr set at Connect before any AR can reach PrmEnd");
                    let pdu = self.build_call_pdu(blocks, call_is_new);
                    output.send.push(Outgoing { bytes: pdu, to });
                }
                Action::Notify { state, reason } => output.notify.push((state, reason)),
            }
        }
    }

    /// Build the ApplicationReady request PDU: fresh call (`is_new`) advances
    /// `call_seq` and remembers it as the outstanding call; a retry reuses it unchanged.
    fn build_call_pdu(&mut self, blocks: Vec<u8>, is_new: bool) -> Vec<u8> {
        let object = self
            .ar
            .context()
            .expect("ctx present for CallController action")
            .params
            .initiator_object_uuid;
        let seq_num = if is_new {
            let seq_num = self.call_seq;
            self.call_seq += 1;
            self.current_call_seq = Some(seq_num);
            seq_num
        } else {
            self.current_call_seq
                .expect("retry call with no prior new call")
        };
        let header = RpcHeader {
            ptype: PacketType::Request,
            flags1: FLAG1_IDEMPOTENT,
            flags2: 0,
            drep: Drep::BIG,
            serial_hi: 0,
            object,
            interface: PNIO_CONTROLLER_INTERFACE,
            activity: self.activity_seed,
            server_boot: 0,
            if_version: 1,
            seq_num,
            opnum: Opnum::Control.to_u16(),
            ihint: 0xffff,
            ahint: 0xffff,
            frag_len: (NdrRequest::LEN + blocks.len()) as u16,
            frag_num: 0,
            auth_proto: 0,
            serial_lo: 0,
        };
        let ndr = NdrRequest::for_blocks(RPC_ARGS_MAX, blocks.len() as u32);
        let mut pdu = Vec::with_capacity(RpcHeader::LEN + NdrRequest::LEN + blocks.len());
        header.write(&mut pdu);
        ndr.write(&mut pdu, Drep::BIG);
        pdu.extend_from_slice(&blocks);
        pdu
    }

    fn cache_lookup(&self, key: &(Uuid, u32)) -> Option<Vec<u8>> {
        self.cache
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, pdu)| pdu.clone())
    }

    fn cache_insert(&mut self, key: (Uuid, u32), pdu: Vec<u8>) {
        if self.cache.len() >= CACHE_CAPACITY {
            self.cache.pop_front();
        }
        self.cache.push_back((key, pdu));
    }
}

/// Build the Response PDU for a request's [`Action::Respond`]: an OK status echoes the
/// blocks, any other status answers with no blocks (the NDR header still carries the
/// request's `args_max` in `max_count`, matching p-net's convention).
fn build_response_pdu(ctx: &RespondCtx, status: PnioStatus, blocks: Vec<u8>) -> Vec<u8> {
    let out_blocks = if status.is_ok() { blocks } else { Vec::new() };
    let header = RpcHeader {
        ptype: PacketType::Response,
        flags1: FLAG1_IDEMPOTENT | FLAG1_NO_FACK,
        flags2: 0,
        drep: Drep::BIG,
        serial_hi: 0,
        object: ctx.object,
        interface: ctx.interface,
        activity: ctx.activity,
        server_boot: 0,
        if_version: 1,
        seq_num: ctx.seq_num,
        opnum: ctx.opnum,
        ihint: 0xffff,
        ahint: 0xffff,
        frag_len: (NdrResponse::LEN + out_blocks.len()) as u16,
        frag_num: 0,
        auth_proto: 0,
        serial_lo: 0,
    };
    let ndr = if status.is_ok() {
        NdrResponse::ok(ctx.args_max, out_blocks.len() as u32)
    } else {
        NdrResponse::error(status.to_u32(), ctx.args_max)
    };
    let mut pdu = Vec::with_capacity(RpcHeader::LEN + NdrResponse::LEN + out_blocks.len());
    header.write(&mut pdu);
    ndr.write(&mut pdu, Drep::BIG);
    pdu.extend_from_slice(&out_blocks);
    pdu
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cm::model::DeviceModel;
    use crate::eth::MacAddr;
    use crate::rpc::Uuid;
    use crate::testutil::{golden, RPC_OFF};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::time::Instant;

    const MAC: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);
    fn cpu() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(172, 16, 2, 100)), 54766)
    }
    fn cpu_cm() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(172, 16, 2, 100)), 34964)
    }
    fn cm() -> Cm {
        Cm::new(
            DeviceModel::pnet_sample(MAC),
            Uuid::parse_str("14af198a-1234-1056-8079-8cf319cd19f8").unwrap(),
        )
    }
    fn pdu(name: &str) -> Vec<u8> {
        golden(name)[RPC_OFF..].to_vec()
    }

    #[test]
    fn full_exchange_is_byte_exact_including_rpc_headers() {
        let mut cm = cm();
        let now = Instant::now();
        let o = cm.handle_datagram(&pdu("connect_req"), cpu(), now).unwrap();
        assert_eq!(o.send.len(), 1);
        assert_eq!(o.send[0].bytes, pdu("connect_res"));
        assert_eq!(o.send[0].to, cpu());
        assert_eq!(o.notify, vec![(ArState::Connected, None)]);
        let o = cm.handle_datagram(&pdu("write_req"), cpu(), now).unwrap();
        assert_eq!(o.send[0].bytes, pdu("write_res"));
        let o = cm.handle_datagram(&pdu("prmend_req"), cpu(), now).unwrap();
        assert_eq!(o.send[0].bytes, pdu("prmend_res"));
        assert_eq!(o.send[1].bytes, pdu("appready_req"));
        assert_eq!(o.send[1].to, cpu_cm());
        assert_eq!(cm.state(), ArState::AppReadySent);
        let o = cm
            .handle_datagram(&pdu("appready_res"), cpu_cm(), now)
            .unwrap();
        assert!(o.send.is_empty());
        assert_eq!(o.notify, vec![(ArState::Data, None)]);
        assert_eq!(cm.state(), ArState::Data);
    }

    #[test]
    fn retransmitted_request_gets_cached_response() {
        let mut cm = cm();
        let now = Instant::now();
        let first = cm.handle_datagram(&pdu("connect_req"), cpu(), now).unwrap();
        let again = cm.handle_datagram(&pdu("connect_req"), cpu(), now).unwrap();
        assert_eq!(again.send[0].bytes, first.send[0].bytes);
        assert!(again.notify.is_empty());
    }

    #[test]
    fn stray_connect_does_not_redirect_app_ready() {
        let mut cm = cm();
        let now = Instant::now();
        let o = cm.handle_datagram(&pdu("connect_req"), cpu(), now).unwrap();
        assert_eq!(o.notify, vec![(ArState::Connected, None)]);

        // Same Connect request bytes, but from a different host, with a different AR
        // UUID, initiator object UUID, and seq_num (so it's a distinct RPC call, not
        // a cached-response retransmit, and not a same-initiator reconnect either):
        // the AR is already `Connected`, so this must be refused without touching
        // `controller_addr`.
        let mut stray = pdu("connect_req");
        stray[64] = 1; // seq_num (LE low byte): 0 -> 1
        stray[108..124].copy_from_slice(&[0x11; 16]); // ARBlockReq.ARUUID
        stray[132..148].copy_from_slice(&[0x22; 16]); // ARBlockReq.InitiatorObjectUUID
        let stray_from = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(172, 16, 2, 77)), 40000);
        let o = cm.handle_datagram(&stray, stray_from, now).unwrap();
        let (n, _) =
            crate::rpc::NdrResponse::parse(&o.send[0].bytes[80..], crate::rpc::Drep::BIG).unwrap();
        assert_eq!(
            PnioStatus(n.status),
            PnioStatus::connect_ar_already_exists()
        );
        assert_eq!(cm.state(), ArState::Connected);

        // The real AR's ApplicationReady call (via PrmEnd) must still go to the real
        // controller, not to the stray host.
        let o = cm.handle_datagram(&pdu("prmend_req"), cpu(), now).unwrap();
        assert_eq!(o.send[1].bytes, pdu("appready_req"));
        assert_eq!(o.send[1].to, cpu_cm());
    }

    #[test]
    fn unsupported_opnum_gets_error_status_response() {
        let mut cm = cm();
        let mut read = pdu("prmend_req");
        read[68] = 2; // opnum Read (LE low byte)
        let o = cm.handle_datagram(&read, cpu(), Instant::now()).unwrap();
        let h = crate::rpc::RpcHeader::parse(&o.send[0].bytes).unwrap();
        assert_eq!(h.ptype, crate::rpc::PacketType::Response);
        assert_eq!(h.opnum, 2);
        let (n, blocks) =
            crate::rpc::NdrResponse::parse(&o.send[0].bytes[80..], crate::rpc::Drep::BIG).unwrap();
        assert_eq!(PnioStatus(n.status), PnioStatus::read_index_unsupported());
        assert!(blocks.is_empty());
    }

    #[test]
    fn unknown_opnum_gets_service_unsupported_status() {
        let mut cm = cm();
        let mut bad = pdu("prmend_req");
        bad[68] = 99; // opnum unmapped by Opnum::from_u16 (LE low byte)
        let o = cm.handle_datagram(&bad, cpu(), Instant::now()).unwrap();
        let (n, blocks) =
            crate::rpc::NdrResponse::parse(&o.send[0].bytes[80..], crate::rpc::Drep::BIG).unwrap();
        assert_eq!(PnioStatus(n.status), PnioStatus::service_unsupported());
        assert!(blocks.is_empty());
    }

    #[test]
    fn rejected_connect_has_error_status_and_no_blocks() {
        let mut cm = Cm::new(
            {
                let mut m = DeviceModel::pnet_sample(MAC);
                m.slots.pop();
                m
            },
            Uuid::NIL,
        );
        let o = cm
            .handle_datagram(&pdu("connect_req"), cpu(), Instant::now())
            .unwrap();
        let (n, blocks) =
            crate::rpc::NdrResponse::parse(&o.send[0].bytes[80..], crate::rpc::Drep::BIG).unwrap();
        assert_eq!(
            PnioStatus(n.status),
            PnioStatus::connect_reject(ConnectBlock::ExpectedSubmodule, 7)
        );
        assert!(blocks.is_empty());
        assert_eq!(cm.state(), ArState::Idle);
    }

    #[test]
    fn wrong_interface_and_garbage_are_errors_not_panics() {
        let mut cm = cm();
        let mut bad = pdu("connect_req");
        bad[24] = 0xff;
        assert!(matches!(
            cm.handle_datagram(&bad, cpu(), Instant::now()),
            Err(RpcError::BadInterface(_))
        ));
        assert!(matches!(
            cm.handle_datagram(&[1, 2, 3], cpu(), Instant::now()),
            Err(RpcError::TooShort { .. })
        ));
    }

    #[test]
    fn reconnect_updates_controller_addr() {
        let mut cm = cm();
        let now = Instant::now();
        let o = cm.handle_datagram(&pdu("connect_req"), cpu(), now).unwrap();
        assert_eq!(o.notify, vec![(ArState::Connected, None)]);
        cm.handle_datagram(&pdu("write_req"), cpu(), now).unwrap();
        cm.handle_datagram(&pdu("prmend_req"), cpu(), now).unwrap();
        let o = cm
            .handle_datagram(&pdu("appready_res"), cpu_cm(), now)
            .unwrap();
        assert_eq!(o.notify, vec![(ArState::Data, None)]);
        assert_eq!(cm.state(), ArState::Data);

        // Same AR (ARUUID unchanged), new SessionKey, from a new source port and a
        // fresh RPC call (new seq_num — the bench also gets a new activity UUID;
        // seq_num alone is enough here to not collide with the cached responses of
        // the exchange above, which used 0/1/2 on this same activity) — as the CPU
        // does on the bench after it aborts and re-Identifies. ARBlockReq starts at
        // PDU offset 108 (ARUUID, 16 bytes), so SessionKey is at [124..126].
        let mut reconnect = pdu("connect_req");
        reconnect[64] = 3; // seq_num (LE low byte): 0 -> 3, a new RPC call
        reconnect[124..126].copy_from_slice(&5u16.to_be_bytes());
        let reconnect_from = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(172, 16, 2, 100)), 54800);
        let o = cm.handle_datagram(&reconnect, reconnect_from, now).unwrap();
        assert_eq!(
            o.notify,
            vec![
                (ArState::Idle, Some(AbortReason::ControllerReconnect)),
                (ArState::Connected, None),
            ]
        );
        assert_eq!(o.send.len(), 1);
        let (n, _) =
            crate::rpc::NdrResponse::parse(&o.send[0].bytes[80..], crate::rpc::Drep::BIG).unwrap();
        assert!(PnioStatus(n.status).is_ok());

        // A PrmEnd on the reconnected AR (again a fresh RPC call, not a retransmit of
        // the first one already cached above) must place the ApplicationReady call
        // to the controller address captured by the takeover. The reconnect's source
        // IP matches the original controller's, so the address itself is unchanged
        // here — but this exercises the takeover code path (not the stale
        // `was_idle` guard it replaces) that sets `controller_addr`.
        let mut prmend = pdu("prmend_req");
        prmend[64] = 4; // seq_num (LE low byte): 2 -> 4, a new RPC call
                        // A real controller's PrmEnd carries the SessionKey it just negotiated (5),
                        // not the golden capture's original one (2); SessionKey is at the same
                        // [124..126] offset as in ARBlockReq (see the reconnect above).
        prmend[124..126].copy_from_slice(&5u16.to_be_bytes());
        let o = cm.handle_datagram(&prmend, cpu(), now).unwrap();
        assert_eq!(o.send[1].to, cpu_cm());
        // `call_seq` is scoped to the device's fixed `activity_seed`, not to the AR,
        // so it must keep counting up across the reconnect rather than restarting at
        // 0 (which would resend the RPC-CL pair `(activity_seed, 0)` the controller
        // already completed for the first AR). The call is therefore identical to
        // the golden ApplicationReady request everywhere except `seq_num` (bytes
        // [64..68], big-endian on our own outgoing calls, which is 1, not 0) and
        // SessionKey (bytes [124..126]): the ApplicationReady call is built from the
        // AR's own context — the reconnected session (5) — not from the PrmEnd
        // request that triggered it.
        let mut expected = pdu("appready_req");
        expected[124..126].copy_from_slice(&5u16.to_be_bytes());
        assert_eq!(o.send[1].bytes[..64], expected[..64]);
        assert_eq!(o.send[1].bytes[68..124], expected[68..124]);
        assert_eq!(o.send[1].bytes[126..], expected[126..]);
        assert_eq!(
            u32::from_be_bytes(o.send[1].bytes[64..68].try_into().unwrap()),
            1
        );
    }

    #[test]
    fn tick_resends_app_ready_to_controller() {
        let mut cm = cm();
        let now = Instant::now();
        cm.handle_datagram(&pdu("connect_req"), cpu(), now).unwrap();
        cm.handle_datagram(&pdu("prmend_req"), cpu(), now).unwrap();
        let o =
            cm.tick(now + crate::cm::ar::APP_READY_TIMEOUT + std::time::Duration::from_millis(1));
        assert_eq!(o.send[0].bytes, pdu("appready_req"));
        assert_eq!(o.send[0].to, cpu_cm());
    }
}

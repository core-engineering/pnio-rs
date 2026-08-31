//! `AlarmChannel`: pure sender/receiver state machine for one alarm priority
//! (High/Low). One notification in flight at a time; the peer's transport ACK and
//! its content-level AlarmAck are separate steps (spec §5.2). No sockets, no clock:
//! every state transition is driven by `now` passed in by the caller.

use super::rta::{
    build_frame, parse_frame, AlarmNotification, PduType, Priority, RtaBody, RtaData, RtaHeader,
    UserData,
};
use crate::cm::{AbortReason, PnioStatus};
use crate::eth::{EthHeader, MacAddr};
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use thiserror::Error;

/// Fixed size (bytes) of an AlarmNotification block's `block_length` field minus its
/// USI-specific `data`: the 22 fixed fields plus the 2-byte block version that
/// `block_length` counts (spec §5.2.5.2 / `docs/alarm-golden-frames.md`).
const NOTIFICATION_FIXED_LEN: usize = 24;

/// How much longer than `rta_timeout` we wait for the controller's *content-level*
/// Alarm-Ack once its transport ACK has confirmed delivery (spec §5.2). The DATA is
/// on the CPU's side by then, so resending it would achieve nothing and aborting the
/// AR would punish a merely slow application; we wait a generous multiple and then
/// drop the alarm, keeping the AR up.
const ALARM_ACK_TIMEOUT_FACTOR: u32 = 10;

/// Static configuration for one alarm channel (one priority): the AR's alarm
/// references, retry policy, negotiated data-length limit, and the two peers' MACs.
#[derive(Debug, Clone)]
pub struct AlarmChannelConfig {
    /// `LocalAlarmReference` we announced (goes into outgoing `AlarmSrcEndpoint`).
    pub local_ref: u16,
    /// The peer's `LocalAlarmReference` (goes into outgoing `AlarmDstEndpoint`).
    pub remote_ref: u16,
    /// How long to wait for a transport ACK or content AlarmAck before resending.
    pub rta_timeout: Duration,
    /// Number of resends of an unacknowledged DATA before the channel aborts.
    pub rta_retries: u16,
    /// Negotiated `AlarmCRBlockReq.MaxAlarmDataLength`.
    pub max_alarm_data_length: u16,
    /// The controller's MAC (frames from any other source are `UnexpectedRx`).
    pub peer_mac: MacAddr,
    /// Our own MAC (used as the outgoing frame's source).
    pub our_mac: MacAddr,
}

/// One alarm the application wants sent.
#[derive(Debug, Clone, PartialEq)]
pub struct AlarmReq {
    /// Caller-chosen id echoed back in `AlarmAction::Acked` so the caller can match
    /// its own request without the channel knowing anything about its origin.
    pub id: u32,
    /// Which alarm channel (High/Low) to send it on.
    pub priority: Priority,
    /// The AlarmNotification block to send.
    pub notification: AlarmNotification,
}

/// An effect the caller must perform: send bytes on the wire, notify the application
/// an alarm was acknowledged, abort the AR, or just note an unexpected frame.
#[derive(Debug, Clone, PartialEq)]
pub enum AlarmAction {
    /// Send this complete Ethernet frame on the alarm channel.
    Send(Vec<u8>),
    /// The in-flight alarm `id` was acknowledged by the controller with `status`.
    Acked {
        /// The [`AlarmReq::id`] of the acknowledged alarm.
        id: u32,
        /// Result reported by the controller's AlarmAck; `PnioStatus::OK` on success.
        status: PnioStatus,
    },
    /// The AR must be aborted for `reason`.
    Abort(AbortReason),
    /// A frame arrived that this channel could not use (wrong source, malformed,
    /// stray ACK, or unhandled DATA content).
    UnexpectedRx,
}

/// Errors from [`AlarmChannel::enqueue`].
#[derive(Debug, Error, PartialEq)]
pub enum AlarmError {
    /// The notification's block length would exceed the AR's negotiated
    /// `MaxAlarmDataLength`; the alarm was refused before it touched the wire.
    #[error("alarm data {len} bytes exceeds the negotiated {max}")]
    TooLong {
        /// The notification block's would-be length in bytes.
        len: usize,
        /// The AR's negotiated `MaxAlarmDataLength`.
        max: u16,
    },
}

/// Running counters, queryable by the caller for diagnostics/metrics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AlarmStats {
    /// First transmissions of a DATA (not resends).
    pub sent: u64,
    /// Alarms acknowledged by the controller (content-level AlarmAck).
    pub acked: u64,
    /// DATA resends (transport-ACK or AlarmAck timeout).
    pub retries: u64,
    /// Frames counted as `UnexpectedRx` or otherwise ignored.
    pub unexpected_rx: u64,
    /// Times an in-flight alarm exhausted its retries without being acknowledged.
    pub send_failures: u64,
    /// Times a delivered alarm (transport-ACKed) was dropped because the controller
    /// never sent its content-level Alarm-Ack in time. The AR is *not* aborted.
    pub ack_timeouts: u64,
    /// ERR-RTA frames received from the controller.
    pub rx_err_rta: u64,
}

/// One in-flight alarm's state: nothing in flight, waiting for the peer's transport
/// ACK of our DATA, or (having got that) waiting for the peer's content-level
/// AlarmAck.
enum State {
    Idle,
    /// Sent a DATA, waiting for the peer's transport ACK (`RtaBody::Ack` with
    /// `ack_seq == seq`).
    SentData {
        req: AlarmReq,
        seq: u16,
        frame: Vec<u8>,
        deadline: Instant,
    },
    /// Got the transport ACK; waiting for the peer's DATA carrying the content-level
    /// AlarmAck block. `deadline` is `ALARM_ACK_TIMEOUT_FACTOR * rta_timeout` away:
    /// on expiry the alarm is dropped, not resent and not fatal to the AR.
    AwaitAlarmAck {
        req: AlarmReq,
        deadline: Instant,
    },
}

/// The one-alarm-in-flight sender/receiver state machine for one priority channel.
/// Pure: `enqueue`/`on_frame`/`on_tick` take `now` explicitly and never touch a clock
/// or a socket themselves — the device loop drives it.
pub struct AlarmChannel {
    cfg: AlarmChannelConfig,
    state: State,
    queue: VecDeque<AlarmReq>,
    /// `SendSeqNum` for our *next* DATA (spec §5.2: starts `SEQ_INIT`, then
    /// `0, 1, 2, …` wrapping at `0x7FFF`).
    next_send_seq: u16,
    /// `SendSeqNum` of the last DATA we sent (what our ACK-RTA/ERR-RTA report as
    /// their own `SendSeqNum`); `SEQ_NONE` before we ever send one.
    last_sent_seq: u16,
    /// `SendSeqNum` of the last DATA we accepted from the peer (what we report as
    /// `AckSeqNum`); `SEQ_NONE` before we accept one.
    last_rx_seq: u16,
    /// Resend attempts made for the current in-flight alarm (reset on a fresh send).
    attempt: u16,
    stats: AlarmStats,
    /// Per-AR `AlarmSpecifier.sequence` counter (0..=0x7FF), independent of the RTA
    /// transport sequence numbers above.
    specifier_seq: u16,
}

impl AlarmChannel {
    /// A fresh, idle channel for `cfg`: empty queue, no alarm in flight, sequence
    /// counters at their initial values ([`super::rta::SEQ_INIT`] / [`super::rta::SEQ_NONE`]).
    pub fn new(cfg: AlarmChannelConfig) -> Self {
        AlarmChannel {
            cfg,
            state: State::Idle,
            queue: VecDeque::new(),
            next_send_seq: super::rta::SEQ_INIT,
            last_sent_seq: super::rta::SEQ_NONE,
            last_rx_seq: super::rta::SEQ_NONE,
            attempt: 0,
            stats: AlarmStats::default(),
            specifier_seq: 0,
        }
    }

    /// Queues `req`; refuses (before touching the wire) if its notification body
    /// would exceed the negotiated `max_alarm_data_length`. If nothing is currently
    /// in flight, sends it immediately.
    pub fn enqueue(&mut self, req: AlarmReq, now: Instant) -> Result<Vec<AlarmAction>, AlarmError> {
        let len = NOTIFICATION_FIXED_LEN + data_len(&req.notification.data);
        if len > self.cfg.max_alarm_data_length as usize {
            return Err(AlarmError::TooLong {
                len,
                max: self.cfg.max_alarm_data_length,
            });
        }
        self.queue.push_back(req);
        Ok(self.send_next(now))
    }

    /// Feeds one received Ethernet frame (already known to be an alarm frame by the
    /// caller's dispatch, but re-validated here) into the state machine.
    pub fn on_frame(&mut self, frame: &[u8], now: Instant) -> Vec<AlarmAction> {
        let pdu = match parse_frame(frame) {
            Ok(p) => p,
            Err(_) => {
                self.stats.unexpected_rx += 1;
                return vec![AlarmAction::UnexpectedRx];
            }
        };
        let src = match EthHeader::parse(frame) {
            Ok((h, _)) => h.src,
            Err(_) => {
                self.stats.unexpected_rx += 1;
                return vec![AlarmAction::UnexpectedRx];
            }
        };
        if src != self.cfg.peer_mac {
            self.stats.unexpected_rx += 1;
            return vec![AlarmAction::UnexpectedRx];
        }
        match pdu.body {
            RtaBody::Ack => self.on_transport_ack(pdu.header.ack_seq, now),
            RtaBody::Data(data) => self.on_data(pdu.header, pdu.priority, data, now),
            RtaBody::Err(status) => self.on_err_rta(status),
            RtaBody::Nack => {
                self.stats.unexpected_rx += 1;
                vec![AlarmAction::UnexpectedRx]
            }
        }
    }

    /// Called by the caller when `now >= next_deadline()`. In `SentData` (no transport
    /// ACK yet) resends the in-flight DATA up to `rta_retries` times, then aborts the
    /// AR. In `AwaitAlarmAck` (delivery already confirmed) drops the alarm and moves
    /// on to the queue (deadline = `ALARM_ACK_TIMEOUT_FACTOR` × the RTA timeout).
    pub fn on_tick(&mut self, now: Instant) -> Vec<AlarmAction> {
        let awaiting_alarm_ack = match &self.state {
            State::SentData { deadline, .. } => {
                if now < *deadline {
                    return vec![];
                }
                false
            }
            State::AwaitAlarmAck { deadline, .. } => {
                if now < *deadline {
                    return vec![];
                }
                true
            }
            State::Idle => return vec![],
        };
        if awaiting_alarm_ack {
            self.drop_unacknowledged(now)
        } else {
            self.retry_or_abort(now)
        }
    }

    /// The next time the caller must call `on_tick` (the in-flight alarm's deadline),
    /// or `None` if nothing is in flight.
    pub fn next_deadline(&self) -> Option<Instant> {
        match &self.state {
            State::SentData { deadline, .. } | State::AwaitAlarmAck { deadline, .. } => {
                Some(*deadline)
            }
            State::Idle => None,
        }
    }

    /// Builds an ERR-RTA frame (Low priority, current `SendSeqNum`/`AckSeqNum`
    /// counters) for the caller to send. Does not touch the state machine or queue.
    pub fn err_rta(&mut self, status: PnioStatus) -> Vec<u8> {
        let pdu = super::rta::RtaPdu {
            priority: Priority::Low,
            header: RtaHeader {
                dst_ref: self.cfg.remote_ref,
                src_ref: self.cfg.local_ref,
                pdu_type: PduType::Err,
                tack: false,
                send_seq: self.last_sent_seq,
                ack_seq: self.last_rx_seq,
            },
            body: RtaBody::Err(status),
        };
        build_frame(self.cfg.peer_mac, self.cfg.our_mac, &pdu)
    }

    /// The `id` of the alarm currently in flight (sent, awaiting either ACK), if any.
    pub fn in_flight(&self) -> Option<u32> {
        match &self.state {
            State::SentData { req, .. } | State::AwaitAlarmAck { req, .. } => Some(req.id),
            State::Idle => None,
        }
    }

    /// Number of alarms queued behind the in-flight one (not counting it).
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// A plain-value copy of this channel's running counters.
    pub fn stats(&self) -> AlarmStats {
        self.stats
    }

    /// Next `AlarmSpecifier.sequence` value (0, 1, 2, … wrapping at `0x7FF`): the
    /// device fills this in before enqueueing a notification.
    pub fn next_specifier_sequence(&mut self) -> u16 {
        let v = self.specifier_seq;
        self.specifier_seq = (self.specifier_seq + 1) & 0x07FF;
        v
    }

    // -- internals -----------------------------------------------------------------

    /// If idle and the queue is non-empty, pops and sends the next request.
    fn send_next(&mut self, now: Instant) -> Vec<AlarmAction> {
        if !matches!(self.state, State::Idle) {
            return vec![];
        }
        let Some(req) = self.queue.pop_front() else {
            return vec![];
        };
        let seq = self.next_send_seq;
        let frame = build_data_frame(&self.cfg, &req, seq, self.last_rx_seq);
        self.last_sent_seq = seq;
        self.next_send_seq = if seq == super::rta::SEQ_INIT {
            0
        } else {
            (seq + 1) & 0x7FFF
        };
        self.attempt = 0;
        self.stats.sent += 1;
        let deadline = now + self.cfg.rta_timeout;
        let out = vec![AlarmAction::Send(frame.clone())];
        self.state = State::SentData {
            req,
            seq,
            frame,
            deadline,
        };
        out
    }

    fn on_transport_ack(&mut self, ack_seq: u16, now: Instant) -> Vec<AlarmAction> {
        if let State::SentData { seq, .. } = &self.state {
            if ack_seq == *seq {
                // The DATA is delivered: drop the frame we were holding for resends
                // and wait (much longer) for the content-level Alarm-Ack instead.
                let old = std::mem::replace(&mut self.state, State::Idle);
                if let State::SentData { req, .. } = old {
                    self.state = State::AwaitAlarmAck {
                        req,
                        deadline: now + self.cfg.rta_timeout * ALARM_ACK_TIMEOUT_FACTOR,
                    };
                }
                return vec![];
            }
        }
        self.stats.unexpected_rx += 1;
        vec![]
    }

    fn on_data(
        &mut self,
        header: RtaHeader,
        priority: Priority,
        data: RtaData,
        now: Instant,
    ) -> Vec<AlarmAction> {
        if header.send_seq == self.last_rx_seq {
            // Peer never got our ACK-RTA (or is otherwise retransmitting): re-ack the
            // same DATA without reprocessing its content.
            return vec![AlarmAction::Send(
                self.build_ack_rta(priority, header.send_seq),
            )];
        }
        self.last_rx_seq = header.send_seq;
        let mut actions = vec![AlarmAction::Send(
            self.build_ack_rta(priority, header.send_seq),
        )];
        match data {
            RtaData::Ack(a) => {
                // `SentData` counts as well as `AwaitAlarmAck`: an Alarm-Ack can
                // overtake the transport ACK of the DATA it answers (different
                // frames, and the CPU may coalesce them). Its arrival proves the
                // DATA was delivered, so it implicitly acknowledges the transport.
                let matches_in_flight = matches!(
                    &self.state,
                    State::SentData { req, .. } | State::AwaitAlarmAck { req, .. }
                        if req.notification.alarm_type == a.alarm_type
                            && req.notification.slot == a.slot
                            && req.notification.subslot == a.subslot
                );
                if matches_in_flight {
                    let old = std::mem::replace(&mut self.state, State::Idle);
                    if let State::SentData { req, .. } | State::AwaitAlarmAck { req, .. } = old {
                        self.stats.acked += 1;
                        actions.push(AlarmAction::Acked {
                            id: req.id,
                            status: a.status,
                        });
                    }
                    actions.extend(self.send_next(now));
                } else {
                    self.stats.unexpected_rx += 1;
                    actions.push(AlarmAction::UnexpectedRx);
                }
            }
            RtaData::Notification(_) | RtaData::Unknown { .. } => {
                self.stats.unexpected_rx += 1;
                actions.push(AlarmAction::UnexpectedRx);
            }
        }
        actions
    }

    fn on_err_rta(&mut self, status: PnioStatus) -> Vec<AlarmAction> {
        self.stats.rx_err_rta += 1;
        self.state = State::Idle;
        self.queue.clear();
        vec![AlarmAction::Abort(AbortReason::ControllerErrRta(status))]
    }

    /// `SentData` past its deadline: the peer never acknowledged the transport, so
    /// resend the exact same DATA frame (up to `rta_retries` times), then abort.
    fn retry_or_abort(&mut self, now: Instant) -> Vec<AlarmAction> {
        if self.attempt < self.cfg.rta_retries {
            self.attempt += 1;
            self.stats.retries += 1;
            let (req, seq, frame) = match std::mem::replace(&mut self.state, State::Idle) {
                State::SentData {
                    req, seq, frame, ..
                } => (req, seq, frame),
                State::Idle | State::AwaitAlarmAck { .. } => {
                    unreachable!("on_tick only routes an expired SentData here")
                }
            };
            let out = vec![AlarmAction::Send(frame.clone())];
            self.state = State::SentData {
                req,
                seq,
                frame,
                deadline: now + self.cfg.rta_timeout,
            };
            out
        } else {
            self.stats.send_failures += 1;
            self.state = State::Idle;
            self.queue.clear();
            vec![AlarmAction::Abort(AbortReason::AlarmSendFailed)]
        }
    }

    /// `AwaitAlarmAck` past its (long) deadline: the DATA *was* delivered — the peer
    /// ACKed it at the transport level — so resending it would only duplicate it, and
    /// aborting the AR would take the device down over a slow controller application.
    /// Drop the alarm, log, count it, and let the queue continue. The controller keeps
    /// whatever it already learned from the notification.
    fn drop_unacknowledged(&mut self, now: Instant) -> Vec<AlarmAction> {
        let State::AwaitAlarmAck { req, .. } = std::mem::replace(&mut self.state, State::Idle)
        else {
            unreachable!("on_tick only routes an expired AwaitAlarmAck here")
        };
        self.stats.ack_timeouts += 1;
        log::warn!(
            "alarm id {} ({:?} on slot {} subslot {}) was delivered but the controller sent no Alarm-Ack within {:?}: dropping it, the AR stays up",
            req.id,
            req.notification.alarm_type,
            req.notification.slot,
            req.notification.subslot,
            self.cfg.rta_timeout * ALARM_ACK_TIMEOUT_FACTOR,
        );
        self.send_next(now)
    }

    fn build_ack_rta(&self, priority: Priority, received_seq: u16) -> Vec<u8> {
        let pdu = super::rta::RtaPdu {
            priority,
            header: RtaHeader {
                dst_ref: self.cfg.remote_ref,
                src_ref: self.cfg.local_ref,
                pdu_type: PduType::Ack,
                tack: false,
                send_seq: self.last_sent_seq,
                ack_seq: received_seq,
            },
            body: RtaBody::Ack,
        };
        build_frame(self.cfg.peer_mac, self.cfg.our_mac, &pdu)
    }
}

fn build_data_frame(cfg: &AlarmChannelConfig, req: &AlarmReq, seq: u16, ack_seq: u16) -> Vec<u8> {
    let pdu = super::rta::RtaPdu {
        priority: req.priority,
        header: RtaHeader {
            dst_ref: cfg.remote_ref,
            src_ref: cfg.local_ref,
            pdu_type: PduType::Data,
            tack: true,
            send_seq: seq,
            ack_seq,
        },
        body: RtaBody::Data(RtaData::Notification(req.notification.clone())),
    };
    build_frame(cfg.peer_mac, cfg.our_mac, &pdu)
}

/// USI-specific data length in bytes, as it will appear on the wire.
fn data_len(data: &UserData) -> usize {
    match data {
        UserData::Raw(v) => v.len(),
        UserData::Channel(_) => 6,
        UserData::ExtChannel(_) => 12,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alarm::rta::*;
    use crate::cm::PnioStatus;
    use crate::testutil::golden_alarm;
    use std::time::{Duration, Instant};

    const DEV: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);
    const CPU: MacAddr = MacAddr([0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3c]);

    fn cfg() -> AlarmChannelConfig {
        AlarmChannelConfig {
            local_ref: 0,
            remote_ref: 0,
            rta_timeout: Duration::from_millis(100),
            rta_retries: 3,
            max_alarm_data_length: 256,
            peer_mac: CPU,
            our_mac: DEV,
        }
    }
    fn process_req(id: u32) -> AlarmReq {
        AlarmReq {
            id,
            priority: Priority::High,
            notification: AlarmNotification {
                alarm_type: AlarmType::Process,
                api: 0,
                slot: 1,
                subslot: 1,
                module_ident: 0x30,
                submodule_ident: 0x130,
                specifier: AlarmSpecifier::default(),
                usi: 0x0010,
                data: UserData::Raw(vec![1]),
            },
        }
    }
    fn sends(actions: &[AlarmAction]) -> Vec<Vec<u8>> {
        actions
            .iter()
            .filter_map(|a| {
                if let AlarmAction::Send(f) = a {
                    Some(f.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    #[test]
    fn full_handshake_reproduces_the_process_alarm_goldens() {
        let t0 = Instant::now();
        let mut ch = AlarmChannel::new(cfg());
        let out = ch.enqueue(process_req(7), t0).unwrap();
        assert_eq!(sends(&out), vec![golden_alarm("alarm_process_notif")]);
        assert_eq!(ch.in_flight(), Some(7));
        let out = ch.on_frame(&golden_alarm("alarm_ack_rta_high_cpu"), t0);
        assert!(out.is_empty(), "transport ack produces nothing to send");
        let out = ch.on_frame(&golden_alarm("alarm_ack_high_cpu"), t0);
        assert_eq!(sends(&out), vec![golden_alarm("alarm_ack_rta_high_dev")]);
        assert!(out.contains(&AlarmAction::Acked {
            id: 7,
            status: PnioStatus::OK
        }));
        assert_eq!(ch.in_flight(), None);
        assert_eq!(ch.stats().sent, 1);
        assert_eq!(ch.stats().acked, 1);
    }

    #[test]
    fn queue_is_fifo_and_one_in_flight() {
        let t0 = Instant::now();
        let mut ch = AlarmChannel::new(cfg());
        let a = ch.enqueue(process_req(1), t0).unwrap();
        let b = ch.enqueue(process_req(2), t0).unwrap();
        assert_eq!(sends(&a).len(), 1);
        assert!(sends(&b).is_empty());
        assert_eq!(ch.queued(), 1);
        ch.on_frame(&golden_alarm("alarm_ack_rta_high_cpu"), t0);
        let out = ch.on_frame(&golden_alarm("alarm_ack_high_cpu"), t0);
        // our ACK-RTA for the CPU's ack, then the second notification (send_seq 0)
        let s = sends(&out);
        assert_eq!(s.len(), 2);
        let second = parse_frame(&s[1]).unwrap();
        assert_eq!(
            (second.header.send_seq, second.header.ack_seq),
            (0x0000, 0xFFFF)
        );
        assert_eq!(ch.in_flight(), Some(2));
    }

    #[test]
    fn retries_then_aborts_when_never_acked() {
        let t0 = Instant::now();
        let mut ch = AlarmChannel::new(cfg());
        ch.enqueue(process_req(1), t0).unwrap();
        let mut resends = 0;
        let mut t = t0;
        let mut aborted = None;
        for _ in 0..5 {
            t += Duration::from_millis(101);
            let out = ch.on_tick(t);
            resends += sends(&out).len();
            if let Some(AlarmAction::Abort(r)) =
                out.iter().find(|a| matches!(a, AlarmAction::Abort(_)))
            {
                aborted = Some(*r);
                break;
            }
        }
        assert_eq!(resends, 3);
        assert_eq!(aborted, Some(crate::cm::AbortReason::AlarmSendFailed));
        assert_eq!(ch.stats().retries, 3);
        assert_eq!(ch.stats().send_failures, 1);
        assert_eq!(ch.in_flight(), None);
    }

    #[test]
    fn alarm_ack_overtaking_the_transport_ack_is_accepted() {
        let t0 = Instant::now();
        let mut ch = AlarmChannel::new(cfg());
        ch.enqueue(process_req(7), t0).unwrap();
        assert_eq!(ch.in_flight(), Some(7));

        // No `alarm_ack_rta_high_cpu` first: the content-level Alarm-Ack arrives
        // while we are still in SentData. It proves the DATA was delivered, so it
        // implicitly acknowledges the transport.
        let out = ch.on_frame(&golden_alarm("alarm_ack_high_cpu"), t0);
        assert_eq!(sends(&out), vec![golden_alarm("alarm_ack_rta_high_dev")]);
        assert!(out.contains(&AlarmAction::Acked {
            id: 7,
            status: PnioStatus::OK
        }));
        assert_eq!(ch.in_flight(), None);
        assert_eq!(ch.queued(), 0);
        assert_eq!(ch.stats().acked, 1);
        assert_eq!(ch.stats().unexpected_rx, 0);
        assert_eq!(ch.next_deadline(), None);
    }

    #[test]
    fn duplicate_data_is_re_acked_but_not_reprocessed() {
        let t0 = Instant::now();
        let mut ch = AlarmChannel::new(cfg());
        ch.enqueue(process_req(1), t0).unwrap();
        ch.on_frame(&golden_alarm("alarm_ack_rta_high_cpu"), t0);
        let first = ch.on_frame(&golden_alarm("alarm_ack_high_cpu"), t0);
        let again = ch.on_frame(&golden_alarm("alarm_ack_high_cpu"), t0);
        assert_eq!(sends(&again), vec![golden_alarm("alarm_ack_rta_high_dev")]);
        assert!(!again.iter().any(|a| matches!(a, AlarmAction::Acked { .. })));
        assert!(first.iter().any(|a| matches!(a, AlarmAction::Acked { .. })));
    }

    #[test]
    fn controller_err_rta_aborts() {
        let t0 = Instant::now();
        let mut ch = AlarmChannel::new(cfg());
        ch.enqueue(process_req(1), t0).unwrap();
        let out = ch.on_frame(&golden_alarm("alarm_err_rta_cpu_removed"), t0);
        assert_eq!(
            out,
            vec![AlarmAction::Abort(
                crate::cm::AbortReason::ControllerErrRta(PnioStatus::new(0xCF, 0x81, 0xFD, 0x11))
            )]
        );
        assert_eq!(ch.in_flight(), None);
        assert_eq!(ch.queued(), 0);
    }

    #[test]
    fn err_rta_out_uses_current_counters_and_low_priority() {
        let mut ch = AlarmChannel::new(cfg());
        let f = ch.err_rta(PnioStatus::rta_abort(PnioStatus::RTA_ABORT_AR_REMOVED));
        let pdu = parse_frame(&f).unwrap();
        assert_eq!(pdu.priority, Priority::Low);
        assert_eq!(
            (pdu.header.send_seq, pdu.header.ack_seq),
            (SEQ_NONE, SEQ_NONE)
        );
        assert_eq!(
            pdu.body,
            RtaBody::Err(PnioStatus::new(0xCF, 0x81, 0xFD, 17))
        );
    }

    #[test]
    fn too_long_is_refused_before_the_wire() {
        let mut c = cfg();
        c.max_alarm_data_length = 30;
        let mut ch = AlarmChannel::new(c);
        let mut r = process_req(1);
        r.notification.data = UserData::Raw(vec![0; 40]);
        assert!(matches!(
            ch.enqueue(r, Instant::now()),
            Err(AlarmError::TooLong { .. })
        ));
    }

    #[test]
    fn frames_from_another_mac_and_garbage_are_unexpected() {
        let t0 = Instant::now();
        let mut ch = AlarmChannel::new(cfg());
        let mut f = golden_alarm("alarm_ack_high_cpu");
        f[6] ^= 0xFF;
        assert_eq!(ch.on_frame(&f, t0), vec![AlarmAction::UnexpectedRx]);
        assert_eq!(ch.on_frame(&[0u8; 10], t0), vec![AlarmAction::UnexpectedRx]);
        assert_eq!(ch.stats().unexpected_rx, 2);
    }

    #[test]
    fn await_alarm_ack_timeout_drops_the_alarm_and_keeps_the_ar() {
        let t0 = Instant::now();
        let mut ch = AlarmChannel::new(cfg());
        let out = ch.enqueue(process_req(1), t0).unwrap();
        assert_eq!(sends(&out), vec![golden_alarm("alarm_process_notif")]);
        // A second alarm queued behind it, to prove the queue continues.
        assert!(sends(&ch.enqueue(process_req(2), t0).unwrap()).is_empty());

        let out = ch.on_frame(&golden_alarm("alarm_ack_rta_high_cpu"), t0);
        assert!(out.is_empty(), "transport ack produces nothing to send");
        assert_eq!(ch.in_flight(), Some(1));

        // The transport ACK confirmed delivery, so the AwaitAlarmAck deadline is
        // ALARM_ACK_TIMEOUT_FACTOR (10) x rta_timeout away, not one rta_timeout:
        // a plain rta_timeout tick must do nothing at all.
        let t = t0 + Duration::from_millis(101);
        assert!(
            ch.on_tick(t).is_empty(),
            "one rta_timeout is not the deadline"
        );
        assert_eq!(ch.in_flight(), Some(1));
        assert_eq!(ch.stats().retries, 0);

        // Past 10 x rta_timeout the alarm is dropped — no resend, no Abort — and the
        // next queued alarm goes out.
        let t = t0 + Duration::from_millis(1001);
        let out = ch.on_tick(t);
        assert!(
            !out.iter().any(|a| matches!(a, AlarmAction::Abort(_))),
            "a slow controller application must not take the AR down: {out:?}"
        );
        let s = sends(&out);
        assert_eq!(s.len(), 1, "exactly the next queued alarm: {out:?}");
        let next = parse_frame(&s[0]).unwrap();
        assert_eq!(next.header.send_seq, 0x0000, "a fresh DATA, not a resend");
        assert_eq!(ch.in_flight(), Some(2));
        assert_eq!(ch.queued(), 0);
        assert_eq!(ch.stats().ack_timeouts, 1);
        assert_eq!(ch.stats().retries, 0);
        assert_eq!(ch.stats().send_failures, 0);
    }

    #[test]
    fn transport_ack_while_idle_is_counted_and_produces_nothing() {
        let t0 = Instant::now();
        let mut ch = AlarmChannel::new(cfg());
        assert_eq!(ch.in_flight(), None);
        let out = ch.on_frame(&golden_alarm("alarm_ack_rta_high_cpu"), t0);
        assert!(out.is_empty());
        assert_eq!(ch.stats().unexpected_rx, 1);
    }

    #[test]
    fn mismatched_alarm_ack_while_awaiting_is_unexpected_but_still_transport_acked() {
        let t0 = Instant::now();
        let mut ch = AlarmChannel::new(cfg());
        ch.enqueue(process_req(1), t0).unwrap();
        ch.on_frame(&golden_alarm("alarm_ack_rta_high_cpu"), t0);
        assert_eq!(ch.in_flight(), Some(1));

        // alarm_diag_ack_cpu carries a Diagnosis AlarmAck; the in-flight alarm is
        // Process, so it cannot satisfy AwaitAlarmAck — but it is still a new,
        // valid DATA that must be ack'd at the transport level.
        let out = ch.on_frame(&golden_alarm("alarm_diag_ack_cpu"), t0);
        assert!(
            out.iter().any(|a| matches!(a, AlarmAction::Send(_))),
            "the ACK-RTA is still sent: {out:?}"
        );
        assert!(out.contains(&AlarmAction::UnexpectedRx));
        assert!(!out.iter().any(|a| matches!(a, AlarmAction::Acked { .. })));
        assert_eq!(
            ch.in_flight(),
            Some(1),
            "AwaitAlarmAck must not be disturbed"
        );
    }

    #[test]
    fn notification_from_the_controller_is_transport_acked_but_unexpected() {
        let t0 = Instant::now();
        let mut ch = AlarmChannel::new(cfg());
        ch.enqueue(process_req(1), t0).unwrap();
        ch.on_frame(&golden_alarm("alarm_ack_rta_high_cpu"), t0);
        assert_eq!(ch.in_flight(), Some(1));

        // alarm_process_notif is captured device(DEV) -> CPU; swap src/dst so it
        // looks like the controller sending *us* a Notification instead.
        let mut f = golden_alarm("alarm_process_notif");
        let (dst, src) = (f[0..6].to_vec(), f[6..12].to_vec());
        f[0..6].copy_from_slice(&src);
        f[6..12].copy_from_slice(&dst);
        assert_eq!(&f[0..6], &DEV.0[..], "dst is now our own MAC");
        assert_eq!(&f[6..12], &CPU.0[..], "src is now the controller's MAC");
        let out = ch.on_frame(&f, t0);
        assert!(
            out.iter().any(|a| matches!(a, AlarmAction::Send(_))),
            "the ACK-RTA is still sent: {out:?}"
        );
        assert!(out.contains(&AlarmAction::UnexpectedRx));
        assert_eq!(ch.in_flight(), Some(1));
    }
}

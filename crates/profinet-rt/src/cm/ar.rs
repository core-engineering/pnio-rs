//! `Ar`: the pure Application Relationship (AR) state machine (IEC 61158-6-10 §6).
//!
//! No I/O, no clock of its own: `on()` takes an `Event` and the current time and
//! returns the `Action`s to perform. `next_deadline()` is queried by the caller (the
//! `device` loop) instead of the state machine pushing timer-set actions — one less
//! action type, same behaviour: the loop sleeps until the deadline and then feeds a
//! `Tick`.
//!
//! State progression: `Idle` --Connect--> `Connected` --Write*--> `Connected`
//! --PrmEnd--> `AppReadySent` --AppReadyRsp(ok)--> `Data`. Any Release or abort
//! condition (activity timeout, ApplicationReady failure/rejection, or an external
//! reason) returns to `Idle`, dropping the AR context and clearing both timers.

use super::connect::{build_connect_res, validate, ArParams, ConnectReq};
use super::control::{app_ready_req, prm_end_done, release_done, ControlBlock};
use super::model::DeviceModel;
use super::status::PnioStatus;
use super::write::{build_write_res, Record, WriteReq};
use std::time::{Duration, Instant};

/// ApplicationReady request retry timeout: how long the device waits for the
/// controller's `IODControlRes(ApplicationReady)` before retrying.
pub const APP_READY_TIMEOUT: Duration = Duration::from_secs(1);
/// Maximum number of ApplicationReady requests sent (the initial one plus retries)
/// before the AR is aborted.
pub const APP_READY_MAX_ATTEMPTS: u8 = 3;
/// Unit of the AR's `activity_timeout_factor` (from the Connect request's
/// ARBlockReq), IEC 61158-6-10 §4.6.1.1: the activity deadline is
/// `now + activity_timeout_factor * ACTIVITY_TIMEOUT_UNIT`.
pub const ACTIVITY_TIMEOUT_UNIT: Duration = Duration::from_millis(100);

/// The AR's lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArState {
    Idle,
    Connected,
    AppReadySent,
    Data,
}

/// Why an AR was aborted back to `Idle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortReason {
    /// The controller sent a Release request.
    ControllerRelease,
    /// `APP_READY_MAX_ATTEMPTS` ApplicationReady requests all timed out.
    AppReadyFailed,
    /// The controller answered ApplicationReady with a non-OK status.
    AppReadyRejected(PnioStatus),
    /// No Write/PrmEnd activity within `activity_timeout_factor * ACTIVITY_TIMEOUT_UNIT`.
    ActivityTimeout,
    /// Any other externally-triggered abort (e.g. link down), free-form reason.
    External(&'static str),
}

/// An input to the AR state machine. Carries the already-parsed request so `Ar`
/// itself does no codec work.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    ConnectReq(ConnectReq),
    WriteReq(WriteReq),
    PrmEndReq(ControlBlock),
    ReleaseReq(ControlBlock),
    AppReadyRsp { status: PnioStatus },
    Tick,
    Abort(AbortReason),
}

impl Event {
    fn name(&self) -> &'static str {
        match self {
            Event::ConnectReq(_) => "ConnectReq",
            Event::WriteReq(_) => "WriteReq",
            Event::PrmEndReq(_) => "PrmEndReq",
            Event::ReleaseReq(_) => "ReleaseReq",
            Event::AppReadyRsp { .. } => "AppReadyRsp",
            Event::Tick => "Tick",
            Event::Abort(_) => "Abort",
        }
    }
}

/// An effect the caller must perform: send an RPC response, call out to the
/// controller (ApplicationReady request), or notify observers of a state change.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Answer the request that triggered this event with these PNIO blocks (empty
    /// on rejection/wrong-state).
    Respond { status: PnioStatus, blocks: Vec<u8> },
    /// Call the controller (ApplicationReady request PNIO blocks).
    CallController { blocks: Vec<u8> },
    /// The AR changed state; `reason` is set only when the new state is `Idle` via
    /// an abort.
    Notify {
        state: ArState,
        reason: Option<AbortReason>,
    },
}

/// The established AR's context: the negotiated parameters, the parameter-write
/// records accumulated so far, when the AR reached `Connected`, and the cached
/// Connect response bytes (so a duplicate Connect resend is byte-identical).
#[derive(Debug, Clone, PartialEq)]
pub struct ArContext {
    pub params: ArParams,
    pub records: Vec<Record>,
    pub connected_at: Instant,
    connect_res: Vec<u8>,
}

/// The pure AR state machine: no I/O, no clock of its own. `on()` is the only way
/// to drive it; `next_deadline()` tells the caller when to feed a `Tick`.
pub struct Ar {
    model: DeviceModel,
    state: ArState,
    ctx: Option<ArContext>,
    app_ready_attempts: u8,
    app_ready_deadline: Option<Instant>,
    activity_deadline: Option<Instant>,
}

impl Ar {
    pub fn new(model: DeviceModel) -> Ar {
        Ar {
            model,
            state: ArState::Idle,
            ctx: None,
            app_ready_attempts: 0,
            app_ready_deadline: None,
            activity_deadline: None,
        }
    }

    pub fn state(&self) -> ArState {
        self.state
    }

    pub fn context(&self) -> Option<&ArContext> {
        self.ctx.as_ref()
    }

    /// The earliest of the activity and ApplicationReady deadlines, if any is
    /// armed. The caller (`device` loop) sleeps until this instant and then feeds
    /// `Event::Tick`.
    pub fn next_deadline(&self) -> Option<Instant> {
        match (self.activity_deadline, self.app_ready_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    /// Drive the state machine with one event, returning the actions to perform.
    pub fn on(&mut self, ev: Event, now: Instant) -> Vec<Action> {
        let prev = self.state;
        let event_name = ev.name();

        let actions = if let Event::Abort(reason) = ev {
            if self.state == ArState::Idle {
                Vec::new()
            } else {
                vec![self.abort(reason)]
            }
        } else {
            match ev {
                Event::ConnectReq(req) => self.handle_connect(req, now),
                Event::WriteReq(req) => self.handle_write(req, now),
                Event::PrmEndReq(req) => self.handle_prm_end(req, now),
                Event::ReleaseReq(req) => self.handle_release(req),
                Event::AppReadyRsp { status } => self.handle_app_ready_rsp(status),
                Event::Tick => self.handle_tick(now),
                Event::Abort(_) => unreachable!("handled above"),
            }
        };

        let aborted = actions.iter().any(|a| {
            matches!(
                a,
                Action::Notify {
                    reason: Some(_),
                    ..
                }
            )
        });
        if aborted {
            log::warn!("AR {prev:?} --{event_name}--> {:?} (abort)", self.state);
        } else {
            log::info!("AR {prev:?} --{event_name}--> {:?}", self.state);
        }
        actions
    }

    fn handle_connect(&mut self, req: ConnectReq, now: Instant) -> Vec<Action> {
        match self.state {
            ArState::Idle => match validate(&req, &self.model) {
                Ok(params) => {
                    let connect_res = build_connect_res(&params, &self.model);
                    let activity_deadline =
                        now + ACTIVITY_TIMEOUT_UNIT * params.activity_timeout_factor as u32;
                    self.ctx = Some(ArContext {
                        params,
                        records: Vec::new(),
                        connected_at: now,
                        connect_res: connect_res.clone(),
                    });
                    self.state = ArState::Connected;
                    self.activity_deadline = Some(activity_deadline);
                    vec![
                        Action::Respond {
                            status: PnioStatus::OK,
                            blocks: connect_res,
                        },
                        Action::Notify {
                            state: ArState::Connected,
                            reason: None,
                        },
                    ]
                }
                Err(status) => vec![Action::Respond {
                    status,
                    blocks: Vec::new(),
                }],
            },
            ArState::Connected | ArState::AppReadySent | ArState::Data => {
                let ctx = self.ctx.as_ref().expect("ctx present outside Idle");
                if req.ar.ar_uuid == ctx.params.ar_uuid {
                    vec![Action::Respond {
                        status: PnioStatus::OK,
                        blocks: ctx.connect_res.clone(),
                    }]
                } else {
                    vec![Action::Respond {
                        status: PnioStatus::connect_ar_already_exists(),
                        blocks: Vec::new(),
                    }]
                }
            }
        }
    }

    fn handle_write(&mut self, req: WriteReq, now: Instant) -> Vec<Action> {
        match self.state {
            ArState::Idle => vec![wrong_state()],
            // Activity timer is armed on Connect and re-armed on Write only while
            // still in Connected; once PrmEnd has started the ApplicationReady
            // phase (AppReadySent) or the AR is in Data, a Write stores records
            // and touches no timer.
            ArState::Connected => {
                let blocks = build_write_res(&req);
                let factor = self
                    .ctx
                    .as_ref()
                    .expect("ctx present outside Idle")
                    .params
                    .activity_timeout_factor;
                let ctx = self.ctx.as_mut().expect("ctx present outside Idle");
                ctx.records.extend(req.records);
                self.activity_deadline = Some(now + ACTIVITY_TIMEOUT_UNIT * factor as u32);
                vec![Action::Respond {
                    status: PnioStatus::OK,
                    blocks,
                }]
            }
            ArState::AppReadySent | ArState::Data => {
                let blocks = build_write_res(&req);
                let ctx = self.ctx.as_mut().expect("ctx present outside Idle");
                ctx.records.extend(req.records);
                vec![Action::Respond {
                    status: PnioStatus::OK,
                    blocks,
                }]
            }
        }
    }

    fn handle_prm_end(&mut self, req: ControlBlock, now: Instant) -> Vec<Action> {
        match self.state {
            ArState::Idle => vec![wrong_state()],
            ArState::Connected => {
                let mut respond = Vec::new();
                prm_end_done(&req).write(&mut respond);
                let mut call = Vec::new();
                app_ready_req(req.ar_uuid, req.session_key).write(&mut call);
                self.state = ArState::AppReadySent;
                self.app_ready_attempts = 1;
                self.app_ready_deadline = Some(now + APP_READY_TIMEOUT);
                self.activity_deadline = None;
                vec![
                    Action::Respond {
                        status: PnioStatus::OK,
                        blocks: respond,
                    },
                    Action::CallController { blocks: call },
                ]
            }
            ArState::AppReadySent | ArState::Data => {
                let mut respond = Vec::new();
                prm_end_done(&req).write(&mut respond);
                vec![Action::Respond {
                    status: PnioStatus::OK,
                    blocks: respond,
                }]
            }
        }
    }

    fn handle_release(&mut self, req: ControlBlock) -> Vec<Action> {
        let mut respond = Vec::new();
        release_done(&req).write(&mut respond);
        let mut actions = vec![Action::Respond {
            status: PnioStatus::OK,
            blocks: respond,
        }];
        if self.state != ArState::Idle {
            actions.push(self.abort(AbortReason::ControllerRelease));
        }
        actions
    }

    fn handle_app_ready_rsp(&mut self, status: PnioStatus) -> Vec<Action> {
        if self.state != ArState::AppReadySent {
            return Vec::new(); // ignored outside AppReadySent
        }
        if status.is_ok() {
            self.state = ArState::Data;
            self.app_ready_deadline = None;
            self.activity_deadline = None;
            self.app_ready_attempts = 0;
            vec![Action::Notify {
                state: ArState::Data,
                reason: None,
            }]
        } else {
            vec![self.abort(AbortReason::AppReadyRejected(status))]
        }
    }

    fn handle_tick(&mut self, now: Instant) -> Vec<Action> {
        match self.state {
            ArState::Connected => match self.activity_deadline {
                Some(deadline) if now >= deadline => {
                    vec![self.abort(AbortReason::ActivityTimeout)]
                }
                _ => Vec::new(),
            },
            ArState::AppReadySent => match self.app_ready_deadline {
                Some(deadline) if now >= deadline => {
                    if self.app_ready_attempts < APP_READY_MAX_ATTEMPTS {
                        self.app_ready_attempts += 1;
                        self.app_ready_deadline = Some(now + APP_READY_TIMEOUT);
                        let ctx = self.ctx.as_ref().expect("ctx present in AppReadySent");
                        let mut call = Vec::new();
                        app_ready_req(ctx.params.ar_uuid, ctx.params.session_key).write(&mut call);
                        vec![Action::CallController { blocks: call }]
                    } else {
                        vec![self.abort(AbortReason::AppReadyFailed)]
                    }
                }
                _ => Vec::new(),
            },
            ArState::Idle | ArState::Data => Vec::new(),
        }
    }

    /// Clear the AR context and both timers, return to `Idle`, and produce the
    /// `Notify` action reporting the abort reason.
    fn abort(&mut self, reason: AbortReason) -> Action {
        self.ctx = None;
        self.app_ready_deadline = None;
        self.activity_deadline = None;
        self.app_ready_attempts = 0;
        self.state = ArState::Idle;
        Action::Notify {
            state: ArState::Idle,
            reason: Some(reason),
        }
    }
}

fn wrong_state() -> Action {
    Action::Respond {
        status: PnioStatus::control_wrong_state(),
        blocks: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cm::block::ty;
    use crate::cm::connect::ConnectReq;
    use crate::cm::control::{cmd, ControlBlock};
    use crate::cm::model::DeviceModel;
    use crate::cm::write::WriteReq;
    use crate::eth::MacAddr;
    use crate::testutil::golden;
    use std::time::{Duration, Instant};

    const BLOCKS: usize = 142;
    const MAC: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);

    fn ar() -> Ar {
        Ar::new(DeviceModel::pnet_sample(MAC))
    }
    fn connect() -> Event {
        Event::ConnectReq(ConnectReq::parse(&golden("connect_req")[BLOCKS..]).unwrap())
    }
    fn write() -> Event {
        Event::WriteReq(WriteReq::parse(&golden("write_req")[BLOCKS..]).unwrap())
    }
    fn prm_end() -> Event {
        Event::PrmEndReq(ControlBlock::parse(&golden("prmend_req")[BLOCKS..]).unwrap())
    }
    fn t0() -> Instant {
        Instant::now()
    }

    fn respond_ok(actions: &[Action]) -> &Vec<u8> {
        match &actions[0] {
            Action::Respond { status, blocks } if status.is_ok() => blocks,
            other => panic!("{other:?}"),
        }
    }

    /// A Release request block (built by mutating a parsed PrmEnd request, same as
    /// the original `release_aborts_and_answers` test does inline).
    fn release_block() -> ControlBlock {
        let mut rel = ControlBlock::parse(&golden("prmend_req")[BLOCKS..]).unwrap();
        rel.block_type = ty::RELEASE_BLOCK_REQ;
        rel.command = cmd::RELEASE;
        rel
    }

    /// Drive a fresh `Ar` all the way to `Data` via the nominal path (Connect,
    /// Write, PrmEnd, AppReadyRsp ok), for tests of the Data row of the transition
    /// table. Returns the machine and the `now` used throughout.
    fn to_data() -> (Ar, Instant) {
        let mut ar = ar();
        let now = t0();
        ar.on(connect(), now);
        ar.on(write(), now);
        ar.on(prm_end(), now);
        ar.on(
            Event::AppReadyRsp {
                status: PnioStatus::OK,
            },
            now,
        );
        assert_eq!(ar.state(), ArState::Data);
        (ar, now)
    }

    #[test]
    fn nominal_idle_to_data() {
        let mut ar = ar();
        let now = t0();
        let a = ar.on(connect(), now);
        assert_eq!(respond_ok(&a), &golden("connect_res")[BLOCKS..]);
        assert!(matches!(
            a[1],
            Action::Notify {
                state: ArState::Connected,
                reason: None
            }
        ));
        assert_eq!(ar.state(), ArState::Connected);
        let a = ar.on(write(), now);
        assert_eq!(respond_ok(&a), &golden("write_res")[BLOCKS..]);
        assert_eq!(ar.context().unwrap().records.len(), 5);
        let a = ar.on(prm_end(), now);
        assert_eq!(respond_ok(&a), &golden("prmend_res")[BLOCKS..]);
        assert!(
            matches!(&a[1], Action::CallController { blocks } if blocks == &golden("appready_req")[BLOCKS..])
        );
        assert_eq!(ar.state(), ArState::AppReadySent);
        assert_eq!(ar.next_deadline(), Some(now + APP_READY_TIMEOUT));
        let a = ar.on(
            Event::AppReadyRsp {
                status: PnioStatus::OK,
            },
            now,
        );
        assert!(matches!(
            a[0],
            Action::Notify {
                state: ArState::Data,
                reason: None
            }
        ));
        assert_eq!(ar.state(), ArState::Data);
        assert_eq!(ar.next_deadline(), None);
    }

    #[test]
    fn rejected_connect_stays_idle_with_status() {
        let mut ar = Ar::new({
            let mut m = DeviceModel::pnet_sample(MAC);
            m.slots.pop();
            m
        });
        let a = ar.on(connect(), t0());
        assert!(
            matches!(&a[0], Action::Respond { status, blocks } if !status.is_ok() && blocks.is_empty())
        );
        assert_eq!(ar.state(), ArState::Idle);
        assert!(ar.context().is_none());
    }

    #[test]
    fn duplicate_connect_is_idempotent_and_other_ar_is_rejected() {
        let mut ar = ar();
        let now = t0();
        let first = respond_ok(&ar.on(connect(), now)).clone();
        assert_eq!(respond_ok(&ar.on(connect(), now)), &first);
        let mut other = match connect() {
            Event::ConnectReq(c) => c,
            _ => unreachable!(),
        };
        other.ar.ar_uuid = crate::rpc::Uuid([7; 16]);
        let a = ar.on(Event::ConnectReq(other), now);
        assert!(
            matches!(&a[0], Action::Respond { status, .. } if *status == PnioStatus::connect_ar_already_exists())
        );
        assert_eq!(ar.state(), ArState::Connected);
    }

    #[test]
    fn write_or_prm_end_in_idle_is_wrong_state() {
        let mut ar = ar();
        let a = ar.on(write(), t0());
        assert!(
            matches!(&a[0], Action::Respond { status, .. } if *status == PnioStatus::control_wrong_state())
        );
        let a = ar.on(prm_end(), t0());
        assert!(
            matches!(&a[0], Action::Respond { status, .. } if *status == PnioStatus::control_wrong_state())
        );
    }

    #[test]
    fn app_ready_retries_three_times_then_aborts() {
        let mut ar = ar();
        let now = t0();
        ar.on(connect(), now);
        let a = ar.on(prm_end(), now);
        assert!(matches!(a[1], Action::CallController { .. }));
        let t1 = now + APP_READY_TIMEOUT + Duration::from_millis(1);
        let a = ar.on(Event::Tick, t1);
        assert!(matches!(a[0], Action::CallController { .. }));
        let t2 = t1 + APP_READY_TIMEOUT + Duration::from_millis(1);
        let a = ar.on(Event::Tick, t2);
        assert!(matches!(a[0], Action::CallController { .. }));
        let t3 = t2 + APP_READY_TIMEOUT + Duration::from_millis(1);
        let a = ar.on(Event::Tick, t3);
        assert!(matches!(
            a[0],
            Action::Notify {
                state: ArState::Idle,
                reason: Some(AbortReason::AppReadyFailed)
            }
        ));
        assert_eq!(ar.state(), ArState::Idle);
    }

    #[test]
    fn app_ready_bad_status_aborts() {
        let mut ar = ar();
        let now = t0();
        ar.on(connect(), now);
        ar.on(prm_end(), now);
        let bad = PnioStatus::new(0xdd, 0x81, 1, 1);
        let a = ar.on(Event::AppReadyRsp { status: bad }, now);
        assert!(matches!(
            a[0],
            Action::Notify {
                state: ArState::Idle,
                reason: Some(AbortReason::AppReadyRejected(s))
            } if s == bad
        ));
    }

    #[test]
    fn activity_timeout_before_data_aborts() {
        let mut ar = ar();
        let now = t0();
        ar.on(connect(), now); // factor 200 -> 20 s
        assert_eq!(
            ar.next_deadline(),
            Some(now + Duration::from_millis(200 * 100))
        );
        let a = ar.on(Event::Tick, now + Duration::from_secs(21));
        assert!(matches!(
            a[0],
            Action::Notify {
                state: ArState::Idle,
                reason: Some(AbortReason::ActivityTimeout)
            }
        ));
    }

    #[test]
    fn release_aborts_and_answers() {
        let mut ar = ar();
        let now = t0();
        ar.on(connect(), now);
        let mut rel = ControlBlock::parse(&golden("prmend_req")[BLOCKS..]).unwrap();
        rel.block_type = ty::RELEASE_BLOCK_REQ;
        rel.command = cmd::RELEASE;
        let a = ar.on(Event::ReleaseReq(rel), now);
        assert!(
            matches!(&a[0], Action::Respond { status, blocks } if status.is_ok() && blocks[0..2] == [0x81, 0x14])
        );
        assert!(matches!(
            a[1],
            Action::Notify {
                state: ArState::Idle,
                reason: Some(AbortReason::ControllerRelease)
            }
        ));
    }

    #[test]
    fn connect_after_abort_succeeds_with_fresh_context() {
        let mut ar = ar();
        let now = t0();
        ar.on(connect(), now);
        ar.on(Event::Abort(AbortReason::External("test")), now);
        assert!(ar.context().is_none());
        let a = ar.on(connect(), now);
        assert_eq!(respond_ok(&a), &golden("connect_res")[BLOCKS..]);
    }

    // -----------------------------------------------------------------------
    // Fix round 1: records accumulate; the activity timer is armed only while
    // Connected; and one test per remaining, previously-untested transition
    // table cell.
    // -----------------------------------------------------------------------

    #[test]
    fn records_accumulate_across_writes() {
        let mut ar = ar();
        let now = t0();
        ar.on(connect(), now);
        ar.on(write(), now);
        assert_eq!(ar.context().unwrap().records.len(), 5);
        ar.on(write(), now);
        assert_eq!(ar.context().unwrap().records.len(), 10);
    }

    #[test]
    fn write_in_app_ready_sent_does_not_touch_timers() {
        let mut ar = ar();
        let now = t0();
        ar.on(connect(), now);
        ar.on(prm_end(), now);
        assert_eq!(ar.next_deadline(), Some(now + APP_READY_TIMEOUT));
        let a = ar.on(write(), now);
        assert_eq!(respond_ok(&a), &golden("write_res")[BLOCKS..]);
        assert_eq!(ar.context().unwrap().records.len(), 5);
        assert_eq!(ar.next_deadline(), Some(now + APP_READY_TIMEOUT));
    }

    // --- Idle ---

    #[test]
    fn idle_release_answers_without_abort() {
        let mut ar = ar();
        let a = ar.on(Event::ReleaseReq(release_block()), t0());
        assert_eq!(a.len(), 1);
        assert!(
            matches!(&a[0], Action::Respond { status, blocks } if status.is_ok() && blocks[0..2] == [0x81, 0x14])
        );
        assert_eq!(ar.state(), ArState::Idle);
        assert!(ar.context().is_none());
    }

    #[test]
    fn idle_app_ready_rsp_is_ignored() {
        let mut ar = ar();
        let a = ar.on(
            Event::AppReadyRsp {
                status: PnioStatus::OK,
            },
            t0(),
        );
        assert!(a.is_empty());
        assert_eq!(ar.state(), ArState::Idle);
    }

    #[test]
    fn idle_tick_is_noop() {
        let mut ar = ar();
        let a = ar.on(Event::Tick, t0());
        assert!(a.is_empty());
        assert_eq!(ar.next_deadline(), None);
    }

    // --- Connected ---

    #[test]
    fn connected_app_ready_rsp_is_ignored() {
        let mut ar = ar();
        let now = t0();
        ar.on(connect(), now);
        let a = ar.on(
            Event::AppReadyRsp {
                status: PnioStatus::OK,
            },
            now,
        );
        assert!(a.is_empty());
        assert_eq!(ar.state(), ArState::Connected);
    }

    #[test]
    fn connected_tick_before_deadline_is_noop() {
        let mut ar = ar();
        let now = t0();
        ar.on(connect(), now); // factor 200 -> 20 s deadline
        let a = ar.on(Event::Tick, now + Duration::from_secs(1));
        assert!(a.is_empty());
        assert_eq!(ar.state(), ArState::Connected);
    }

    #[test]
    fn connected_write_rearms_activity_deadline_from_now() {
        let mut ar = ar();
        let now = t0();
        ar.on(connect(), now);
        let now2 = now + Duration::from_secs(5);
        ar.on(write(), now2);
        assert_eq!(ar.next_deadline(), Some(now2 + Duration::from_secs(20)));
    }

    // --- AppReadySent ---

    #[test]
    fn app_ready_sent_connect_same_uuid_is_idempotent() {
        let mut ar = ar();
        let now = t0();
        ar.on(connect(), now);
        ar.on(prm_end(), now);
        let a = ar.on(connect(), now);
        assert_eq!(respond_ok(&a), &golden("connect_res")[BLOCKS..]);
        assert_eq!(ar.state(), ArState::AppReadySent);
    }

    #[test]
    fn app_ready_sent_prm_end_again_is_idempotent_without_new_call() {
        let mut ar = ar();
        let now = t0();
        ar.on(connect(), now);
        ar.on(prm_end(), now);
        let a = ar.on(prm_end(), now);
        assert_eq!(a.len(), 1);
        assert_eq!(respond_ok(&a), &golden("prmend_res")[BLOCKS..]);
        // No re-arm/retry happened: the deadline is unchanged, so attempts is too.
        assert_eq!(ar.next_deadline(), Some(now + APP_READY_TIMEOUT));
    }

    #[test]
    fn app_ready_sent_release_aborts() {
        let mut ar = ar();
        let now = t0();
        ar.on(connect(), now);
        ar.on(prm_end(), now);
        let a = ar.on(Event::ReleaseReq(release_block()), now);
        assert!(
            matches!(&a[0], Action::Respond { status, blocks } if status.is_ok() && blocks[0..2] == [0x81, 0x14])
        );
        assert!(matches!(
            a[1],
            Action::Notify {
                state: ArState::Idle,
                reason: Some(AbortReason::ControllerRelease)
            }
        ));
        assert_eq!(ar.state(), ArState::Idle);
    }

    // --- Data ---

    #[test]
    fn data_connect_other_uuid_is_rejected() {
        let (mut ar, now) = to_data();
        let mut other = match connect() {
            Event::ConnectReq(c) => c,
            _ => unreachable!(),
        };
        other.ar.ar_uuid = crate::rpc::Uuid([7; 16]);
        let a = ar.on(Event::ConnectReq(other), now);
        assert!(
            matches!(&a[0], Action::Respond { status, .. } if *status == PnioStatus::connect_ar_already_exists())
        );
        assert_eq!(ar.state(), ArState::Data);
    }

    #[test]
    fn data_connect_same_uuid_resends_cached_bytes() {
        let (mut ar, now) = to_data();
        let a = ar.on(connect(), now);
        assert_eq!(respond_ok(&a), &golden("connect_res")[BLOCKS..]);
        assert_eq!(ar.state(), ArState::Data);
    }

    #[test]
    fn data_write_stores_records_and_touches_no_timer() {
        let (mut ar, now) = to_data();
        let a = ar.on(write(), now);
        assert_eq!(respond_ok(&a), &golden("write_res")[BLOCKS..]);
        assert_eq!(ar.next_deadline(), None);
    }

    #[test]
    fn data_prm_end_is_idempotent() {
        let (mut ar, now) = to_data();
        let a = ar.on(prm_end(), now);
        assert_eq!(respond_ok(&a), &golden("prmend_res")[BLOCKS..]);
        assert_eq!(ar.state(), ArState::Data);
    }

    #[test]
    fn data_release_aborts() {
        let (mut ar, now) = to_data();
        let a = ar.on(Event::ReleaseReq(release_block()), now);
        assert!(
            matches!(&a[0], Action::Respond { status, blocks } if status.is_ok() && blocks[0..2] == [0x81, 0x14])
        );
        assert!(matches!(
            a[1],
            Action::Notify {
                state: ArState::Idle,
                reason: Some(AbortReason::ControllerRelease)
            }
        ));
        assert_eq!(ar.state(), ArState::Idle);
    }

    #[test]
    fn data_app_ready_rsp_is_ignored() {
        let (mut ar, now) = to_data();
        let a = ar.on(
            Event::AppReadyRsp {
                status: PnioStatus::OK,
            },
            now,
        );
        assert!(a.is_empty());
        assert_eq!(ar.state(), ArState::Data);
    }

    #[test]
    fn data_tick_is_noop() {
        let (mut ar, now) = to_data();
        let a = ar.on(Event::Tick, now + Duration::from_secs(100));
        assert!(a.is_empty());
        assert_eq!(ar.state(), ArState::Data);
    }
}

//! The acyclic loop: services the DCP (Ethernet) and RPC (UDP) sockets, drives
//! [`Cm`] with what they produce, and executes its outputs. Log-and-drop
//! for parse errors (spec §8); only transport I/O failures abort the loop.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::alarm::{
    AlarmAction, AlarmChannel, AlarmChannelConfig, AlarmNotification, AlarmReq, Priority,
};
use crate::cm::model::DeviceModel;
use crate::cm::{AbortReason, ArParams, ArState, Cm, CmOutput, PnioStatus};
use crate::config::Slot;
use crate::dcp::{handle_dcp_frame, DcpConfig};
use crate::diag::{ChannelError, DiagStore, Diagnosis};
use crate::eth::poll::wait_any_readable;
use crate::eth::{EthTransport, TransportError};
use crate::im::{Im0, ImStore};
use crate::rpc::{RpcError, RpcTransport, Uuid};
use crate::rt::{IoImage, RtStats};
#[cfg(target_os = "linux")]
use crate::rt::{Layout, RtConfig, RtError, RtEvent, RtHandle, RtRunner, WatchdogState};

/// Callback invoked once per AR state-change notification.
type StateChangeCallback = Box<dyn FnMut(ArState, Option<AbortReason>) + Send>;

/// Cyclic (RT) thread configuration for one AR.
///
/// `None` in [`DeviceSetup::rt`] means no cyclic thread is ever started — used by the
/// mock-based tests and the AR-only example, which have no real Ethernet interface to
/// send RTC1 frames on.
#[derive(Debug, Clone)]
pub struct RtOptions {
    /// Interface the RT thread opens (`AF_PACKET`).
    pub iface: String,
    /// Pin the RT thread to this CPU, if set.
    pub cpu_pin: Option<usize>,
    /// Run the RT thread at this `SCHED_FIFO` priority, if set.
    pub rt_priority: Option<u8>,
    /// Lock process memory and pre-fault the RT stack (`mlockall`); needs
    /// `CAP_IPC_LOCK` or a sufficient `RLIMIT_MEMLOCK`, otherwise a `SchedWarning`.
    pub lock_memory: bool,
}

/// Static device identity + configuration handed to [`Device::new`]: the DCP identity
/// answered on the wire, the AR/slot model `Cm` establishes connections against, the
/// activity UUID used for our outgoing ApplicationReady calls, and the cyclic thread
/// configuration (if any) started once the AR reaches `Data`.
#[derive(Debug, Clone)]
pub struct DeviceSetup {
    pub dcp: DcpConfig,
    pub model: DeviceModel,
    pub activity_seed: Uuid,
    pub rt: Option<RtOptions>,
    /// Device identity answered by I&M0 reads.
    pub im0: Im0,
    /// Backing file for the writable I&M1-3 records; `None` keeps them in memory
    /// only (blank at startup, lost on restart).
    pub im_store: Option<PathBuf>,
}

/// One diagnosis change the application asks the acyclic loop to apply. Queued in
/// [`DiagShared::queue`] and drained once per [`Device::step`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagCommand {
    Raise(Diagnosis),
    Clear {
        slot: Slot,
        channel: u16,
        error: ChannelError,
    },
}

/// Shared between `Device` (which owns the acyclic loop) and the application: the
/// inbound command queue, the published set of active diagnoses, and the alarm
/// channel's counters.
///
/// The counters are mirrored from [`AlarmChannel::stats`] after every batch of
/// actions, so they are per-AR (a new AR starts a fresh channel, hence fresh
/// counts); `rx_no_channel` is the device's own and counts alarm frames that
/// arrived with no channel open at all.
#[derive(Debug, Default)]
pub struct DiagShared {
    pub queue: Mutex<VecDeque<DiagCommand>>,
    pub active: Mutex<Vec<Diagnosis>>,
    pub sent: AtomicU64,
    pub acked: AtomicU64,
    pub retries: AtomicU64,
    pub unexpected_rx: AtomicU64,
    pub send_failures: AtomicU64,
    pub ack_timeouts: AtomicU64,
    pub rx_err_rta: AtomicU64,
    pub rx_no_channel: AtomicU64,
}

/// The `PNIOStatus.ErrorCode2` announced in the ERR-RTA that precedes a device-side
/// abort, or `None` when no ERR-RTA must be sent: the controller's own ERR-RTA has
/// already torn the alarm channel down on its side, so answering it would be noise
/// (spec §5.6).
fn err_rta_code2(reason: &AbortReason) -> Option<u8> {
    // Exhaustive on purpose (no `_` arm): a new `AbortReason` must be given a code2
    // here rather than silently inheriting one.
    match reason {
        AbortReason::ControllerErrRta(_) => None,
        AbortReason::RtWatchdog => Some(PnioStatus::RTA_ABORT_DHT_WDT_EXPIRED),
        AbortReason::AlarmSendFailed => Some(PnioStatus::RTA_ABORT_ALARM_SEND_FAILED),
        // Every other device-side abort says the same thing: this AR is gone.
        AbortReason::RtSocket
        | AbortReason::Shutdown
        | AbortReason::ControllerRelease
        | AbortReason::ControllerReconnect
        | AbortReason::AppReadyFailed
        | AbortReason::AppReadyRejected(_)
        | AbortReason::ActivityTimeout
        | AbortReason::External(_) => Some(PnioStatus::RTA_ABORT_AR_REMOVED),
    }
}

/// Poll bound used while an alarm is in flight or diagnosis commands are queued, so
/// RTA retries and freshly raised alarms are serviced in milliseconds rather than at
/// the caller's idle cadence.
const ALARM_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// `AlarmCRBlockReq.RTATimeoutFactor` is expressed in units of 100 ms.
const RTA_TIMEOUT_UNIT: Duration = Duration::from_millis(100);

/// Counts of what one [`Device::step`] processed: frames/datagrams drained from each
/// transport, and PDUs sent out (DCP responses are not counted; only RPC sends are).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StepReport {
    pub eth_frames: usize,
    pub rpc_datagrams: usize,
    pub sent: usize,
}

/// Transport I/O failures from `step`/`run`. Parse errors from `handle_dcp_frame` or
/// `Cm::handle_datagram` never reach here — those are logged and dropped in place.
#[derive(Debug, Error)]
pub enum DeviceError {
    #[error("Ethernet transport error: {0}")]
    Eth(#[from] TransportError),
    #[error("RPC transport error: {0}")]
    Rpc(#[from] RpcError),
}

/// Owns both transports and the `Cm` state machine, and runs the single-threaded
/// acyclic loop: wait for readiness, drain both sockets (DCP frames through
/// [`handle_dcp_frame`], RPC datagrams through [`Cm::handle_datagram`]), then let
/// `Cm::tick` drive its timers, dispatching every PDU/notification each step produces.
pub struct Device<E: EthTransport, R: RpcTransport> {
    setup: DeviceSetup,
    eth: E,
    rpc: R,
    cm: Cm,
    on_state_change: Option<StateChangeCallback>,
    /// The shared I/O image handed to the application via [`Device::image`]. Built
    /// empty in `new` and (re)sized/indexed from the negotiated layout each time the
    /// AR reaches `Data`.
    image: Arc<IoImage>,
    /// Counters updated by the RT thread, readable via [`Device::rt_stats`] whether
    /// or not a runner is currently alive.
    stats: Arc<RtStats>,
    /// Station problem indicator, shared with the RT thread via [`RtConfig`]: `true`
    /// clears bit 5 (`Station_Problem_Indicator`) of the data status on every produced
    /// frame. Read via [`Device::problem_indicator`]; driven by diagnosis bookkeeping
    /// (Task 8).
    problem: Arc<AtomicBool>,
    /// Channel-diagnosis bookkeeping: owns the active set, produces the
    /// appears/disappears notifications and the aggregate problem indicator.
    diag: DiagStore,
    /// The application's side of the diagnosis interface (command queue, published
    /// active set, alarm counters).
    diag_shared: Arc<DiagShared>,
    /// The Low-priority alarm channel of the current AR: created when the AR reaches
    /// `Data`, dropped with the AR.
    alarm: Option<AlarmChannel>,
    /// Monotonic id handed to [`AlarmReq::id`] so an `Acked` action can be traced
    /// back to the notification that produced it.
    next_alarm_id: u32,
    /// Incremented every time an alarm channel is opened. `step` collects actions
    /// early and applies them last, so each batch carries the epoch it was produced
    /// under and is dropped if the channel has been replaced (or closed) in between —
    /// otherwise a stale abort (e.g. the controller's ERR-RTA) drained in the same
    /// `step` as its reconnect would tear down the AR that reconnect just built.
    alarm_epoch: u64,
    /// The currently running RT thread, if any (Linux-only: the runner itself is
    /// only ever built on Linux).
    #[cfg(target_os = "linux")]
    runner: Option<RtHandle>,
    /// How [`Device::start_runner`] spawns the RT thread; overridable via
    /// [`Device::with_runner_factory`] (defaults to [`RtRunner::spawn`]).
    #[cfg(target_os = "linux")]
    runner_factory: Box<dyn Fn(RtConfig) -> Result<RtHandle, RtError> + Send>,
}

impl<E: EthTransport, R: RpcTransport> Device<E, R> {
    pub fn new(setup: DeviceSetup, eth: E, rpc: R) -> Device<E, R> {
        let mut cm = Cm::new(setup.model.clone(), setup.activity_seed);
        cm.set_im(setup.im0.clone(), ImStore::load(setup.im_store.clone()));
        let diag = DiagStore::from_model(&setup.model);
        Device {
            setup,
            eth,
            rpc,
            cm,
            on_state_change: None,
            image: Arc::new(IoImage::empty()),
            stats: Arc::new(RtStats::default()),
            problem: Arc::new(AtomicBool::new(false)),
            diag,
            diag_shared: Arc::new(DiagShared::default()),
            alarm: None,
            next_alarm_id: 0,
            alarm_epoch: 0,
            #[cfg(target_os = "linux")]
            runner: None,
            #[cfg(target_os = "linux")]
            runner_factory: Box::new(RtRunner::spawn),
        }
    }

    pub fn state(&self) -> ArState {
        self.cm.state()
    }

    pub fn eth(&self) -> &E {
        &self.eth
    }

    pub fn rpc(&self) -> &R {
        &self.rpc
    }

    /// The shared I/O image: empty (no cells) until the AR first reaches `Data`, then
    /// rebuilt from the negotiated layout on every `Data` (including AR
    /// re-negotiation).
    pub fn image(&self) -> Arc<IoImage> {
        self.image.clone()
    }

    /// The RT thread's counters. Readable (and all-zero) even with no runner alive.
    pub fn rt_stats(&self) -> Arc<RtStats> {
        self.stats.clone()
    }

    /// Current station problem indicator: `true` means the produced frames carry
    /// [`DataStatus::RUN_PRIMARY_VALID_PROBLEM`](crate::rt::DataStatus::RUN_PRIMARY_VALID_PROBLEM)
    /// instead of the steady-state value.
    pub fn problem_indicator(&self) -> bool {
        self.problem.load(Ordering::Relaxed)
    }

    /// The application's side of the diagnosis interface: push [`DiagCommand`]s into
    /// `queue` (drained once per [`Device::step`]), read back `active` and the alarm
    /// counters.
    pub fn diag_shared(&self) -> Arc<DiagShared> {
        self.diag_shared.clone()
    }

    /// True while an alarm has been sent and is still waiting for the controller's
    /// transport ACK or its content-level AlarmAck.
    pub fn alarm_in_flight(&self) -> bool {
        self.alarm
            .as_ref()
            .is_some_and(|ch| ch.in_flight().is_some())
    }

    /// Poll bound for the caller's loop: `ALARM_POLL_INTERVAL` (20 ms) while an
    /// alarm is in flight or diagnosis commands are queued, so RTA retries and fresh
    /// alarms are serviced promptly; otherwise `default`.
    pub fn poll_interval(&self, default: Duration) -> Duration {
        let queued = !self
            .diag_shared
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty();
        if self.alarm_in_flight() || queued {
            ALARM_POLL_INTERVAL.min(default)
        } else {
            default
        }
    }

    /// A clone of the current AR's negotiated parameters, if one is established.
    pub fn ar_params(&self) -> Option<ArParams> {
        self.cm.context().map(|c| c.params.clone())
    }

    /// Registers a callback invoked once per AR state-change notification produced by
    /// `Cm` (used by the example for logging).
    pub fn on_state_change(
        &mut self,
        f: impl FnMut(ArState, Option<AbortReason>) + Send + 'static,
    ) {
        self.on_state_change = Some(Box::new(f));
    }

    /// Runs the loop until `stop` is set, re-checking it at least every 200ms so it
    /// stays responsive even with nothing arriving on either socket.
    pub fn run(&mut self, stop: &AtomicBool) -> Result<(), DeviceError> {
        while !stop.load(Ordering::Relaxed) {
            let wait = self.poll_interval(Duration::from_millis(200));
            self.step(Instant::now(), Some(wait))?;
        }
        self.shutdown(Instant::now())
    }

    /// Announces a device-side abort on the alarm channel (ERR-RTA), then aborts the
    /// AR. No ERR-RTA is sent when there is no channel, or for
    /// [`AbortReason::ControllerErrRta`] (the controller aborted first). A no-op on
    /// the `Cm` side if the AR is already `Idle`.
    pub fn abort_with_err_rta(
        &mut self,
        reason: AbortReason,
        now: Instant,
    ) -> Result<(), DeviceError> {
        let mut report = StepReport::default();
        self.abort_with_err_rta_in(reason, now, &mut report)
    }

    /// Tells the controller the device is going away: ERR-RTA "AR removed" on the
    /// alarm channel plus an AR abort, so it sees the loss in milliseconds instead of
    /// waiting for its own watchdog. A no-op with no AR up.
    pub fn shutdown(&mut self, now: Instant) -> Result<(), DeviceError> {
        if self.cm.state() == ArState::Idle {
            return Ok(());
        }
        self.abort_with_err_rta(AbortReason::Shutdown, now)
    }

    /// One loop iteration: wait for readiness (capped by both `wait` and `Cm`'s next
    /// timer deadline), drain every pending DCP frame and RPC datagram, then let
    /// `Cm::tick` drive its timers. Parse errors are logged and dropped; transport I/O
    /// errors abort with `DeviceError`.
    pub fn step(
        &mut self,
        now: Instant,
        wait: Option<Duration>,
    ) -> Result<StepReport, DeviceError> {
        let alarm_deadline = self.alarm.as_ref().and_then(|ch| ch.next_deadline());
        let deadline = match (self.cm.next_deadline(), alarm_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let deadline_wait = deadline.map(|d| d.saturating_duration_since(now));
        let effective_wait = match (wait, deadline_wait) {
            (Some(w), Some(d)) => Some(w.min(d)),
            (Some(w), None) => Some(w),
            (None, Some(d)) => Some(d),
            (None, None) => None,
        };
        // Everything below is dated with `now`. When a real poll happened it may have
        // waited up to `effective_wait` (200 ms in `run`), so re-read the clock afterwards:
        // an alarm enqueued in this step would otherwise carry a retry deadline computed
        // from the pre-wait instant — already expired at the next step — and the
        // controller would receive the notification twice (HIL 2026-08-31, §6i). Mock
        // transports without fds skip the poll and keep the caller's `now`, so tests
        // that drive time by hand stay deterministic.
        let now = if let (Some(eth_fd), Some(rpc_fd)) = (self.eth.raw_fd(), self.rpc.raw_fd()) {
            let mut fds = vec![eth_fd, rpc_fd];
            #[cfg(target_os = "linux")]
            if let Some(runner) = &self.runner {
                fds.push(runner.event_fd());
            }
            wait_any_readable(&fds, effective_wait)
                .map_err(|e| DeviceError::Eth(TransportError::Io(e)))?;
            Instant::now()
        } else {
            // Mock transports have no fds to poll on; proceed straight to draining (their
            // `recv` ignores the timeout and returns immediately; the RT events are
            // drained below regardless of whether we polled for them).
            now
        };

        let mut report = StepReport::default();
        // Alarm actions produced anywhere in this step are collected here and applied
        // last (step 7): applying an `Abort` re-enters `dispatch`, which drops the
        // very channel the earlier steps are still borrowing. Each batch carries the
        // channel epoch it was produced under, so stages 3-5 replacing the channel
        // invalidate it (see `alarm_epoch`).
        let mut pending: Vec<(u64, Vec<AlarmAction>)> = Vec::new();

        // (1) Ethernet: alarm frames to the channel, everything else to DCP.
        while let Some(frame) = self.eth.recv(Some(Duration::ZERO))? {
            report.eth_frames += 1;
            if crate::alarm::is_alarm_frame(&frame) {
                // Alarm frames are unicast to us. A socket that also sees other
                // stations' traffic must not have its counters moved by it.
                if frame.get(..6) != Some(&self.setup.dcp.mac.0[..]) {
                    log::trace!("alarm frame addressed to another station; dropping");
                    continue;
                }
                match &mut self.alarm {
                    Some(ch) => {
                        let a = ch.on_frame(&frame, now);
                        if !a.is_empty() {
                            pending.push((self.alarm_epoch, a));
                        }
                    }
                    None => {
                        log::debug!("alarm frame with no alarm channel open; dropping");
                        self.diag_shared
                            .rx_no_channel
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
                continue;
            }
            match handle_dcp_frame(&frame, &self.setup.dcp) {
                Ok(Some(resp)) => self.eth.send(&resp)?,
                Ok(None) => {}
                Err(e) => log::warn!("dropping unparsable DCP frame: {e}"),
            }
        }
        self.publish_alarm_stats();

        // (2) The application's diagnosis commands.
        self.drain_diag_queue(now, &mut pending);

        // (3) RPC.
        while let Some((buf, from)) = self.rpc.recv(Some(Duration::ZERO))? {
            report.rpc_datagrams += 1;
            match self.cm.handle_datagram(&buf, from, now) {
                Ok(out) => self.dispatch(out, now, &mut report)?,
                Err(e) => log::warn!("dropping unparsable RPC datagram: {e}"),
            }
        }

        // (4) RT thread events.
        #[cfg(target_os = "linux")]
        self.drain_rt_events(now, &mut report)?;

        // (5) `Cm` timers.
        let out = self.cm.tick(now);
        self.dispatch(out, now, &mut report)?;

        // (6) RTA timers.
        if let Some(ch) = &mut self.alarm {
            let a = ch.on_tick(now);
            if !a.is_empty() {
                pending.push((self.alarm_epoch, a));
            }
        }
        self.publish_alarm_stats();

        // (7) Apply everything the (still current) alarm channel asked for.
        for (epoch, batch) in pending {
            self.apply_alarm_actions(batch, epoch, now, &mut report)?;
        }

        Ok(report)
    }

    /// Drains the application's [`DiagCommand`]s into the [`DiagStore`], turning each
    /// resulting notification into a queued Low-priority alarm, then republishes the
    /// active set and the problem indicator.
    fn drain_diag_queue(&mut self, now: Instant, pending: &mut Vec<(u64, Vec<AlarmAction>)>) {
        let cmds: Vec<DiagCommand> = self
            .diag_shared
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect();
        if cmds.is_empty() {
            return;
        }
        for cmd in cmds {
            let notification = match cmd {
                DiagCommand::Raise(d) => {
                    if !self.diag.knows(d.slot) {
                        log::warn!("diagnosis for unknown slot {}; dropping", d.slot.0);
                    }
                    self.diag.raise(d)
                }
                DiagCommand::Clear {
                    slot,
                    channel,
                    error,
                } => self.diag.clear(slot, channel, error),
            };
            if let Some(n) = notification {
                let a = self.queue_notification(n, now);
                if !a.is_empty() {
                    pending.push((self.alarm_epoch, a));
                }
            }
        }
        self.publish_diag_state();
    }

    /// Publishes the diagnosis store's view: the active set for the application and
    /// the aggregate problem indicator for the RT thread's data status.
    fn publish_diag_state(&self) {
        *self
            .diag_shared
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = self.diag.active();
        self.problem
            .store(self.diag.problem_indicator(), Ordering::Relaxed);
    }

    /// Mirrors the alarm channel's counters into [`DiagShared`]. A no-op with no
    /// channel, so the counts of a channel that just died stay readable.
    fn publish_alarm_stats(&self) {
        let Some(ch) = &self.alarm else {
            return;
        };
        let s = ch.stats();
        let d = &self.diag_shared;
        d.sent.store(s.sent, Ordering::Relaxed);
        d.acked.store(s.acked, Ordering::Relaxed);
        d.retries.store(s.retries, Ordering::Relaxed);
        d.unexpected_rx.store(s.unexpected_rx, Ordering::Relaxed);
        d.send_failures.store(s.send_failures, Ordering::Relaxed);
        d.ack_timeouts.store(s.ack_timeouts, Ordering::Relaxed);
        d.rx_err_rta.store(s.rx_err_rta, Ordering::Relaxed);
    }

    /// Stamps the per-AR `AlarmSpecifier.sequence` and queues one diagnosis
    /// notification on the alarm channel. Diagnosis alarms are always Low priority
    /// (spec §5.2). No channel (no AR yet) drops it: the store keeps the diagnosis
    /// active and [`DiagStore::replay`] re-sends it when the next AR reaches `Data`.
    fn queue_notification(&mut self, mut n: AlarmNotification, now: Instant) -> Vec<AlarmAction> {
        let id = self.next_alarm_id;
        let Some(ch) = self.alarm.as_mut() else {
            return Vec::new();
        };
        n.specifier.sequence = ch.next_specifier_sequence();
        let req = AlarmReq {
            id,
            priority: Priority::Low,
            notification: n,
        };
        match ch.enqueue(req, now) {
            Ok(actions) => {
                self.next_alarm_id = self.next_alarm_id.wrapping_add(1);
                actions
            }
            Err(e) => {
                // Unreachable for the 6-byte channel-diagnosis payloads this store
                // produces against any sane negotiated MaxAlarmDataLength.
                log::error!("dropping diagnosis alarm: {e}");
                Vec::new()
            }
        }
    }

    /// Performs one batch of [`AlarmAction`]s produced under channel epoch `epoch`:
    /// frames go out on the acyclic socket, an `Abort` announces itself with an
    /// ERR-RTA and tears the AR down. Counters come from [`AlarmChannel::stats`],
    /// mirrored once the batch is done.
    ///
    /// A batch whose channel is gone (closed, or replaced by a reconnect processed
    /// later in the same `step`) is dropped whole: it belongs to an AR that no longer
    /// exists. `Abort` is always the only action a batch carries, so this cannot drop
    /// half of one.
    fn apply_alarm_actions(
        &mut self,
        actions: Vec<AlarmAction>,
        epoch: u64,
        now: Instant,
        report: &mut StepReport,
    ) -> Result<(), DeviceError> {
        if self.alarm.is_none() || self.alarm_epoch != epoch {
            log::debug!(
                "dropping {} alarm action(s) from a closed alarm channel",
                actions.len()
            );
            return Ok(());
        }
        for action in actions {
            match action {
                AlarmAction::Send(frame) => self.eth.send(&frame)?,
                AlarmAction::Acked { id, status } => {
                    log::debug!("alarm {id} acknowledged, status {:#010x}", status.to_u32());
                }
                AlarmAction::UnexpectedRx => {
                    log::debug!("unexpected frame on the alarm channel");
                }
                AlarmAction::Abort(reason) => {
                    log::warn!("alarm channel asked for an AR abort: {reason:?}");
                    self.abort_with_err_rta_in(reason, now, report)?;
                }
            }
        }
        self.publish_alarm_stats();
        Ok(())
    }

    /// [`Device::abort_with_err_rta`], threading the caller's [`StepReport`] so the
    /// RPC PDUs the abort produces are counted like any other dispatch.
    fn abort_with_err_rta_in(
        &mut self,
        reason: AbortReason,
        now: Instant,
        report: &mut StepReport,
    ) -> Result<(), DeviceError> {
        if let (Some(code2), Some(ch)) = (err_rta_code2(&reason), self.alarm.as_mut()) {
            let frame = ch.err_rta(PnioStatus::rta_abort(code2));
            self.eth.send(&frame)?;
        }
        let out = self.cm.abort(reason, now);
        self.dispatch(out, now, report)
    }

    /// Opens the AR's Low-priority alarm channel from the negotiated AlarmCR
    /// parameters. The length bound is the controller's `MaxAlarmDataLength` (what it
    /// accepts from us), not our own answer.
    fn open_alarm_channel(&mut self) {
        let Some(params) = self.cm.context().map(|c| c.params.clone()) else {
            return; // unreachable in practice: a Data notify implies a live context
        };
        self.alarm_epoch = self.alarm_epoch.wrapping_add(1);
        self.alarm = Some(AlarmChannel::new(AlarmChannelConfig {
            local_ref: params.alarm_ref_local,
            remote_ref: params.alarm_ref_remote,
            rta_timeout: RTA_TIMEOUT_UNIT * params.rta_timeout_factor.max(1) as u32,
            rta_retries: params.rta_retries,
            max_alarm_data_length: params.max_alarm_data_length_remote,
            peer_mac: params.initiator_mac,
            our_mac: self.setup.dcp.mac,
        }));
    }

    /// Sends every PDU, then reports every AR notification to the state-change
    /// callback and, in turn, starts or stops the RT runner: `Data` (from a fresh
    /// negotiation, not a resend) starts it, an abort back to `Idle` stops it.
    fn dispatch(
        &mut self,
        out: CmOutput,
        now: Instant,
        report: &mut StepReport,
    ) -> Result<(), DeviceError> {
        for o in out.send {
            self.rpc.send(&o.bytes, o.to)?;
            report.sent += 1;
        }
        for (state, reason) in out.notify {
            if let Some(cb) = &mut self.on_state_change {
                cb(state, reason);
            }
            match (state, reason) {
                (ArState::Data, None) => {
                    #[cfg(target_os = "linux")]
                    self.start_runner();
                    #[cfg(not(target_os = "linux"))]
                    if self.setup.rt.is_some() {
                        log::error!("cyclic RT thread is Linux-only");
                    }
                    // The alarm channel lives and dies with the AR. A controller that
                    // reconnects has forgotten every alarm we ever sent it, so the
                    // still-active diagnoses are re-announced on the fresh channel.
                    self.open_alarm_channel();
                    let epoch = self.alarm_epoch;
                    for n in self.diag.replay() {
                        let actions = self.queue_notification(n, now);
                        self.apply_alarm_actions(actions, epoch, now, report)?;
                    }
                    self.publish_diag_state();
                }
                (ArState::Idle, Some(_)) => {
                    #[cfg(target_os = "linux")]
                    self.stop_runner();
                    self.alarm = None;
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// Runner lifecycle: kept in a private, Linux-only `impl` block so the platform split
/// doesn't clutter the rest of `Device`.
#[cfg(target_os = "linux")]
impl<E: EthTransport, R: RpcTransport> Device<E, R> {
    /// Registers the function `start_runner` uses to spawn the RT thread, overriding
    /// the default [`RtRunner::spawn`]. A test/embedding hook: it's how
    /// tests hand the runner an already-open transport
    /// ([`RtRunner::spawn_with_transport`]) instead of a real `AF_PACKET` socket.
    pub fn with_runner_factory(
        &mut self,
        f: impl Fn(RtConfig) -> Result<RtHandle, RtError> + Send + 'static,
    ) {
        self.runner_factory = Box::new(f);
    }

    /// True while the RT thread is alive.
    pub fn rt_running(&self) -> bool {
        self.runner.as_ref().is_some_and(|r| r.is_running())
    }

    /// Builds the cyclic layout from the just-negotiated AR parameters, rebuilds the
    /// I/O image from it, and starts the RT thread — per [`DeviceSetup::rt`]. A
    /// `Layout` build failure or a spawn failure is logged and leaves no runner
    /// behind: the AR stays up without cyclic data, as in Plan 3.
    fn start_runner(&mut self) {
        let Some(rt) = self.setup.rt.clone() else {
            return;
        };
        let Some(params) = self.cm.context().map(|c| c.params.clone()) else {
            return; // unreachable in practice: a Data notify implies a live context
        };
        let layout = match Layout::from_ar(&params, &self.setup.model) {
            Ok(layout) => layout,
            Err(e) => {
                log::error!("cyclic layout build failed, AR stays up without cyclic data: {e}");
                return;
            }
        };
        self.image.rebuild(&layout);
        let cfg = RtConfig {
            iface: rt.iface,
            our_mac: self.setup.dcp.mac,
            cpu_mac: params.initiator_mac,
            layout,
            image: self.image.clone(),
            stats: self.stats.clone(),
            problem_indicator: self.problem.clone(),
            cpu_pin: rt.cpu_pin,
            rt_priority: rt.rt_priority,
            lock_memory: rt.lock_memory,
        };
        match (self.runner_factory)(cfg) {
            Ok(handle) => self.runner = Some(handle),
            Err(e) => log::error!("RT runner spawn failed: {e}"),
        }
    }

    /// Stops and joins the RT thread, if any, bounded so a stuck thread cannot hang
    /// the acyclic loop forever.
    ///
    /// The application must never see `Fresh` while no runner feeds the image: once
    /// joined, this publishes a validity derived from the current one with the
    /// watchdog forced to `Expired` and `provider_run` cleared, so
    /// `image().validity().freshness()` reads `Stale` until the next `Data` rebuilds
    /// the cells.
    fn stop_runner(&mut self) {
        if let Some(runner) = self.runner.take() {
            runner.stop();
            if let Err(e) = runner.join(Duration::from_millis(500)) {
                log::warn!("RT runner join timed out: {e}");
            }
            let mut v = self.image.validity();
            v.watchdog = WatchdogState::Expired;
            v.provider_run = false;
            self.image.set_validity(v);
            // Drop the stopped AR's cell index too: otherwise a caller polling
            // `ar_state() == Data` after the *next* AR reaches `Data` (before
            // `start_runner` rebuilds the image — see `dispatch`) could still read
            // stale offsets belonging to the previous AR's layout instead of
            // observing "no layout yet".
            self.image.clear();
        }
    }

    /// Drains and acts on every pending [`RtEvent`]: a watchdog or socket failure
    /// aborts the AR (which stops the runner through the `Idle` notify in
    /// `dispatch`, above); scheduling warnings and the thread's exit are only logged.
    fn drain_rt_events(
        &mut self,
        now: Instant,
        report: &mut StepReport,
    ) -> Result<(), DeviceError> {
        while let Some(ev) = self.runner.as_ref().and_then(|r| r.take_event()) {
            match ev {
                RtEvent::WatchdogExpired => {
                    log::warn!("RT consumer watchdog expired; aborting the AR");
                    self.abort_with_err_rta_in(AbortReason::RtWatchdog, now, report)?;
                }
                RtEvent::SocketError(s) => {
                    log::error!("RT socket error, aborting the AR: {s}");
                    self.abort_with_err_rta_in(AbortReason::RtSocket, now, report)?;
                }
                RtEvent::SchedWarning(s) => log::warn!("RT scheduling warning: {s}"),
                RtEvent::Exited => log::info!("RT thread exited"),
            }
        }
        Ok(())
    }
}

impl<E: EthTransport, R: RpcTransport> Drop for Device<E, R> {
    /// Stops (and bounded-joins) a still-running RT thread so dropping a `Device`
    /// cannot leak a transmitting thread and its socket.
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        self.stop_runner();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alarm::{is_alarm_frame, parse_frame, AlarmType, RtaBody, RtaData};
    use crate::cm::model::DeviceModel;
    use crate::cm::PnioStatus;
    use crate::config::{Direction, Slot};
    use crate::dcp::{DcpConfig, DeviceProperties};
    use crate::diag::{ChannelError, Diagnosis, Severity};
    use crate::eth::EthTransport;
    use crate::eth::{MacAddr, MockTransport};
    use crate::rpc::{MockRpcTransport, Uuid};
    #[cfg(target_os = "linux")]
    use crate::rt::{Freshness, RtRunner};
    #[cfg(target_os = "linux")]
    use crate::testutil::golden_rt;
    use crate::testutil::{golden, golden_alarm, RPC_OFF};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const MAC: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);

    fn setup() -> DeviceSetup {
        DeviceSetup {
            dcp: DcpConfig {
                mac: MAC,
                properties: DeviceProperties {
                    name_of_station: "rt-labs-dev".into(),
                    type_of_station: "P-Net Sample Application".into(),
                    vendor_id: 0x0493,
                    device_id: 0x0002,
                    device_role: 0x0100,
                    device_instance: 1,
                    device_options: vec![1, 2, 2, 2, 2, 3],
                    ip: [172, 16, 2, 10],
                    subnet: [255, 255, 255, 0],
                    gateway: [172, 16, 2, 10],
                    ip_block_info: 1,
                },
            },
            model: DeviceModel::pnet_sample(MAC),
            activity_seed: Uuid::parse_str("14af198a-1234-1056-8079-8cf319cd19f8").unwrap(),
            rt: None,
            im0: Im0::default(),
            im_store: None,
        }
    }

    /// The controller MAC of the `cm` Connect golden — the `initiator_mac` the alarm
    /// channel accepts frames from.
    const CPU_MAC: MacAddr = MacAddr([0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f]);

    /// A `MockTransport` behind an `Arc` so a test keeps `sent()`/`push_rx()` access
    /// after `Device::new` takes the transport by value (same pattern as the RT
    /// tests' shared mock).
    #[derive(Clone)]
    struct SharedEth(Arc<MockTransport>);

    impl SharedEth {
        fn new() -> SharedEth {
            SharedEth(Arc::new(MockTransport::new()))
        }
        fn sent(&self) -> Vec<Vec<u8>> {
            self.0.sent()
        }
        fn push_rx(&self, frame: Vec<u8>) {
            self.0.push_rx(frame);
        }
    }

    impl EthTransport for SharedEth {
        fn send(&self, frame: &[u8]) -> Result<(), TransportError> {
            self.0.send(frame)
        }
        fn recv_into(
            &self,
            buf: &mut [u8],
            timeout: Option<Duration>,
        ) -> Result<Option<usize>, TransportError> {
            self.0.recv_into(buf, timeout)
        }
    }

    /// An alarm golden retargeted to this AR's controller: the alarm capture and the
    /// `cm` Connect capture come from two bench runs whose CPU MACs differ in the last
    /// byte, and `AlarmChannel` drops frames whose source is not `initiator_mac`.
    fn cpu_alarm(name: &str) -> Vec<u8> {
        let mut f = golden_alarm(name);
        f[6..12].copy_from_slice(&CPU_MAC.0);
        f
    }

    /// Queues the four RPC goldens that drive the AR to `Data`. `seq_base` renumbers
    /// their DCE-RPC `seq_num` (LE, offset 64) so a second bring-up is not answered
    /// from `Cm`'s per-`(activity, seq_num)` response cache.
    fn feed_bring_up(
        dev: &Device<SharedEth, MockRpcTransport>,
        seq_base: Option<u32>,
        session_key: Option<u16>,
    ) {
        let cpu = "172.16.2.100:54766".parse().unwrap();
        let cpu_cm = "172.16.2.100:34964".parse().unwrap();
        for (i, name) in ["connect_req", "write_req", "prmend_req"]
            .iter()
            .enumerate()
        {
            let mut pdu = golden(name)[RPC_OFF..].to_vec();
            if let Some(base) = seq_base {
                pdu[64..68].copy_from_slice(&(base + i as u32).to_le_bytes());
            }
            // `ARBlockReq.SessionKey`: RPC header (80) + NDR (20) + block header (6)
            // + ARType (2) + ARUUID (16). A *new* key on the same ARUUID is what makes
            // `Ar` treat a Connect arriving in `Data` as a controller reconnect
            // instead of an exact retransmission.
            if let (Some(key), 0) = (session_key, i) {
                pdu[124..126].copy_from_slice(&key.to_be_bytes());
            }
            dev.rpc().push_rx(pdu, cpu);
        }
        dev.rpc()
            .push_rx(golden("appready_res")[RPC_OFF..].to_vec(), cpu_cm);
    }

    /// A device whose AR is up (`Data`), with a handle on its Ethernet transport.
    fn device_in_data() -> (Device<SharedEth, MockRpcTransport>, SharedEth) {
        let eth = SharedEth::new();
        let mut dev = Device::new(setup(), eth.clone(), MockRpcTransport::new());
        feed_bring_up(&dev, None, None);
        dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
        assert_eq!(dev.state(), ArState::Data);
        (dev, eth)
    }

    /// Re-establishes the AR from `Idle` with a fresh set of RPC sequence numbers.
    fn reconnect(dev: &mut Device<SharedEth, MockRpcTransport>) {
        feed_bring_up(dev, Some(0x100), None);
        dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    }

    fn line_break() -> Diagnosis {
        Diagnosis {
            slot: Slot(1),
            channel: 0,
            error: ChannelError::LineBreak,
            severity: Severity::Fault,
            direction: Direction::Input,
        }
    }

    #[test]
    fn diagnosis_raised_through_the_queue_hits_the_wire_and_the_problem_bit() {
        let (mut dev, eth) = device_in_data();
        let shared = dev.diag_shared();
        shared
            .queue
            .lock()
            .unwrap()
            .push_back(DiagCommand::Raise(line_break()));
        dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
        let sent = eth.sent();
        let last = sent.last().expect("a notification was sent");
        let pdu = parse_frame(last).unwrap();
        let RtaBody::Data(RtaData::Notification(n)) = pdu.body else {
            panic!("expected an AlarmNotification, got {:?}", pdu.body)
        };
        assert_eq!(n.alarm_type, AlarmType::Diagnosis);
        assert_eq!(n.slot, 1);
        assert_eq!(n.specifier.sequence, 0);
        assert_eq!(pdu.header.send_seq, 0xFFFF);
        assert!(dev.problem_indicator());
        assert!(dev.alarm_in_flight());
        assert_eq!(
            dev.poll_interval(Duration::from_millis(200)),
            Duration::from_millis(20)
        );
        assert_eq!(shared.active.lock().unwrap().len(), 1);
        assert_eq!(shared.sent.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn controller_err_rta_aborts_and_drops_the_channel() {
        let (mut dev, eth) = device_in_data();
        eth.push_rx(cpu_alarm("alarm_err_rta_cpu_removed"));
        dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
        assert_eq!(dev.state(), ArState::Idle);
        assert!(!dev.alarm_in_flight());
        assert!(
            eth.sent().iter().all(|f| !is_alarm_frame(f)),
            "no ERR-RTA answer to a controller abort"
        );
        assert_eq!(dev.diag_shared().rx_err_rta.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn shutdown_sends_err_rta_ar_removed() {
        let (mut dev, eth) = device_in_data();
        dev.shutdown(Instant::now()).unwrap();
        let last = eth.sent().last().unwrap().clone();
        let pdu = parse_frame(&last).unwrap();
        assert_eq!(
            pdu.body,
            RtaBody::Err(PnioStatus::rta_abort(PnioStatus::RTA_ABORT_AR_REMOVED))
        );
        assert_eq!(dev.state(), ArState::Idle);
        // Idempotent: a second shutdown from `Idle` sends nothing more.
        let before = eth.sent().len();
        dev.shutdown(Instant::now()).unwrap();
        assert_eq!(eth.sent().len(), before);
    }

    #[test]
    fn active_diagnosis_is_replayed_on_the_next_data() {
        let (mut dev, eth) = device_in_data();
        dev.diag_shared()
            .queue
            .lock()
            .unwrap()
            .push_back(DiagCommand::Raise(line_break()));
        dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
        eth.push_rx(cpu_alarm("alarm_err_rta_cpu_removed"));
        dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
        assert_eq!(dev.state(), ArState::Idle);
        let before = eth.sent().len();
        reconnect(&mut dev);
        assert_eq!(dev.state(), ArState::Data);
        let replayed: Vec<Vec<u8>> = eth.sent()[before..]
            .iter()
            .filter(|f| is_alarm_frame(f))
            .cloned()
            .collect();
        assert_eq!(replayed.len(), 1);
        let RtaBody::Data(RtaData::Notification(n)) = parse_frame(&replayed[0]).unwrap().body
        else {
            panic!("expected a replayed AlarmNotification")
        };
        assert_eq!(n.alarm_type, AlarmType::Diagnosis);
        assert_eq!(n.slot, 1);
        assert!(dev.problem_indicator());
    }

    /// The controller's ERR-RTA and its reconnect can land in the same `step`: the
    /// abort is collected at stage (1) but only applied at stage (7), by which time
    /// stage (3) has established a *new* AR. The channel epoch must make the stale
    /// abort a no-op instead of tearing the fresh AR down.
    #[test]
    fn stale_abort_does_not_tear_down_an_ar_rebuilt_in_the_same_step() {
        let (mut dev, eth) = device_in_data();
        eth.push_rx(cpu_alarm("alarm_err_rta_cpu_removed"));
        feed_bring_up(&dev, Some(0x200), Some(3));
        dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();

        assert_eq!(dev.state(), ArState::Data, "the reconnected AR survives");
        assert_eq!(dev.ar_params().unwrap().session_key, 3);
        assert!(!dev.alarm_in_flight());
        // ... and the channel that came with it works: a raise still reaches the wire.
        let before = eth.sent().len();
        dev.diag_shared()
            .queue
            .lock()
            .unwrap()
            .push_back(DiagCommand::Raise(line_break()));
        dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
        let fresh: Vec<Vec<u8>> = eth.sent()[before..]
            .iter()
            .filter(|f| is_alarm_frame(f))
            .cloned()
            .collect();
        assert_eq!(fresh.len(), 1);
        assert!(matches!(
            parse_frame(&fresh[0]).unwrap().body,
            RtaBody::Data(RtaData::Notification(_))
        ));
        assert!(dev.alarm_in_flight());
    }

    #[test]
    fn alarm_frames_addressed_to_another_station_are_ignored() {
        let (mut dev, eth) = device_in_data();
        let mut foreign = cpu_alarm("alarm_err_rta_cpu_removed");
        foreign[..6].copy_from_slice(&[0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xff]);
        eth.push_rx(foreign);
        dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
        assert_eq!(dev.state(), ArState::Data);
        let shared = dev.diag_shared();
        assert_eq!(shared.rx_no_channel.load(Ordering::Relaxed), 0);
        assert_eq!(shared.rx_err_rta.load(Ordering::Relaxed), 0);
        assert_eq!(shared.unexpected_rx.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn alarm_frames_without_a_channel_are_counted_and_dropped() {
        let eth = MockTransport::new();
        eth.push_rx(golden_alarm("alarm_ack_high_cpu"));
        let mut dev = Device::new(setup(), eth, MockRpcTransport::new());
        let r = dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
        assert_eq!(r.eth_frames, 1);
        assert_eq!(dev.eth().sent(), Vec::<Vec<u8>>::new());
        assert_eq!(dev.diag_shared().rx_no_channel.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn full_bring_up_through_the_loop() {
        let eth = MockTransport::new();
        let rpc = MockRpcTransport::new();
        let cpu = "172.16.2.100:54766".parse().unwrap();
        let cpu_cm = "172.16.2.100:34964".parse().unwrap();
        eth.push_rx(golden("dcp_set_req"));
        rpc.push_rx(golden("connect_req")[RPC_OFF..].to_vec(), cpu);
        rpc.push_rx(golden("write_req")[RPC_OFF..].to_vec(), cpu);
        rpc.push_rx(golden("prmend_req")[RPC_OFF..].to_vec(), cpu);
        rpc.push_rx(golden("appready_res")[RPC_OFF..].to_vec(), cpu_cm);
        let mut dev = Device::new(setup(), eth, rpc);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        dev.on_state_change(move |st, why| s2.lock().unwrap().push((st, why)));
        let r = dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
        assert_eq!((r.eth_frames, r.rpc_datagrams), (1, 4));
        assert_eq!(dev.state(), ArState::Data);
        assert_eq!(dev.eth().sent(), vec![golden("dcp_set_res")]);
        let sent = dev.rpc().sent();
        assert_eq!(sent.len(), 4);
        assert_eq!(sent[0].0, golden("connect_res")[RPC_OFF..]);
        assert_eq!(sent[3].0, golden("appready_req")[RPC_OFF..]);
        assert_eq!(sent[3].1, cpu_cm);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![(ArState::Connected, None), (ArState::Data, None)]
        );
    }

    #[test]
    fn garbage_is_dropped_and_loop_continues() {
        let eth = MockTransport::new();
        let rpc = MockRpcTransport::new();
        let cpu = "172.16.2.100:54766".parse().unwrap();
        rpc.push_rx(vec![1, 2, 3], cpu);
        rpc.push_rx(golden("connect_req")[RPC_OFF..].to_vec(), cpu);
        let mut dev = Device::new(setup(), eth, rpc);
        dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
        assert_eq!(dev.state(), ArState::Connected);
    }

    #[test]
    fn run_returns_when_flag_is_set_from_another_thread() {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut dev = Device::new(setup(), MockTransport::new(), MockRpcTransport::new());
        let flipper = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(30));
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
            })
        };
        dev.run(&stop).unwrap();
        flipper.join().unwrap();
    }

    /// Wraps a `MockRpcTransport` for `recv`, but always fails `send` — used to prove
    /// that a transport I/O error during `dispatch` is not swallowed by `step`.
    struct FailingRpc(MockRpcTransport);

    impl RpcTransport for FailingRpc {
        fn send(&self, _buf: &[u8], _to: std::net::SocketAddr) -> Result<(), RpcError> {
            Err(RpcError::Io(std::io::Error::other("boom")))
        }
        fn recv(
            &self,
            timeout: Option<Duration>,
        ) -> Result<Option<(Vec<u8>, std::net::SocketAddr)>, RpcError> {
            self.0.recv(timeout)
        }
    }

    #[test]
    fn transport_error_propagates_out_of_run() {
        let eth = MockTransport::new();
        let rpc = FailingRpc(MockRpcTransport::new());
        let cpu = "172.16.2.100:54766".parse().unwrap();
        rpc.0
            .push_rx(golden("connect_req")[RPC_OFF..].to_vec(), cpu);
        let mut dev = Device::new(setup(), eth, rpc);
        let err = dev.step(Instant::now(), Some(Duration::ZERO)).unwrap_err();
        assert!(matches!(err, DeviceError::Rpc(_)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn data_starts_the_runner_and_idle_stops_it() {
        let eth = MockTransport::new();
        let rpc = MockRpcTransport::new();
        let cpu = "172.16.2.100:54766".parse().unwrap();
        let cpu_cm = "172.16.2.100:34964".parse().unwrap();
        rpc.push_rx(golden("connect_req")[RPC_OFF..].to_vec(), cpu);
        rpc.push_rx(golden("prmend_req")[RPC_OFF..].to_vec(), cpu);
        rpc.push_rx(golden("appready_res")[RPC_OFF..].to_vec(), cpu_cm);
        let mut s = setup();
        s.rt = Some(RtOptions {
            iface: "mock".into(),
            cpu_pin: None,
            rt_priority: None,
            lock_memory: false,
        });
        let mut dev = Device::new(s, eth, rpc);
        dev.with_runner_factory(|cfg| RtRunner::spawn_with_transport(cfg, MockTransport::new()));
        dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
        assert_eq!(dev.state(), ArState::Data);
        assert!(dev.rt_running());
        assert_eq!(dev.image().cells().len(), 7);
        // controller Release -> Idle -> runner stopped
        let mut rel = golden("prmend_req")[RPC_OFF..].to_vec();
        rel[68] = 1; // opnum Release (LE low byte)
        rel[64] = 9; // new seq_num
        rel[100] = 0x01;
        rel[101] = 0x14; // block type ReleaseBlockReq
                         // command field: RPC header (80) + NDR (20) = block at 100; block header (6) +
                         // reserved (2) + ar_uuid (16) + session_key (2) + reserved (2) = command at 128.
        rel[128] = 0x00;
        rel[129] = 0x04; // command Release
        dev.rpc().push_rx(rel, cpu);
        dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
        assert_eq!(dev.state(), ArState::Idle);
        assert!(!dev.rt_running());
        // A clean release must not leave the image `Fresh` over frozen outputs.
        assert_eq!(dev.image().validity().freshness(), Freshness::Stale);
        // ... nor keep the stopped AR's cell index around: the next AR's `Data`
        // must not be servable against a stale layout for the window between the
        // state-change notify and `start_runner` rebuilding the image.
        assert!(dev.image().cells().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn watchdog_event_aborts_the_ar() {
        let eth = MockTransport::new();
        let rpc = MockRpcTransport::new();
        let cpu = "172.16.2.100:54766".parse().unwrap();
        let cpu_cm = "172.16.2.100:34964".parse().unwrap();
        rpc.push_rx(golden("connect_req")[RPC_OFF..].to_vec(), cpu);
        rpc.push_rx(golden("prmend_req")[RPC_OFF..].to_vec(), cpu);
        rpc.push_rx(golden("appready_res")[RPC_OFF..].to_vec(), cpu_cm);
        let mut s = setup();
        s.rt = Some(RtOptions {
            iface: "mock".into(),
            cpu_pin: None,
            rt_priority: None,
            lock_memory: false,
        });
        let mut dev = Device::new(s, eth, rpc);
        // Shrink the cyclic period and the output watchdog so the runner's watchdog
        // fires quickly against a mock transport fed a single CPU frame: no further
        // frames arrive after it, so the consumer watchdog trips a few cycles later.
        dev.with_runner_factory(|mut cfg| {
            cfg.layout.input_cr.cycle_step = 160;
            cfg.layout.output_cr.cycle_step = 160;
            cfg.layout.output_cr.watchdog = Duration::from_millis(10);
            let mock = MockTransport::new();
            mock.push_rx(golden_rt("rtc_cpu_8001"));
            RtRunner::spawn_with_transport(cfg, mock)
        });
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        dev.on_state_change(move |st, why| s2.lock().unwrap().push((st, why)));

        dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
        assert_eq!(dev.state(), ArState::Data);
        assert!(dev.rt_running());

        std::thread::sleep(Duration::from_millis(60));
        dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();

        assert_eq!(dev.state(), ArState::Idle);
        assert_eq!(
            seen.lock().unwrap().last(),
            Some(&(ArState::Idle, Some(AbortReason::RtWatchdog)))
        );
        assert!(!dev.rt_running());
        // ... announced on the alarm channel as "DHT watchdog expired" first.
        let last = dev
            .eth()
            .sent()
            .last()
            .expect("an ERR-RTA was sent")
            .clone();
        assert_eq!(
            parse_frame(&last).unwrap().body,
            RtaBody::Err(PnioStatus::rta_abort(PnioStatus::RTA_ABORT_DHT_WDT_EXPIRED))
        );
    }

    /// Always fails `send`, so the RT thread's very first tick reports
    /// `RtEvent::SocketError` and exits, through the real event path — same
    /// mechanism as `watchdog_event_aborts_the_ar`, but for the socket-failure
    /// branch of `drain_rt_events` rather than the consumer-watchdog one.
    struct FailingTransport;

    impl EthTransport for FailingTransport {
        fn send(&self, _frame: &[u8]) -> Result<(), TransportError> {
            Err(TransportError::Io(std::io::Error::other("boom")))
        }
        fn recv_into(
            &self,
            _buf: &mut [u8],
            _timeout: Option<Duration>,
        ) -> Result<Option<usize>, TransportError> {
            Ok(None)
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn socket_error_event_aborts_the_ar_with_rt_socket() {
        let eth = MockTransport::new();
        let rpc = MockRpcTransport::new();
        let cpu = "172.16.2.100:54766".parse().unwrap();
        let cpu_cm = "172.16.2.100:34964".parse().unwrap();
        rpc.push_rx(golden("connect_req")[RPC_OFF..].to_vec(), cpu);
        rpc.push_rx(golden("prmend_req")[RPC_OFF..].to_vec(), cpu);
        rpc.push_rx(golden("appready_res")[RPC_OFF..].to_vec(), cpu_cm);
        let mut s = setup();
        s.rt = Some(RtOptions {
            iface: "mock".into(),
            cpu_pin: None,
            rt_priority: None,
            lock_memory: false,
        });
        let mut dev = Device::new(s, eth, rpc);
        dev.with_runner_factory(|cfg| RtRunner::spawn_with_transport(cfg, FailingTransport));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        dev.on_state_change(move |st, why| s2.lock().unwrap().push((st, why)));

        dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
        assert_eq!(dev.state(), ArState::Data);
        assert!(dev.rt_running());

        // Give the RT thread a chance to take its first tick against the always-
        // failing transport and push `SocketError` before the next `step` drains it.
        std::thread::sleep(Duration::from_millis(60));
        dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();

        assert_eq!(dev.state(), ArState::Idle);
        assert_eq!(
            seen.lock().unwrap().last(),
            Some(&(ArState::Idle, Some(AbortReason::RtSocket)))
        );
        assert!(!dev.rt_running());
        // ... announced on the alarm channel as "AR removed" first.
        let last = dev
            .eth()
            .sent()
            .last()
            .expect("an ERR-RTA was sent")
            .clone();
        assert_eq!(
            parse_frame(&last).unwrap().body,
            RtaBody::Err(PnioStatus::rta_abort(PnioStatus::RTA_ABORT_AR_REMOVED))
        );
    }

    /// A pipe's read end that is never written to: `poll(2)` waits the full timeout on it.
    struct NeverReadable(std::os::fd::RawFd);
    impl NeverReadable {
        fn new() -> Self {
            let mut fds = [0; 2];
            // Safety: `fds` is a valid 2-element array; `pipe` fills it or fails.
            let r = unsafe { libc::pipe(fds.as_mut_ptr()) };
            assert_eq!(r, 0, "pipe");
            NeverReadable(fds[0])
        }
    }

    /// `SharedEth` that also exposes a (never readable) fd, so `Device::step` really polls.
    struct PollableEth(SharedEth, std::os::fd::RawFd);
    impl EthTransport for PollableEth {
        fn send(&self, frame: &[u8]) -> Result<(), TransportError> {
            self.0.send(frame)
        }
        fn recv_into(
            &self,
            buf: &mut [u8],
            timeout: Option<Duration>,
        ) -> Result<Option<usize>, TransportError> {
            self.0.recv_into(buf, timeout)
        }
        fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
            Some(self.1)
        }
    }
    struct PollableRpc(MockRpcTransport, std::os::fd::RawFd);
    impl RpcTransport for PollableRpc {
        fn send(&self, buf: &[u8], to: std::net::SocketAddr) -> Result<(), RpcError> {
            self.0.send(buf, to)
        }
        fn recv(
            &self,
            timeout: Option<Duration>,
        ) -> Result<Option<(Vec<u8>, std::net::SocketAddr)>, RpcError> {
            self.0.recv(timeout)
        }
        fn raw_fd(&self) -> Option<std::os::fd::RawFd> {
            Some(self.1)
        }
    }

    /// HIL 2026-08-31 (§6i): a notification enqueued after a long poll wait was dated with
    /// the `now` the caller took *before* the wait, so its retry deadline was already expired
    /// at the next step and the CPU received the frame twice, 37 µs apart. The poll wait must
    /// not age the alarm's deadline.
    #[test]
    fn poll_wait_does_not_age_the_alarm_retry_deadline() {
        let eth = SharedEth::new();
        let pipe = NeverReadable::new();
        let mut dev = Device::new(
            setup(),
            PollableEth(eth.clone(), pipe.0),
            PollableRpc(MockRpcTransport::new(), pipe.0),
        );
        let cpu = "172.16.2.100:54766".parse().unwrap();
        let cpu_cm = "172.16.2.100:34964".parse().unwrap();
        for name in ["connect_req", "write_req", "prmend_req"] {
            dev.rpc().0.push_rx(golden(name)[RPC_OFF..].to_vec(), cpu);
        }
        dev.rpc()
            .0
            .push_rx(golden("appready_res")[RPC_OFF..].to_vec(), cpu_cm);
        dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
        assert_eq!(dev.state(), ArState::Data);

        dev.diag_shared()
            .queue
            .lock()
            .unwrap()
            .push_back(DiagCommand::Raise(Diagnosis {
                slot: Slot(1),
                channel: 0,
                error: ChannelError::LineBreak,
                severity: Severity::Fault,
                direction: Direction::Input,
            }));
        // `now` taken before a 150 ms poll wait (the RTA timeout is 100 ms on this AR).
        let t0 = Instant::now();
        dev.step(t0, Some(Duration::from_millis(150))).unwrap();
        assert!(
            t0.elapsed() >= Duration::from_millis(140),
            "the poll must have waited"
        );
        let alarms_after_send = eth
            .sent()
            .iter()
            .filter(|f| crate::alarm::is_alarm_frame(f))
            .count();
        assert_eq!(alarms_after_send, 1, "one notification sent");

        // Immediately afterwards: nothing is due yet (the CPU has ~100 ms to ACK).
        dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
        let alarms_now = eth
            .sent()
            .iter()
            .filter(|f| crate::alarm::is_alarm_frame(f))
            .count();
        assert_eq!(alarms_now, 1, "no premature retransmission");
        assert_eq!(
            dev.diag_shared().retries.load(Ordering::Relaxed),
            0,
            "no retry counted"
        );
    }
}

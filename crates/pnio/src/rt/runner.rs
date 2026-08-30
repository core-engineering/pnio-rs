//! The RT thread: the only place in the stack that owns a clock, a socket and a
//! priority.
//!
//! [`RtRunner::spawn`] starts a thread that drives the pure pieces built by the rest
//! of `rt`: a `timerfd` paces the send cycle, [`RtEngine`] produces and consumes the
//! frames, and [`IoImage`] is the non-blocking hand-off with the application. The
//! thread never allocates and never logs once it is running — it reports out-of-band
//! conditions as [`RtEvent`]s on a queue backed by an `eventfd`, so the acyclic side
//! can wait on `event_fd()` in its own `poll` and do the logging.
//!
//! Nothing is allocated once the loop runs: the RX buffer ([`MAX_FRAME_LEN`] bytes),
//! the TX frame, the input snapshot, the poll set and the event queue capacity are
//! all set up before it starts.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::engine::{RtEngine, RtStats, RxVerdict, WatchdogVerdict};
use super::image::{IoImage, Validity, WatchdogState};
use super::layout::Layout;
use super::sched;
use super::RtError;
use crate::eth::poll::{poll_readable_into, wait_readable};
use crate::eth::AfPacketTransport;
use crate::eth::{EthTransport, MacAddr, TransportError, MAX_FRAME_LEN};

/// Upper bound on frames consumed in one receive pass, so a flooded socket cannot
/// starve the send cycle: whatever is left waits for the next pass.
const MAX_RX_PER_PASS: usize = 64;

/// Upper bound on consecutive main-loop iterations in which `poll` reported
/// nothing readable and neither a tick nor a frame was processed, before the
/// loop treats itself as stuck (e.g. a persistent `POLLERR`/`POLLNVAL` on a bad
/// fd, which `poll_readable_into` reports as "readable" precisely so the loop
/// reaches the failing call and observes the error — but a bug elsewhere could
/// still make this spin) and exits with [`RtEvent::SocketError`] instead of
/// burning 100% CPU forever.
const MAX_NO_PROGRESS_ITERATIONS: u32 = 1000;

/// Everything the RT thread needs to run one AR.
pub struct RtConfig {
    /// Interface to open in [`RtRunner::spawn`] (ignored by
    /// [`RtRunner::spawn_with_transport`]).
    pub iface: String,
    /// Source MAC we stamp on the frames we produce.
    pub our_mac: MacAddr,
    /// The controller's MAC: the only source we accept frames from.
    pub cpu_mac: MacAddr,
    /// C-SDU plan for both CRs; `input_cr.period()` also paces the timer.
    pub layout: Layout,
    /// Shared I/O image: inputs from the application, outputs to it.
    pub image: Arc<IoImage>,
    /// Counters, shared with the engine and readable from any thread.
    pub stats: Arc<RtStats>,
    /// Station problem indicator, shared with the acyclic side: `true` clears bit 5
    /// (`Station_Problem_Indicator`) of the data status on every produced frame.
    pub problem_indicator: Arc<AtomicBool>,
    /// Pin the RT thread to this CPU, if set.
    pub cpu_pin: Option<usize>,
    /// Run the RT thread at this `SCHED_FIFO` priority, if set.
    pub rt_priority: Option<u8>,
    /// Lock the process memory (`mlockall`) and pre-fault the RT stack before the loop.
    pub lock_memory: bool,
}

/// Out-of-band conditions reported by the RT thread, drained by the acyclic side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtEvent {
    /// The consumer watchdog expired (reported once per gap, like the engine).
    WatchdogExpired,
    /// The socket failed; the thread stops right after this event.
    SocketError(String),
    /// Real-time scheduling or CPU pinning could not be applied; the thread runs on
    /// anyway, at whatever priority it already had.
    SchedWarning(String),
    /// Last event of the thread's life, pushed even if the loop panicked.
    Exited,
}

/// State shared between the RT thread and its [`RtHandle`].
struct RtShared {
    stop: AtomicBool,
    running: AtomicBool,
    events: Mutex<VecDeque<RtEvent>>,
    /// Readable while at least one event is queued.
    event_fd: OwnedFd,
    /// Written by [`RtHandle::stop`] only, to break the thread's `poll`.
    wake_fd: OwnedFd,
    stats: Arc<RtStats>,
}

impl RtShared {
    fn push_event(&self, event: RtEvent) {
        let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        events.push_back(event);
        signal_eventfd(self.event_fd.as_raw_fd());
    }

    fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        signal_eventfd(self.wake_fd.as_raw_fd());
    }
}

/// Pushes `Exited` and clears `running` when the thread body ends, panic included.
struct ExitGuard<'a>(&'a RtShared);

impl Drop for ExitGuard<'_> {
    fn drop(&mut self) {
        self.0.push_event(RtEvent::Exited);
        self.0.running.store(false, Ordering::Release);
    }
}

/// Handle on a running RT thread: stop it, wait for it, read its events and counters.
///
/// Dropping the handle stops the thread (see the `Drop` impl below), so a
/// discarded handle cannot leak a transmitting thread and its socket. Drop does
/// not join, though — it only requests the stop and returns immediately; call
/// [`RtHandle::join`] first if the thread must actually be gone before moving on.
pub struct RtHandle {
    shared: Arc<RtShared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for RtHandle {
    /// Stops the thread if it is still running when the handle is dropped.
    /// Idempotent (`stop()` on an already-stopped thread is a no-op) and never
    /// blocks — a `Drop` impl must not join.
    fn drop(&mut self) {
        self.stop();
    }
}

impl RtHandle {
    /// Ask the thread to stop and wake it out of its `poll`. Returns immediately;
    /// use [`RtHandle::join`] to wait for the thread to actually exit.
    pub fn stop(&self) {
        self.shared.request_stop();
    }

    /// Wait up to `timeout` for the thread to exit, polling `is_finished()` every
    /// millisecond.
    ///
    /// Returns `Err(RtError::Stopped)` if it is still running when `timeout`
    /// elapses; the thread is then left detached (it holds only its own `Arc`s and
    /// will release them when it does exit). Takes `&self` — not `self` — so events
    /// and counters stay readable after the thread is gone.
    pub fn join(&self, timeout: Duration) -> Result<(), RtError> {
        let deadline = Instant::now() + timeout;
        loop {
            let mut slot = self.thread.lock().unwrap_or_else(|e| e.into_inner());
            match slot.as_ref() {
                None => return Ok(()),
                Some(handle) if handle.is_finished() => {
                    let handle = slot.take().expect("checked Some just above");
                    drop(slot);
                    let _ = handle.join(); // a panic in the RT thread is not ours to resume
                    return Ok(());
                }
                Some(_) => drop(slot),
            }
            if Instant::now() >= deadline {
                return Err(RtError::Stopped);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// An fd that is readable while at least one event is queued, so a caller can
    /// wait on the RT thread inside its own `poll(2)` loop.
    pub fn event_fd(&self) -> RawFd {
        self.shared.event_fd.as_raw_fd()
    }

    /// Pop the oldest pending event, clearing [`RtHandle::event_fd`] once the queue
    /// runs empty.
    pub fn take_event(&self) -> Option<RtEvent> {
        let mut events = self.shared.events.lock().unwrap_or_else(|e| e.into_inner());
        let event = events.pop_front();
        if events.is_empty() {
            drain_eventfd(self.shared.event_fd.as_raw_fd());
        }
        event
    }

    /// The counters the RT thread updates.
    pub fn stats(&self) -> Arc<RtStats> {
        self.shared.stats.clone()
    }

    /// False once the thread body has ended.
    pub fn is_running(&self) -> bool {
        self.shared.running.load(Ordering::Acquire)
    }
}

/// Spawns the RT thread.
pub struct RtRunner;

impl RtRunner {
    /// Open `cfg.iface` and start the RT thread on it.
    ///
    /// The socket is opened in the *calling* thread so a bad interface name or a
    /// missing `CAP_NET_RAW` comes back as an error here rather than as an event
    /// from a thread that immediately dies.
    pub fn spawn(cfg: RtConfig) -> Result<RtHandle, RtError> {
        let transport = AfPacketTransport::open(&cfg.iface)?;
        // The RT socket must never wake up for DCP or alarms: an unfiltered run is
        // not comparable, so a filter failure is fatal here, not a warning.
        transport.attach_filter(&crate::eth::bpf::rt_filter())?;
        Self::spawn_with_transport(cfg, transport)
    }

    /// Start the RT thread on an already-open transport.
    ///
    /// A transport without a [`EthTransport::raw_fd`] (the in-memory mock) cannot be
    /// polled, so it is drained with a zero-timeout `recv` after every tick instead.
    pub fn spawn_with_transport<T: EthTransport + 'static>(
        cfg: RtConfig,
        transport: T,
    ) -> Result<RtHandle, RtError> {
        // Created here, unarmed, so a `timerfd_create` failure is an early
        // `RtError` in the *calling* thread; arming happens in `run_loop`, after
        // the RT thread's own setup, so the timer's first period starts only once
        // the thread is actually ready to service it (see `arm_timerfd`).
        let timer = new_timerfd()?;
        let shared = Arc::new(RtShared {
            stop: AtomicBool::new(false),
            running: AtomicBool::new(true),
            events: Mutex::new(VecDeque::with_capacity(16)),
            event_fd: new_eventfd()?,
            wake_fd: new_eventfd()?,
            stats: cfg.stats.clone(),
        });

        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("pnio-rt".to_string())
            .spawn(move || {
                let _exit = ExitGuard(&thread_shared);
                run_loop(cfg, transport, timer, &thread_shared);
            })?;

        Ok(RtHandle {
            shared,
            thread: Mutex::new(Some(thread)),
        })
    }
}

/// A publish the image could not take yet, retried on the next tick.
#[derive(Clone, Copy)]
enum Pending {
    /// Data plus validity, from an accepted frame.
    Data(Validity),
    /// Validity only, from a watchdog verdict.
    Validity(Validity),
}

/// Ideal tick grid anchored on the first wake-up: lateness = now − (start + period × n).
///
/// Pure: it owns no clock of its own and does not call `Instant::now()` — the caller
/// hands it `now` from each timer read.
struct TickGrid {
    period: Duration,
    start: Option<Instant>,
    ticks: u64,
}

impl TickGrid {
    fn new(period: Duration) -> Self {
        Self {
            period,
            start: None,
            ticks: 0,
        }
    }

    /// Account for one timer read of `expirations` ticks at `now`; returns the
    /// lateness of this wake against the ideal grid, in nanoseconds. The first read
    /// anchors the grid (lateness 0) and does not count its extra expirations —
    /// they are the setup time between arming the timer and the first read, not
    /// missed cycles on the wire.
    fn on_read(&mut self, now: Instant, expirations: u64) -> u64 {
        match self.start {
            None => {
                self.start = Some(now);
                self.ticks = 1;
                0
            }
            Some(start) => {
                self.ticks = self.ticks.saturating_add(expirations);
                let offset = Duration::from_nanos(
                    (self.period.as_nanos() as u64).saturating_mul(self.ticks.saturating_sub(1)),
                );
                match start.checked_add(offset) {
                    Some(expected) => now.saturating_duration_since(expected).as_nanos() as u64,
                    // Only reachable after ~584 years of continuous uptime at 1 ms
                    // resolution; not worth panicking the RT thread over.
                    None => 0,
                }
            }
        }
    }

    /// Total ticks since the grid was anchored, used for `Validity.cycle`.
    fn ticks(&self) -> u64 {
        self.ticks
    }
}

/// The RT thread body. Sets up scheduling, then loops until `stop` is requested or
/// the socket fails. Allocates only before the loop and on the way out (the event
/// strings); never logs.
fn run_loop<T: EthTransport>(cfg: RtConfig, transport: T, timer: OwnedFd, shared: &RtShared) {
    let RtConfig {
        our_mac,
        cpu_mac,
        layout,
        image,
        stats,
        problem_indicator,
        cpu_pin,
        rt_priority,
        lock_memory,
        ..
    } = cfg;

    // --- setup: allocation, formatting and warnings all happen before the loop ---
    if let Some(cpu) = cpu_pin {
        if let Err(e) = sched::set_affinity(&[cpu]) {
            shared.push_event(RtEvent::SchedWarning(format!("CPU pin {cpu}: {e}")));
        }
    }
    if let Some(priority) = rt_priority {
        if let Err(e) = sched::set_fifo(priority) {
            shared.push_event(RtEvent::SchedWarning(format!(
                "SCHED_FIFO priority {priority}: {e}"
            )));
        }
    }
    if lock_memory {
        // Pre-fault the stack unconditionally: it is useful even when `mlockall`
        // itself fails (the pages still get touched once, avoiding a page fault on
        // the RT path), so a failed lock does not skip it.
        if let Err(e) = sched::lock_memory() {
            shared.push_event(RtEvent::SchedWarning(format!("mlockall: {e}")));
        }
        sched::prefault_stack();
    }

    let period = layout.input_cr.period();
    let mut snapshot = vec![0u8; layout.input_cr.data_length];
    let mut rx_buf = [0u8; MAX_FRAME_LEN];
    let mut engine = RtEngine::new(
        layout,
        our_mac,
        cpu_mac,
        Arc::clone(&stats),
        Arc::clone(&problem_indicator),
    );

    let socket_fd = transport.raw_fd();
    let mut fds = [timer.as_raw_fd(), shared.wake_fd.as_raw_fd(), 0];
    let nfds = match socket_fd {
        Some(fd) => {
            fds[2] = fd;
            3
        }
        None => 2,
    };
    let mut ready = [false; 3];

    let mut grid = TickGrid::new(period);
    let mut pending: Option<Pending> = None;
    // Consecutive iterations with neither a tick nor a frame processed; see
    // [`MAX_NO_PROGRESS_ITERATIONS`].
    let mut no_progress: u32 = 0;

    // Arm the cycle timer only now, after the setup above (affinity, SCHED_FIFO,
    // mlockall, stack pre-fault) is done and immediately before the loop starts:
    // arming any earlier risks the first `read_timer` reporting several
    // expirations that are really just this setup's own duration, which
    // `RtEngine::on_tick` would otherwise count as missed ticks that never
    // happened on the wire (see `TickGrid` and `new_timerfd`/`arm_timerfd`).
    if let Err(e) = arm_timerfd(timer.as_raw_fd(), period) {
        shared.push_event(RtEvent::SocketError(format!("timerfd arm: {e}")));
        return;
    }

    while !shared.stop.load(Ordering::Acquire) {
        if let Err(e) = poll_readable_into(&fds[..nfds], &mut ready[..nfds], None) {
            shared.push_event(RtEvent::SocketError(format!("poll: {e}")));
            break;
        }

        if ready[1] {
            drain_eventfd(shared.wake_fd.as_raw_fd()); // stop requested: the loop condition acts
        }

        // Set as soon as a tick or a frame is actually processed; drives
        // `no_progress` below.
        let mut progressed = false;

        let mut ticked = false;
        if ready[0] {
            match read_timer(timer.as_raw_fd()) {
                Err(e) => {
                    shared.push_event(RtEvent::SocketError(format!("timerfd read: {e}")));
                    break;
                }
                Ok(0) => {} // not due yet: a spurious wakeup, or EAGAIN folded to 0
                Ok(expirations) => {
                    ticked = true;
                    progressed = true;
                    let now = Instant::now();
                    // The very first genuine tick anchors `TickGrid`, so the engine
                    // must not see its raw `expirations` either: any extra count
                    // there is the same setup time the grid itself discards, not
                    // missed cycles on the wire.
                    let is_first_read = grid.ticks() == 0;
                    let lateness = grid.on_read(now, expirations);
                    stats.tick_lateness.record(lateness);
                    let ticks = grid.ticks();

                    if let Some(p) = pending.take() {
                        retry_pending(p, &engine, &image, &stats, &mut pending);
                    }

                    if engine.check_watchdog(now) == WatchdogVerdict::Expired {
                        // The controller stopped providing: the application's view of
                        // output validity goes BAD until it talks again (our IOCS bytes
                        // are unaffected — they stay GOOD, see RtEngine::on_tick).
                        engine.mark_outputs_stale();
                        shared.push_event(RtEvent::WatchdogExpired);
                        let validity = Validity {
                            provider_run: engine.provider_run(),
                            primary: engine.primary(),
                            watchdog: WatchdogState::Expired,
                            last_rx_age: engine.last_rx().map(|t| now.saturating_duration_since(t)),
                            cycle: ticks,
                        };
                        if !image.rt_set_validity(validity) {
                            stats
                                .output_publish_deferred
                                .fetch_add(1, Ordering::Relaxed);
                            pending = Some(Pending::Validity(validity));
                        }
                    }

                    if !image.rt_snapshot_inputs(&mut snapshot) {
                        stats.input_snapshot_reused.fetch_add(1, Ordering::Relaxed);
                    }
                    // For the first read, pass 1 regardless of the raw expiration
                    // count: see the comment on `is_first_read` above.
                    let engine_expirations = if is_first_read {
                        1
                    } else {
                        u32::try_from(expirations).unwrap_or(u32::MAX)
                    };
                    let frame = engine.on_tick(engine_expirations, &snapshot);
                    if let Err(e) = transport.send(frame) {
                        shared.push_event(RtEvent::SocketError(format!("send: {e}")));
                        break;
                    }
                    stats.cycle_work.record(now.elapsed().as_nanos() as u64);
                }
            }
        }

        // A polled socket is drained when it says so; a transport with no fd is
        // drained once per tick instead.
        let drain = if nfds == 3 { ready[2] } else { ticked };
        if drain {
            let mut drained_frame = false;
            if !drain_rx(
                &transport,
                &mut rx_buf,
                socket_fd,
                &mut engine,
                &image,
                &stats,
                grid.ticks(),
                &mut pending,
                shared,
                &mut drained_frame,
            ) {
                break;
            }
            progressed = progressed || drained_frame;
        }

        if progressed {
            no_progress = 0;
        } else {
            no_progress = no_progress.saturating_add(1);
            if no_progress >= MAX_NO_PROGRESS_ITERATIONS {
                shared.push_event(RtEvent::SocketError("poll made no progress".to_string()));
                break;
            }
        }
    }
}

/// Pure decision table pinning the "keep draining" policy of [`drain_rx`].
///
/// - `has_fd`: the transport exposes a raw fd, so a zero-timeout
///   [`wait_readable`] can be checked *before* every `recv`.
/// - `readable`: that pre-check's result (only consulted when `has_fd`).
/// - `got_frame`: the `recv` call that just ran returned `Ok(Some(_))` rather
///   than `Ok(None)` (only consulted when `!has_fd`).
///
/// With a raw fd, the pre-check's readability is the only signal: a queue that
/// is still readable keeps being drained even when the frame `recv` just
/// handed back was skipped — our own looped-back `PACKET_OUTGOING` frame, or a
/// non-PROFINET one — because on a live NIC that is the *usual* case, not the
/// end of the queue. Without a fd to probe ahead of time (the in-memory mock),
/// the only signal is whether `recv` actually handed back a frame; it stops as
/// soon as it does not.
fn drain_should_continue(has_fd: bool, readable: bool, got_frame: bool) -> bool {
    if has_fd {
        readable
    } else {
        got_frame
    }
}

/// Consume up to [`MAX_RX_PER_PASS`] frames. Returns false if the socket failed (the
/// event is already pushed and the caller must leave the loop). Sets
/// `*processed_frame` if at least one frame actually arrived (`recv` returned
/// `Ok(Some(_))`), whether or not the engine went on to accept it — that is the
/// signal the caller's no-progress counter cares about.
#[allow(clippy::too_many_arguments)]
fn drain_rx<T: EthTransport>(
    transport: &T,
    rx_buf: &mut [u8; MAX_FRAME_LEN],
    socket_fd: Option<RawFd>,
    engine: &mut RtEngine,
    image: &IoImage,
    stats: &RtStats,
    ticks: u64,
    pending: &mut Option<Pending>,
    shared: &RtShared,
    processed_frame: &mut bool,
) -> bool {
    let has_fd = socket_fd.is_some();
    for _ in 0..MAX_RX_PER_PASS {
        if let Some(fd) = socket_fd {
            match wait_readable(fd, Some(Duration::ZERO)) {
                Ok(true) => {}
                Ok(false) => break, // queue empty
                Err(e) => {
                    shared.push_event(RtEvent::SocketError(format!("poll: {e}")));
                    return false;
                }
            }
        }

        let got_frame = match transport.recv_into(rx_buf, Some(Duration::ZERO)) {
            Ok(None) => false,
            Ok(Some(n)) => {
                *processed_frame = true;
                let now = Instant::now();
                let prev_rx = engine.last_rx();
                if let RxVerdict::Accepted { .. } = engine.on_frame(&rx_buf[..n], now) {
                    if let Some(prev) = prev_rx {
                        stats
                            .rx_interval
                            .record(now.saturating_duration_since(prev).as_nanos() as u64);
                    }
                    let validity = Validity {
                        provider_run: engine.provider_run(),
                        primary: engine.primary(),
                        watchdog: WatchdogState::Ok,
                        last_rx_age: Some(Duration::ZERO),
                        cycle: ticks,
                    };
                    if image.rt_publish(engine.rx_csdu(), validity) {
                        *pending = None;
                    } else {
                        stats
                            .output_publish_deferred
                            .fetch_add(1, Ordering::Relaxed);
                        *pending = Some(Pending::Data(validity));
                    }
                }
                true
            }
            // An oversized frame is not ours to consume and not a socket failure:
            // count it and keep draining.
            Err(TransportError::FrameTooLong { .. }) => {
                stats.rx_invalid.fetch_add(1, Ordering::Relaxed);
                *processed_frame = true;
                true
            }
            Err(e) => {
                shared.push_event(RtEvent::SocketError(format!("recv: {e}")));
                return false;
            }
        };

        if !drain_should_continue(has_fd, true, got_frame) {
            break;
        }
    }
    true
}

/// Retry one deferred publish; if the image is still busy it stays pending.
fn retry_pending(
    p: Pending,
    engine: &RtEngine,
    image: &IoImage,
    stats: &RtStats,
    pending: &mut Option<Pending>,
) {
    let taken = match p {
        Pending::Data(v) => image.rt_publish(engine.rx_csdu(), v),
        Pending::Validity(v) => image.rt_set_validity(v),
    };
    if !taken {
        stats
            .output_publish_deferred
            .fetch_add(1, Ordering::Relaxed);
        *pending = Some(p);
    }
}

/// A `CLOCK_MONOTONIC` timerfd, created but left unarmed: arming it is
/// [`arm_timerfd`]'s job, called once the RT thread's own setup (affinity,
/// `SCHED_FIFO`, `mlockall`, stack pre-fault) is done, so the timer's first period
/// starts only once the thread is actually ready to service it.
fn new_timerfd() -> Result<OwnedFd, RtError> {
    // Safety: `timerfd_create(2)` with valid constant arguments; the returned fd is
    // immediately wrapped in an `OwnedFd`, which closes it on drop.
    let raw = unsafe {
        libc::timerfd_create(
            libc::CLOCK_MONOTONIC,
            libc::TFD_NONBLOCK | libc::TFD_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(RtError::Io(std::io::Error::last_os_error()));
    }
    // Safety: `raw` was just returned by a successful `timerfd_create(2)` and is not
    // owned anywhere else.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Arms `fd` to fire every `period`, starting one `period` from now.
fn arm_timerfd(fd: RawFd, period: Duration) -> Result<(), RtError> {
    if period.is_zero() {
        // An all-zero `it_value` disarms the timer instead of firing: refuse it here
        // rather than block forever in the loop.
        return Err(RtError::Io(std::io::Error::from_raw_os_error(libc::EINVAL)));
    }
    let spec = libc::itimerspec {
        it_interval: to_timespec(period),
        it_value: to_timespec(period),
    };
    // Safety: `fd` is a valid timerfd and `spec` a fully-initialized `itimerspec`
    // live for the call; the old-value pointer is allowed to be null.
    let ret = unsafe { libc::timerfd_settime(fd, 0, &spec, ptr::null_mut()) };
    if ret < 0 {
        return Err(RtError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

fn to_timespec(d: Duration) -> libc::timespec {
    // Cast to the field's inferred type rather than naming `libc::time_t` (deprecated
    // on musl, which will widen it to 64-bit in a future libc release — `as _` tracks
    // that change on both glibc and musl instead of pinning the pre-change alias).
    libc::timespec {
        tv_sec: d.as_secs() as _,
        tv_nsec: d.subsec_nanos() as _,
    }
}

/// Number of expirations since the last read, or `Ok(0)` if the timer was not
/// ready yet (`EAGAIN`/`EWOULDBLOCK`: `poll` can wake the loop on a different fd
/// in the set while the timerfd itself has nothing new). Any other error means
/// the timerfd itself has gone bad and the caller must stop rather than spin.
fn read_timer(fd: RawFd) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    // Safety: `fd` is a valid non-blocking timerfd; `buf` is 8 writable bytes.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n == 8 {
        Ok(u64::from_ne_bytes(buf))
    } else if n < 0 {
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EAGAIN) => Ok(0), // EAGAIN == EWOULDBLOCK on Linux
            _ => Err(e),
        }
    } else {
        // A short read (0 < n < 8) cannot happen on a timerfd per the kernel
        // contract; treat it as "not ready yet" rather than panic on it.
        Ok(0)
    }
}

/// A non-blocking, close-on-exec `eventfd` used as a one-bit doorbell.
fn new_eventfd() -> Result<OwnedFd, RtError> {
    // Safety: `eventfd(2)` with valid constant arguments; the returned fd is
    // immediately wrapped in an `OwnedFd`, which closes it on drop.
    let raw = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    if raw < 0 {
        return Err(RtError::Io(std::io::Error::last_os_error()));
    }
    // Safety: `raw` was just returned by a successful `eventfd(2)` and is not owned
    // anywhere else.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Add 1 to an eventfd's counter, making it readable. Errors are not actionable:
/// the only failure mode on a non-blocking eventfd is a saturated counter, which
/// means the reader is already awake.
fn signal_eventfd(fd: RawFd) {
    let one: u64 = 1;
    // Safety: `fd` is a valid eventfd; `one` is 8 readable bytes live for the call.
    let _ = unsafe { libc::write(fd, &one as *const u64 as *const libc::c_void, 8) };
}

/// Reset an eventfd's counter to 0. `EAGAIN` (already 0) is the expected no-op.
fn drain_eventfd(fd: RawFd) {
    let mut buf = [0u8; 8];
    // Safety: `fd` is a valid non-blocking eventfd; `buf` is 8 writable bytes.
    let _ = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cm::{validate, ConnectReq, DeviceModel};
    use crate::eth::{MacAddr, MockTransport};
    use crate::rt::image::{Freshness, IoImage};
    use crate::rt::layout::Layout;
    use crate::testutil::{golden, golden_rt};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const CPU: MacAddr = MacAddr([0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f]);
    const DEV: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);

    fn layout() -> Layout {
        let model = DeviceModel::pnet_sample(DEV);
        let req = ConnectReq::parse(&golden("connect_req")[142..]).unwrap();
        Layout::from_ar(&validate(&req, &model).unwrap(), &model).unwrap()
    }

    /// Shrink the period for the test: 5 ms instead of 32 ms (cycle_step stays 1024
    /// for the counter in production; here it also drives the timer).
    fn cfg(image: Arc<IoImage>, stats: Arc<RtStats>) -> RtConfig {
        let mut layout = layout();
        layout.input_cr.cycle_step = 160; // 160 x 31.25 us = 5 ms
        layout.output_cr.cycle_step = 160;
        layout.output_cr.watchdog = Duration::from_millis(15);
        RtConfig {
            iface: String::new(),
            our_mac: DEV,
            cpu_mac: CPU,
            layout,
            image,
            stats,
            problem_indicator: Arc::new(AtomicBool::new(false)),
            cpu_pin: None,
            rt_priority: None,
            lock_memory: false,
        }
    }

    #[test]
    fn tick_grid_anchors_on_first_read_and_does_not_count_its_setup_expirations() {
        let period = Duration::from_millis(1);
        let mut grid = TickGrid::new(period);
        let t0 = Instant::now();

        // The first read anchors the grid: lateness is always 0, and only 1 tick
        // is counted no matter how many expirations the timerfd reports (those
        // extra ones are the RT thread's own setup time, not missed cycles).
        assert_eq!(grid.on_read(t0, 3), 0);
        assert_eq!(grid.ticks(), 1);

        // Exactly one period later, on time: still 0.
        assert_eq!(grid.on_read(t0 + period, 1), 0);
        assert_eq!(grid.ticks(), 2);

        // One period after that tick's own ideal instant, plus 250 us of jitter.
        let late = t0 + 2 * period + Duration::from_micros(250);
        assert_eq!(grid.on_read(late, 1), 250_000);
        assert_eq!(grid.ticks(), 3);

        // Two periods after *this* tick's ideal instant (t0 + 2*period), plus
        // 10 us, and reporting 2 expirations (one missed tick in between).
        let later = t0 + 4 * period + Duration::from_micros(10);
        assert_eq!(grid.on_read(later, 2), 10_000);
        assert_eq!(grid.ticks(), 5);

        // A wake earlier than the ideal grid (clock jitter): clamped to 0, never
        // a panic or an underflow.
        let early = t0 + 5 * period - Duration::from_millis(1);
        assert_eq!(grid.on_read(early, 1), 0);
        assert_eq!(grid.ticks(), 6);
    }

    #[test]
    fn runner_ticks_sends_and_consumes_with_a_mock_transport() {
        let image = Arc::new(IoImage::new(&layout()));
        let stats = Arc::new(RtStats::default());
        let mock = MockTransport::new();
        mock.push_rx(golden_rt("echo_cpu_8001"));
        let mock = Arc::new(mock);
        image.write_inputs(1, 1, &[0x5a]).unwrap();
        let h = RtRunner::spawn_with_transport(
            cfg(image.clone(), stats.clone()),
            SharedMock(mock.clone()),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(60));
        h.stop();
        h.join(Duration::from_secs(1)).unwrap();
        let sent = mock.sent();
        // Upper bound widened from 14 to 20: 60 ms sleep at a 5 ms period leaves
        // little slack, and a loaded CI/WSL host can stretch scheduling enough to
        // fit a couple of extra ticks in.
        assert!(sent.len() >= 8 && sent.len() <= 20, "sent {}", sent.len());
        assert_eq!(&sent[0][12..18], &[0x81, 0x00, 0xc0, 0x00, 0x88, 0x92]);
        assert_eq!(sent[0][20 + 3], 0x5a); // our DI byte from the image
                                           // the CPU frame was consumed and published
        let qb0 = image.read_outputs(2, 1, |b, _| b[0]).unwrap();
        assert_eq!(qb0, 0x01);
        assert_eq!(image.validity().freshness(), Freshness::Stale); // watchdog 15 ms expired
        assert!(stats.snapshot().tx >= 8);
        assert_eq!(stats.snapshot().rx_accepted, 1);
        assert_eq!(stats.snapshot().watchdog_expirations, 1);
        // The first timer read must never be reported as missed ticks: the timer
        // is armed only after thread setup, and the engine sees `1` expiration
        // for that first read regardless of what the timerfd itself reports.
        assert_eq!(stats.snapshot().missed_ticks, 0);
        assert_eq!(stats.cycle_work.count(), stats.snapshot().tx);
        assert_eq!(stats.tick_lateness.count(), stats.snapshot().tx);
        assert_eq!(stats.rx_interval.count(), 0); // one frame: no interval yet
        assert_eq!(h.take_event(), Some(RtEvent::WatchdogExpired));
        assert_eq!(h.take_event(), Some(RtEvent::Exited));
    }

    #[test]
    fn repeated_golden_frame_yields_one_rx_interval_sample() {
        // Two accepted frames with the *same* cycle counter: `RtEngine::on_frame`
        // only bumps the `reordered` stat on a repeat (d == 0), it does not drop
        // the frame — so both frames are still `Accepted`, and the second one is
        // what gives `rx_interval` its first sample.
        let image = Arc::new(IoImage::new(&layout()));
        let stats = Arc::new(RtStats::default());
        let mock = MockTransport::new();
        mock.push_rx(golden_rt("echo_cpu_8001"));
        mock.push_rx(golden_rt("echo_cpu_8001"));
        let mock = Arc::new(mock);
        let h =
            RtRunner::spawn_with_transport(cfg(image, stats.clone()), SharedMock(mock)).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        h.stop();
        h.join(Duration::from_secs(1)).unwrap();
        assert_eq!(stats.snapshot().rx_accepted, 2);
        assert_eq!(stats.rx_interval.count(), 1);
        assert!(stats.rx_interval.max_ns() > 0);
    }

    #[test]
    fn oversized_frame_counts_rx_invalid_and_the_thread_keeps_ticking() {
        // A single over-length frame: `MockTransport::recv_into` hands back
        // `TransportError::FrameTooLong` for it once (the frame is popped either
        // way), then the queue is empty and every later call returns `Ok(None)` —
        // exactly the "once, then Ok(None)" shape this test needs, no bespoke
        // transport required.
        let image = Arc::new(IoImage::new(&layout()));
        let stats = Arc::new(RtStats::default());
        let mock = MockTransport::new();
        mock.push_rx(vec![0u8; MAX_FRAME_LEN + 1]);
        let mock = Arc::new(mock);
        let h =
            RtRunner::spawn_with_transport(cfg(image, stats.clone()), SharedMock(mock)).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let tx_before = stats.snapshot().tx;
        std::thread::sleep(Duration::from_millis(30));
        let tx_after = stats.snapshot().tx;
        h.stop();
        h.join(Duration::from_secs(1)).unwrap();
        assert_eq!(stats.snapshot().rx_invalid, 1);
        assert!(tx_before > 0, "runner never ticked");
        assert!(
            tx_after > tx_before,
            "runner stopped ticking after the bad frame"
        );
        assert!(!h.is_running());
    }

    #[test]
    fn stop_is_prompt_and_join_times_out_cleanly() {
        let image = Arc::new(IoImage::new(&layout()));
        let h = RtRunner::spawn_with_transport(
            cfg(image, Arc::new(RtStats::default())),
            MockTransport::new(),
        )
        .unwrap();
        let t = Instant::now();
        h.stop();
        h.join(Duration::from_secs(1)).unwrap();
        assert!(t.elapsed() < Duration::from_millis(200));
        assert!(!h.is_running());
    }

    #[test]
    fn sched_warning_is_reported_not_fatal() {
        // No `geteuid() == 0` guard: it is not just root that can make
        // `SCHED_FIFO` succeed — an unprivileged user may also hold enough
        // `RLIMIT_RTPRIO` (via e.g. a `/etc/security/limits.d` grant or
        // `CAP_SYS_NICE`) for `set_fifo_priority` not to fail. So this accepts
        // either outcome: a warning was reported, or none was needed because the
        // thread was already up and running before it was asked to stop.
        let image = Arc::new(IoImage::new(&layout()));
        let mut c = cfg(image, Arc::new(RtStats::default()));
        c.rt_priority = Some(80); // may or may not exceed this environment's RLIMIT_RTPRIO
        let h = RtRunner::spawn_with_transport(c, MockTransport::new()).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let was_running = h.is_running();
        h.stop();
        h.join(Duration::from_secs(1)).unwrap();
        let first = h.take_event();
        let got_warning = matches!(first, Some(RtEvent::SchedWarning(_)));
        assert!(
            got_warning || was_running,
            "expected a SchedWarning, or at least a running thread before stop; \
             got {first:?} (was_running={was_running})"
        );
    }

    #[test]
    fn dropping_the_handle_stops_the_thread() {
        let image = Arc::new(IoImage::new(&layout()));
        let stats = Arc::new(RtStats::default());
        let h = RtRunner::spawn_with_transport(cfg(image, stats.clone()), MockTransport::new())
            .unwrap();
        std::thread::sleep(Duration::from_millis(40)); // let a few ticks happen
        drop(h); // no explicit stop()/join(): Drop must stop the thread on its own
        std::thread::sleep(Duration::from_millis(30));
        let tx_after_drop = stats.snapshot().tx;
        assert!(tx_after_drop > 0, "runner never ticked");
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            stats.snapshot().tx,
            tx_after_drop,
            "thread kept sending after the handle was dropped"
        );
    }

    #[test]
    fn drain_should_continue_pins_the_policy() {
        // With a raw fd, only the pre-check's readability matters: a skipped
        // frame does not stop the drain as long as the queue still reports
        // readable.
        assert!(drain_should_continue(true, true, false));
        assert!(drain_should_continue(true, true, true));
        // Without a fd to probe ahead of time, only whether `recv` actually
        // handed back a frame matters.
        assert!(drain_should_continue(false, true, true));
        assert!(!drain_should_continue(false, true, false));
    }

    /// `MockTransport` is not `Clone`; share it through an `Arc` for the test.
    struct SharedMock(Arc<MockTransport>);
    impl crate::eth::EthTransport for SharedMock {
        fn send(&self, f: &[u8]) -> Result<(), crate::eth::TransportError> {
            self.0.send(f)
        }
        fn recv_into(
            &self,
            buf: &mut [u8],
            timeout: Option<Duration>,
        ) -> Result<Option<usize>, crate::eth::TransportError> {
            self.0.recv_into(buf, timeout)
        }
    }
}

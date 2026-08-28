//! The acyclic loop: services the DCP (Ethernet) and RPC (UDP) sockets, drives
//! [`Cm`](crate::cm::Cm) with what they produce, and executes its outputs. Log-and-drop
//! for parse errors (spec §8); only transport I/O failures abort the loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::cm::model::DeviceModel;
use crate::cm::{AbortReason, ArParams, ArState, Cm, CmOutput};
use crate::dcp::{handle_dcp_frame, DeviceConfig as DcpDeviceConfig};
use crate::eth::poll::wait_any_readable;
use crate::eth::{EthTransport, TransportError};
use crate::rpc::{RpcError, RpcTransport, Uuid};
use crate::rt::{IoImage, RtStats};
#[cfg(target_os = "linux")]
use crate::rt::{Layout, RtConfig, RtError, RtEvent, RtHandle, RtRunner};

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
}

/// Static device identity + configuration handed to [`Device::new`]: the DCP identity
/// answered on the wire, the AR/slot model `Cm` establishes connections against, the
/// activity UUID used for our outgoing ApplicationReady calls, and the cyclic thread
/// configuration (if any) started once the AR reaches `Data`.
#[derive(Debug, Clone)]
pub struct DeviceSetup {
    pub dcp: DcpDeviceConfig,
    pub model: DeviceModel,
    pub activity_seed: Uuid,
    pub rt: Option<RtOptions>,
}

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
        let cm = Cm::new(setup.model.clone(), setup.activity_seed);
        Device {
            setup,
            eth,
            rpc,
            cm,
            on_state_change: None,
            image: Arc::new(IoImage::empty()),
            stats: Arc::new(RtStats::default()),
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
            self.step(Instant::now(), Some(Duration::from_millis(200)))?;
        }
        Ok(())
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
        let deadline_wait = self
            .cm
            .next_deadline()
            .map(|d| d.saturating_duration_since(now));
        let effective_wait = match (wait, deadline_wait) {
            (Some(w), Some(d)) => Some(w.min(d)),
            (Some(w), None) => Some(w),
            (None, Some(d)) => Some(d),
            (None, None) => None,
        };
        if let (Some(eth_fd), Some(rpc_fd)) = (self.eth.raw_fd(), self.rpc.raw_fd()) {
            let mut fds = vec![eth_fd, rpc_fd];
            #[cfg(target_os = "linux")]
            if let Some(runner) = &self.runner {
                fds.push(runner.event_fd());
            }
            wait_any_readable(&fds, effective_wait)
                .map_err(|e| DeviceError::Eth(TransportError::Io(e)))?;
        }
        // else: mock transports have no fds to poll on; proceed straight to draining
        // (their `recv` ignores the timeout and returns immediately; the RT events are
        // drained below regardless of whether we polled for them).

        let mut report = StepReport::default();

        while let Some(frame) = self.eth.recv(Some(Duration::ZERO))? {
            report.eth_frames += 1;
            match handle_dcp_frame(&frame, &self.setup.dcp) {
                Ok(Some(resp)) => self.eth.send(&resp)?,
                Ok(None) => {}
                Err(e) => log::warn!("dropping unparsable DCP frame: {e}"),
            }
        }

        while let Some((buf, from)) = self.rpc.recv(Some(Duration::ZERO))? {
            report.rpc_datagrams += 1;
            match self.cm.handle_datagram(&buf, from, now) {
                Ok(out) => self.dispatch(out, &mut report)?,
                Err(e) => log::warn!("dropping unparsable RPC datagram: {e}"),
            }
        }

        #[cfg(target_os = "linux")]
        self.drain_rt_events(now, &mut report)?;

        let out = self.cm.tick(now);
        self.dispatch(out, &mut report)?;

        Ok(report)
    }

    /// Sends every PDU, then reports every AR notification to the state-change
    /// callback and, in turn, starts or stops the RT runner: `Data` (from a fresh
    /// negotiation, not a resend) starts it, an abort back to `Idle` stops it.
    fn dispatch(&mut self, out: CmOutput, report: &mut StepReport) -> Result<(), DeviceError> {
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
                }
                (ArState::Idle, Some(_)) => {
                    #[cfg(target_os = "linux")]
                    self.stop_runner();
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
            cpu_pin: rt.cpu_pin,
            rt_priority: rt.rt_priority,
        };
        match (self.runner_factory)(cfg) {
            Ok(handle) => self.runner = Some(handle),
            Err(e) => log::error!("RT runner spawn failed: {e}"),
        }
    }

    /// Stops and joins the RT thread, if any, bounded so a stuck thread cannot hang
    /// the acyclic loop forever.
    fn stop_runner(&mut self) {
        if let Some(runner) = self.runner.take() {
            runner.stop();
            if let Err(e) = runner.join(Duration::from_millis(500)) {
                log::warn!("RT runner join timed out: {e}");
            }
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
                    let out = self.cm.abort(AbortReason::RtWatchdog, now);
                    self.dispatch(out, report)?;
                }
                RtEvent::SocketError(s) => {
                    log::error!("RT socket error, aborting the AR: {s}");
                    let out = self.cm.abort(AbortReason::RtWatchdog, now);
                    self.dispatch(out, report)?;
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
    use crate::cm::model::DeviceModel;
    use crate::dcp::{DeviceConfig, DeviceProperties};
    use crate::eth::{MacAddr, MockTransport};
    use crate::rpc::{MockRpcTransport, Uuid};
    #[cfg(target_os = "linux")]
    use crate::rt::RtRunner;
    #[cfg(target_os = "linux")]
    use crate::testutil::golden_rt;
    use crate::testutil::{golden, RPC_OFF};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const MAC: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);

    fn setup() -> DeviceSetup {
        DeviceSetup {
            dcp: DeviceConfig {
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
        }
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
    }
}

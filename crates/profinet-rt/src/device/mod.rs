//! The acyclic loop: services the DCP (Ethernet) and RPC (UDP) sockets, drives
//! [`Cm`](crate::cm::Cm) with what they produce, and executes its outputs. Log-and-drop
//! for parse errors (spec §8); only transport I/O failures abort the loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::cm::model::DeviceModel;
use crate::cm::{AbortReason, ArState, Cm, CmOutput};
use crate::dcp::{handle_dcp_frame, DeviceConfig as DcpDeviceConfig};
use crate::eth::poll::wait_any_readable;
use crate::eth::{EthTransport, TransportError};
use crate::rpc::{RpcError, RpcTransport, Uuid};

/// Callback invoked once per AR state-change notification.
type StateChangeCallback = Box<dyn FnMut(ArState, Option<AbortReason>) + Send>;

/// Static device identity + configuration handed to [`Device::new`]: the DCP identity
/// answered on the wire, the AR/slot model `Cm` establishes connections against, and
/// the activity UUID used for our outgoing ApplicationReady calls.
#[derive(Debug, Clone)]
pub struct DeviceSetup {
    pub dcp: DcpDeviceConfig,
    pub model: DeviceModel,
    pub activity_seed: Uuid,
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
            wait_any_readable(&[eth_fd, rpc_fd], effective_wait)
                .map_err(|e| DeviceError::Eth(TransportError::Io(e)))?;
        }
        // else: mock transports have no fds to poll on; proceed straight to draining
        // (their `recv` ignores the timeout and returns immediately).

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

        let out = self.cm.tick(now);
        self.dispatch(out, &mut report)?;

        Ok(report)
    }

    fn dispatch(&mut self, out: CmOutput, report: &mut StepReport) -> Result<(), DeviceError> {
        for o in out.send {
            self.rpc.send(&o.bytes, o.to)?;
            report.sent += 1;
        }
        for (state, reason) in out.notify {
            if let Some(cb) = &mut self.on_state_change {
                cb(state, reason);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cm::model::DeviceModel;
    use crate::dcp::{DeviceConfig, DeviceProperties};
    use crate::eth::{MacAddr, MockTransport};
    use crate::rpc::{MockRpcTransport, Uuid};
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
    fn run_stops_on_flag() {
        let stop = std::sync::atomic::AtomicBool::new(true);
        let mut dev = Device::new(setup(), MockTransport::new(), MockRpcTransport::new());
        dev.run(&stop).unwrap();
    }
}

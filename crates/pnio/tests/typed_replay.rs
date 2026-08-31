#![cfg(target_os = "linux")] // drives `pnio::api`, which needs the Linux transports
//! End-to-end with the typed config: synthetic Connect → Data, a fabricated CPU frame for
//! the 16 REAL + 32 BOOL layout decoded through IoDevice, our inputs published and
//! visible in the produced frame.
mod common;
use common::{golden, synthetic_connect_req, RPC_OFF, RT_CSDU_OFF, RT_FRAMEID_OFF};
use pnio::api::IoDevice;
use pnio::config::{DeviceConfig, Slot};
use pnio::data::FieldType::*;
use pnio::device::RtOptions;
use pnio::eth::{EthTransport, MacAddr, TransportError};
use pnio::rpc::MockRpcTransport;
use pnio::rt::{DataStatus, Layout, RtFrame, RtRunner};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DEV: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);
const CPU: MacAddr = MacAddr([0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f]);

/// A test-only "wire" shared between the acyclic loop's `eth` and the RT thread's,
/// FrameID-partitioned like the real `bpf::acyclic_filter`/`bpf::rt_filter` BPF
/// programs that split a real NIC's two sockets by FrameID so each wakes only for its
/// own traffic. Copied from `crates/pnio/src/api.rs`'s private `#[cfg(test)] mod
/// tests` (`SharedMock`) because an integration test cannot see that module.
///
/// Without this partitioning a single shared `MockTransport` would race: the acyclic
/// loop has no `raw_fd` to poll on with mocks, so `Device::step` drains `eth.recv` in
/// a tight, unthrottled loop and would win essentially every time, silently dropping
/// every RTC1 frame as unparsable DCP before the RT thread (which only drains once
/// per ~1ms tick) ever saw it.
///
/// Unlike the original, this copy also records every frame passed to `send` — needed
/// here (unlike in `api.rs`'s tests) to inspect the Input CR frames the device
/// produces and check our published inputs landed at the right layout offsets.
#[derive(Clone)]
struct SharedMock {
    frames: Arc<Mutex<VecDeque<Vec<u8>>>>,
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
    range: (u16, u16),
}
impl SharedMock {
    /// One shared queue (plus one shared TX log), two role-filtered handles:
    /// `(acyclic, rt)`.
    fn new_pair() -> (SharedMock, SharedMock) {
        let frames = Arc::new(Mutex::new(VecDeque::new()));
        let sent = Arc::new(Mutex::new(Vec::new()));
        (
            SharedMock {
                frames: frames.clone(),
                sent: sent.clone(),
                range: (0xFC00, 0xFFFF),
            },
            SharedMock {
                frames,
                sent,
                range: (0x8000, 0xBFFF),
            },
        )
    }
    fn push_rx(&self, frame: Vec<u8>) {
        self.frames
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(frame);
    }
    /// All frames sent via `send`, across both role handles, in order.
    fn sent(&self) -> Vec<Vec<u8>> {
        self.sent.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}
/// The FrameID of a VLAN-tagged or untagged PROFINET frame, or `None` if it isn't
/// one — same header shapes `RtFrame::parse`/the acyclic BPF filter handle.
fn frame_id(frame: &[u8]) -> Option<u16> {
    if frame.len() >= 20 && frame[12..14] == [0x81, 0x00] {
        Some(u16::from_be_bytes([frame[18], frame[19]]))
    } else if frame.len() >= 16 && frame[12..14] == [0x88, 0x92] {
        Some(u16::from_be_bytes([frame[14], frame[15]]))
    } else {
        None
    }
}
impl EthTransport for SharedMock {
    fn send(&self, f: &[u8]) -> Result<(), TransportError> {
        self.sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(f.to_vec());
        Ok(())
    }
    fn recv_into(
        &self,
        buf: &mut [u8],
        _t: Option<Duration>,
    ) -> Result<Option<usize>, TransportError> {
        let mut q = self.frames.lock().unwrap_or_else(|e| e.into_inner());
        let (lo, hi) = self.range;
        let pos = q
            .iter()
            .position(|f| matches!(frame_id(f), Some(id) if id >= lo && id <= hi));
        match pos.and_then(|i| q.remove(i)) {
            Some(frame) => {
                buf[..frame.len()].copy_from_slice(&frame);
                Ok(Some(frame.len()))
            }
            None => Ok(None),
        }
    }
}

fn sample() -> DeviceConfig {
    DeviceConfig::builder("pnio-dev")
        .input(Slot(1), &[Real; 16])
        .input(Slot(2), &[Bool; 32])
        .output(Slot(3), &[Real; 16])
        .output(Slot(4), &[Bool; 32])
        .build()
        .unwrap()
}

/// Waits for `dev.ready()` — AR at `Data` *and* the image actually laid out.
/// `ar_state() == Data` alone is not enough: for a few microseconds the acyclic
/// thread can report `Data` before `Device::start_runner` has rebuilt the image.
fn wait_until_ready(dev: &IoDevice) {
    let t0 = std::time::Instant::now();
    while !dev.ready() {
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "AR stuck in {:?}",
            dev.ar_state()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn typed_round_trip_with_the_sample_config() {
    let cfg = sample();
    let model = cfg.model(DEV);
    let rpc = MockRpcTransport::new();
    let cpu = "172.16.2.100:54766".parse().unwrap();
    let cpu_cm = "172.16.2.100:34964".parse().unwrap();
    rpc.push_rx(synthetic_connect_req(&model), cpu);
    rpc.push_rx(golden("write_req")[RPC_OFF..].to_vec(), cpu);
    rpc.push_rx(golden("prmend_req")[RPC_OFF..].to_vec(), cpu);
    rpc.push_rx(golden("appready_res")[RPC_OFF..].to_vec(), cpu_cm);

    let (eth_acyclic, eth_rt) = SharedMock::new_pair();
    let rt = Some(RtOptions {
        iface: String::new(),
        cpu_pin: None,
        rt_priority: None,
        lock_memory: false,
    });
    let dev = IoDevice::start_with(
        cfg.clone(),
        DEV,
        [172, 16, 2, 10],
        rt,
        eth_acyclic.clone(),
        rpc,
        move |c| RtRunner::spawn_with_transport(c, eth_rt.clone()),
    )
    .unwrap();
    wait_until_ready(&dev);

    // Our inputs: REAL 1.0 at slot 1 index 0, -2.5 at index 15, bits 0 and 31 of slot 2.
    dev.with_inputs(Slot(1), |w| {
        w.real(0, 1.0)?;
        w.real(15, -2.5)
    })
    .unwrap();
    dev.with_inputs(Slot(2), |w| {
        w.bool(0, true)?;
        w.bool(31, true)
    })
    .unwrap();

    // A CPU frame: REAL 1.0 at slot 3 index 0, -2.5 at index 15, bit 7 of slot 4 byte 3
    // (index 31).
    let params = dev.ar_params().unwrap();
    let layout = Layout::from_ar(&params, &model).unwrap();
    let mut csdu = vec![0u8; layout.output_cr.data_length];
    for o in &layout.output_cr.objects {
        csdu[o.iops_off] = 0x80;
    }
    for c in &layout.output_cr.iocs {
        csdu[c.iocs_off] = 0x80;
    }
    let s3 = layout
        .output_cr
        .objects
        .iter()
        .find(|o| o.slot == 3)
        .unwrap()
        .data_off;
    csdu[s3..s3 + 4].copy_from_slice(&[0x3F, 0x80, 0, 0]);
    csdu[s3 + 60..s3 + 64].copy_from_slice(&[0xC0, 0x20, 0, 0]);
    let s4 = layout
        .output_cr
        .objects
        .iter()
        .find(|o| o.slot == 4)
        .unwrap()
        .data_off;
    csdu[s4 + 3] = 0x80;
    let mut buf = vec![0u8; 1522];
    let n = RtFrame {
        frame_id: 0x8001,
        cycle_counter: 1024,
        data_status: DataStatus(0x35),
        transfer_status: 0,
        csdu: &csdu,
    }
    .write(&mut buf, DEV, CPU)
    .unwrap();
    let frame = buf[..n].to_vec();

    // A real controller resends every cycle; keep feeding it (~1ms, the CR's own
    // period) instead of a single push + fixed sleep, so the RT thread's consumer
    // watchdog can't expire between the push and the assertions below.
    let stop_feed = Arc::new(AtomicBool::new(false));
    let feeder = {
        let stop_feed = stop_feed.clone();
        let eth_acyclic = eth_acyclic.clone();
        let frame = frame.clone();
        std::thread::spawn(move || {
            while !stop_feed.load(Ordering::Relaxed) {
                eth_acyclic.push_rx(frame.clone());
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    let t0 = std::time::Instant::now();
    loop {
        if dev.read_real(Slot(3), 0) == Ok(1.0) {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "CPU frame never landed"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    stop_feed.store(true, Ordering::Relaxed);
    feeder.join().unwrap();

    let snap = dev.outputs(Slot(3)).unwrap();
    assert_eq!(snap.real(0).unwrap(), 1.0);
    assert_eq!(snap.real(15).unwrap(), -2.5);
    assert!(dev.read_bool(Slot(4), 31).unwrap());
    assert!(!dev.read_bool(Slot(4), 30).unwrap());

    // The frames we produced carry our inputs at the Input CR offsets. Our TX is
    // always VLAN-tagged (`RtFrame::write`): FrameID at RT_FRAMEID_OFF (18..20),
    // C-SDU at RT_CSDU_OFF (20).
    let sent = eth_acyclic.sent();
    let sent_frame = sent
        .iter()
        .rev()
        .find(|f| f.len() > RT_CSDU_OFF && f[RT_FRAMEID_OFF..RT_CSDU_OFF] == [0x80, 0x00])
        .expect("device sent an Input CR (0x8000) frame");
    let s1 = layout
        .input_cr
        .objects
        .iter()
        .find(|o| o.slot == 1)
        .unwrap()
        .data_off;
    assert_eq!(
        &sent_frame[RT_CSDU_OFF + s1..RT_CSDU_OFF + s1 + 4],
        &[0x3F, 0x80, 0, 0]
    );
    let s2 = layout
        .input_cr
        .objects
        .iter()
        .find(|o| o.slot == 2)
        .unwrap()
        .data_off;
    assert_eq!(sent_frame[RT_CSDU_OFF + s2], 0x01);
    assert_eq!(sent_frame[RT_CSDU_OFF + s2 + 3], 0x80);

    dev.stop().unwrap();
}

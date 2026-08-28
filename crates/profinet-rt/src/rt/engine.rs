//! PPM/CPM engine: the cyclic heart of RTC1 exchange.
//!
//! Pure — no sockets, no clock of its own (the caller supplies `Instant` and tick
//! `expirations`). [`RtEngine::on_tick`] produces our provider frame each cycle;
//! [`RtEngine::on_frame`] consumes the controller's frame; [`RtEngine::check_watchdog`]
//! tracks the consumer watchdog. Stats are atomics shared with the rest of the stack.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use super::frame::{frame_len, DataStatus, RtFrame};
use super::hist::Histogram;
use super::layout::Layout;
use crate::eth::MacAddr;

/// IOxS byte meaning "good" (`DataState_Valid` / `DataState_Ok`, low nibble `0x0`, bit7 set).
pub const IOXS_GOOD: u8 = 0x80;
/// IOxS byte meaning "bad".
pub const IOXS_BAD: u8 = 0x00;

/// Why a received frame was dropped outright (parsed, but unusable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// Non-zero transfer status: the provider marks this frame not to be used.
    TransferStatus(u8),
    /// C-SDU shorter than the output CR's `data_length`.
    ShortCsdu { have: usize, need: usize },
    /// Parse failure (too short, wrong ethertype, ...).
    Malformed,
}

/// Outcome of [`RtEngine::on_frame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxVerdict {
    /// Accepted: matched our output CR's frame ID and source, transfer status OK,
    /// C-SDU long enough. `data_valid` false means the C-SDU was *not* copied
    /// (stale data kept).
    Accepted {
        provider_run: bool,
        primary: bool,
        data_valid: bool,
    },
    /// Not for us: wrong source MAC or wrong frame ID (e.g. our own frame echoed back).
    Ignored,
    /// Parsed and addressed to us, but unusable.
    Dropped(DropReason),
}

/// Outcome of [`RtEngine::check_watchdog`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogVerdict {
    /// No frame has been accepted yet; the watchdog has nothing to measure against.
    NotArmed,
    /// Within the watchdog window since the last accepted frame.
    Ok,
    /// The watchdog window has just been exceeded (reported once, on the transition).
    Expired,
    /// The watchdog stays exceeded (reported on every check after the first `Expired`,
    /// until a new frame is accepted).
    Stopped,
}

/// Cyclic exchange counters, all relaxed atomics so they can be read from another
/// thread (e.g. a diagnostics endpoint) without locking.
#[derive(Debug, Default)]
pub struct RtStats {
    pub tx: AtomicU64,
    pub rx_accepted: AtomicU64,
    pub rx_ignored: AtomicU64,
    pub rx_dropped: AtomicU64,
    pub rx_invalid: AtomicU64,
    pub reordered: AtomicU64,
    pub watchdog_expirations: AtomicU64,
    pub missed_ticks: AtomicU64,
    pub input_snapshot_reused: AtomicU64,
    pub output_publish_deferred: AtomicU64,
    /// Timer wake-up minus scheduled expiry, per tick.
    pub tick_lateness: Histogram,
    /// Tick wake-up to `send` returned, per tick: our own cost.
    pub cycle_work: Histogram,
    /// Interval between two consecutive accepted controller frames.
    pub rx_interval: Histogram,
}

/// Plain-value snapshot of [`RtStats`], taken with a single relaxed load per field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatsSnapshot {
    pub tx: u64,
    pub rx_accepted: u64,
    pub rx_ignored: u64,
    pub rx_dropped: u64,
    pub rx_invalid: u64,
    pub reordered: u64,
    pub watchdog_expirations: u64,
    pub missed_ticks: u64,
    pub input_snapshot_reused: u64,
    pub output_publish_deferred: u64,
    pub max_tick_lateness_ns: u64,
    pub max_cycle_work_ns: u64,
    pub max_rx_interval_ns: u64,
}

impl RtStats {
    /// Take a plain-value snapshot (one relaxed load per counter).
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            tx: self.tx.load(Ordering::Relaxed),
            rx_accepted: self.rx_accepted.load(Ordering::Relaxed),
            rx_ignored: self.rx_ignored.load(Ordering::Relaxed),
            rx_dropped: self.rx_dropped.load(Ordering::Relaxed),
            rx_invalid: self.rx_invalid.load(Ordering::Relaxed),
            reordered: self.reordered.load(Ordering::Relaxed),
            watchdog_expirations: self.watchdog_expirations.load(Ordering::Relaxed),
            missed_ticks: self.missed_ticks.load(Ordering::Relaxed),
            input_snapshot_reused: self.input_snapshot_reused.load(Ordering::Relaxed),
            output_publish_deferred: self.output_publish_deferred.load(Ordering::Relaxed),
            max_tick_lateness_ns: self.tick_lateness.max_ns(),
            max_cycle_work_ns: self.cycle_work.max_ns(),
            max_rx_interval_ns: self.rx_interval.max_ns(),
        }
    }
}

/// Whether the watchdog has ever seen an accepted frame, and if so whether it has
/// already reported `Expired` for the current gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchdogState {
    NotArmed,
    Ok,
    Expired,
}

/// The pure PPM/CPM engine for one AR: produces our cyclic frame, consumes the
/// controller's, tracks IOPS/IOCS, the cycle counter, and the consumer watchdog.
///
/// No I/O and no clock of its own — the caller drives it with `Instant`s and tick
/// counts, and owns the socket that sends `on_tick`'s returned slice / feeds
/// `on_frame` the received bytes.
pub struct RtEngine {
    layout: Layout,
    our_mac: MacAddr,
    cpu_mac: MacAddr,
    stats: Arc<RtStats>,

    /// Preallocated TX frame buffer, sized for `frame_len(input_cr.data_length)`.
    tx: Vec<u8>,
    /// Preallocated scratch C-SDU for the frame being built (`input_cr.data_length`
    /// bytes), assembled here each tick and then handed to [`RtFrame::write`] — kept
    /// separate from `tx` so the two can be borrowed independently.
    tx_csdu: Vec<u8>,
    /// Preallocated last-accepted C-SDU from the output CR (`output_cr.data_length` bytes).
    rx_csdu: Vec<u8>,
    /// Last IOPS-good bit per `output_cr.objects[i]`, refreshed on every accepted+valid frame.
    rx_iops_good: Vec<bool>,
    /// Last IOCS-good bit per `output_cr.iocs[j]` — the controller's echoed consumer
    /// status for each of our input submodules — refreshed on every accepted+valid frame.
    rx_iocs_good: Vec<bool>,

    cycle_counter: u16,
    last_rx: Option<Instant>,
    last_rx_cycle_counter: Option<u16>,
    watchdog_state: WatchdogState,
    provider_run: bool,
    primary: bool,
}

impl RtEngine {
    /// Build the engine for `layout`, ready to run. `our_mac` is the source address
    /// we stamp on frames we produce; `cpu_mac` is the only source `on_frame` accepts.
    pub fn new(layout: Layout, our_mac: MacAddr, cpu_mac: MacAddr, stats: Arc<RtStats>) -> Self {
        let tx = vec![0u8; frame_len(layout.input_cr.data_length)];
        let tx_csdu = vec![0u8; layout.input_cr.data_length];
        let rx_csdu = vec![0u8; layout.output_cr.data_length];
        let rx_iops_good = vec![false; layout.output_cr.objects.len()];
        // One entry per `output_cr.iocs` object: the controller echoes back IOCS for
        // each of *our* input submodules, so this collection — despite living in the
        // output CR on the wire — is "per input object" in meaning.
        let rx_iocs_good = vec![false; layout.output_cr.iocs.len()];

        RtEngine {
            layout,
            our_mac,
            cpu_mac,
            stats,
            tx,
            tx_csdu,
            rx_csdu,
            rx_iops_good,
            rx_iocs_good,
            cycle_counter: 0,
            last_rx: None,
            last_rx_cycle_counter: None,
            watchdog_state: WatchdogState::NotArmed,
            provider_run: false,
            primary: false,
        }
    }

    /// Produce our provider frame for this tick. `inputs` is the full input-CR C-SDU
    /// image (`input_cr.data_length` bytes); the engine copies each object's slice into
    /// the TX C-SDU and stamps IOPS GOOD everywhere. IOCS = our consumer status for the
    /// controller's outputs: GOOD for every plugged submodule; independent of the
    /// received IOPS (the S7-1500 sends IOPS BAD until the device acknowledges
    /// consumption — mirroring would deadlock). Advances the cycle counter by
    /// `cycle_step * expirations` before writing it. Allocation-free: writes into the
    /// preallocated `self.tx` and returns a slice of it.
    pub fn on_tick(&mut self, expirations: u32, inputs: &[u8]) -> &[u8] {
        let cr = &self.layout.input_cr;

        for obj in &cr.objects {
            self.tx_csdu[obj.data_off..obj.data_off + obj.data_len]
                .copy_from_slice(&inputs[obj.data_off..obj.data_off + obj.data_len]);
            self.tx_csdu[obj.iops_off] = IOXS_GOOD;
        }
        for cs in &cr.iocs {
            self.tx_csdu[cs.iocs_off] = IOXS_GOOD;
        }

        self.cycle_counter = self
            .cycle_counter
            .wrapping_add(cr.cycle_step.wrapping_mul(expirations as u16));

        let rt = RtFrame {
            frame_id: cr.frame_id,
            csdu: &self.tx_csdu,
            cycle_counter: self.cycle_counter,
            data_status: DataStatus::RUN_PRIMARY_VALID_OK,
            transfer_status: 0,
        };
        let n = rt
            .write(&mut self.tx, self.cpu_mac, self.our_mac)
            .expect("tx buffer preallocated to frame_len");

        self.stats.tx.fetch_add(1, Ordering::Relaxed);
        self.stats
            .missed_ticks
            .fetch_add(expirations.saturating_sub(1) as u64, Ordering::Relaxed);

        &self.tx[..n]
    }

    /// Consume a received frame. See [`RxVerdict`] for the outcome states.
    pub fn on_frame(&mut self, frame: &[u8], now: Instant) -> RxVerdict {
        let (eth, rt) = match RtFrame::parse(frame) {
            Ok(v) => v,
            Err(_) => {
                self.stats.rx_dropped.fetch_add(1, Ordering::Relaxed);
                return RxVerdict::Dropped(DropReason::Malformed);
            }
        };

        let cr = &self.layout.output_cr;
        if eth.src != self.cpu_mac || rt.frame_id != cr.frame_id {
            self.stats.rx_ignored.fetch_add(1, Ordering::Relaxed);
            return RxVerdict::Ignored;
        }

        if rt.transfer_status != 0 {
            self.stats.rx_dropped.fetch_add(1, Ordering::Relaxed);
            return RxVerdict::Dropped(DropReason::TransferStatus(rt.transfer_status));
        }

        if rt.csdu.len() < cr.data_length {
            self.stats.rx_dropped.fetch_add(1, Ordering::Relaxed);
            return RxVerdict::Dropped(DropReason::ShortCsdu {
                have: rt.csdu.len(),
                need: cr.data_length,
            });
        }

        self.last_rx = Some(now);
        self.watchdog_state = WatchdogState::Ok;

        if let Some(prev) = self.last_rx_cycle_counter {
            let d = rt.cycle_counter.wrapping_sub(prev);
            if d == 0 || d > 0x8000 {
                self.stats.reordered.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.last_rx_cycle_counter = Some(rt.cycle_counter);

        let provider_run = rt.data_status.provider_run();
        let primary = rt.data_status.primary();
        let data_valid = rt.data_status.data_valid();
        self.provider_run = provider_run;
        self.primary = primary;

        if data_valid {
            self.rx_csdu[..cr.data_length].copy_from_slice(&rt.csdu[..cr.data_length]);
            for (i, obj) in cr.objects.iter().enumerate() {
                self.rx_iops_good[i] = self.rx_csdu[obj.iops_off] & 0x80 != 0;
            }
            for (j, cs) in cr.iocs.iter().enumerate() {
                self.rx_iocs_good[j] = self.rx_csdu[cs.iocs_off] & 0x80 != 0;
            }
            self.stats.rx_accepted.fetch_add(1, Ordering::Relaxed);
        } else {
            self.stats.rx_invalid.fetch_add(1, Ordering::Relaxed);
        }

        RxVerdict::Accepted {
            provider_run,
            primary,
            data_valid,
        }
    }

    /// Check the consumer watchdog against `now`. See [`WatchdogVerdict`].
    pub fn check_watchdog(&mut self, now: Instant) -> WatchdogVerdict {
        let last_rx = match self.last_rx {
            None => return WatchdogVerdict::NotArmed,
            Some(t) => t,
        };

        let elapsed = now.saturating_duration_since(last_rx);
        if elapsed <= self.layout.output_cr.watchdog {
            self.watchdog_state = WatchdogState::Ok;
            return WatchdogVerdict::Ok;
        }

        match self.watchdog_state {
            WatchdogState::Expired => WatchdogVerdict::Stopped,
            _ => {
                self.watchdog_state = WatchdogState::Expired;
                self.stats
                    .watchdog_expirations
                    .fetch_add(1, Ordering::Relaxed);
                WatchdogVerdict::Expired
            }
        }
    }

    /// Force every remembered output IOPS to BAD.
    ///
    /// Called by the runner when the consumer watchdog expires: the controller has
    /// stopped providing, so the application's view of output validity
    /// ([`RtEngine::rx_iops_good`]) must go BAD until it talks again — an accepted
    /// frame refreshes them. Does **not** affect the IOCS bytes we send: those are our
    /// own consumer status, always GOOD for a plugged submodule (see `on_tick`),
    /// independent of this.
    pub fn mark_outputs_stale(&mut self) {
        for good in &mut self.rx_iops_good {
            *good = false;
        }
    }

    /// Convenience: `self.stats.snapshot()`.
    pub fn stats_snapshot(&self) -> StatsSnapshot {
        self.stats.snapshot()
    }

    /// Last accepted+valid C-SDU from the output CR (`output_cr.data_length` bytes).
    pub fn rx_csdu(&self) -> &[u8] {
        &self.rx_csdu
    }

    /// Last IOPS-good bit per `output_cr.objects[i]`.
    pub fn rx_iops_good(&self) -> &[bool] {
        &self.rx_iops_good
    }

    /// Last IOCS-good bit per `output_cr.iocs[j]` (the controller's echoed consumer
    /// status for each of our input submodules).
    pub fn rx_iocs_good(&self) -> &[bool] {
        &self.rx_iocs_good
    }

    /// `Provider_State.Run` from the last accepted frame's data status.
    pub fn provider_run(&self) -> bool {
        self.provider_run
    }

    /// `State.Primary` from the last accepted frame's data status.
    pub fn primary(&self) -> bool {
        self.primary
    }

    /// Timestamp of the last accepted frame, if any.
    pub fn last_rx(&self) -> Option<Instant> {
        self.last_rx
    }

    /// Our current cycle counter (the value stamped on the frame we last produced).
    pub fn cycle_counter(&self) -> u16 {
        self.cycle_counter
    }

    /// The layout this engine was built from.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cm::{validate, ConnectReq, DeviceModel};
    use crate::eth::MacAddr;
    use crate::rt::frame::RtFrame;
    use crate::rt::layout::Layout;
    use crate::testutil::{golden, golden_rt, RT_CSDU_OFF};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const CPU: MacAddr = MacAddr([0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f]);
    const DEV: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);

    fn engine() -> RtEngine {
        let model = DeviceModel::pnet_sample(DEV);
        let req = ConnectReq::parse(&golden("connect_req")[142..]).unwrap();
        let params = validate(&req, &model).unwrap();
        RtEngine::new(
            Layout::from_ar(&params, &model).unwrap(),
            DEV,
            CPU,
            Arc::new(RtStats::default()),
        )
    }

    #[test]
    fn stats_snapshot_carries_the_histogram_maxima() {
        let s = RtStats::default();
        s.tick_lateness.record(1_500);
        s.cycle_work.record(20_000);
        s.rx_interval.record(1_010_000);
        let snap = s.snapshot();
        assert_eq!(snap.max_tick_lateness_ns, 1_500);
        assert_eq!(snap.max_cycle_work_ns, 20_000);
        assert_eq!(snap.max_rx_interval_ns, 1_010_000);
    }

    #[test]
    fn produced_frame_matches_pnet_except_counter_and_status() {
        let mut e = engine();
        // inputs image: DI = 0x2c at [3], DIO = 0x2d at [6], echo zeros — as p-net sent in rtc_dev_8000
        let mut inputs = vec![0u8; 40];
        inputs[3] = 0x2c;
        inputs[6] = 0x2d;
        let out = e.on_tick(1, &inputs).to_vec();
        let g = golden_rt("rtc_dev_8000");
        assert_eq!(&out[..60], &g[..60]); // header + C-SDU identical (IOPS/IOCS all GOOD)
        assert_eq!(&out[60..62], &1024u16.to_be_bytes()); // our first counter = one step
        assert_eq!(out[62], 0x35); // we emit Run|Primary|Valid|Ok, p-net 0x36
        assert_eq!(out[63], 0);
        assert_eq!(e.stats_snapshot().tx, 1);
    }

    #[test]
    fn iocs_is_always_good_for_plugged_outputs() {
        let mut e = engine();
        let inputs = vec![0u8; 40];

        // no CPU frame yet -> IOCS GOOD for the three output objects at [5], [8], [18]
        // (it's our own consumer status, not a mirror of the controller's IOPS)
        let out = e.on_tick(1, &inputs).to_vec();
        assert_eq!(
            (
                out[RT_CSDU_OFF + 5],
                out[RT_CSDU_OFF + 8],
                out[RT_CSDU_OFF + 18]
            ),
            (IOXS_GOOD, IOXS_GOOD, IOXS_GOOD)
        );
        // IOPS of our own objects always GOOD: [0],[1],[2],[4],[7],[17]
        for off in [0, 1, 2, 4, 7, 17] {
            assert_eq!(out[RT_CSDU_OFF + off], IOXS_GOOD, "iops at {off}");
        }

        // CPU frame with IOPS BAD (0x60, "detected by controller") on every output ->
        // still IOCS GOOD in our produced frame, only the application-facing
        // rx_iops_good() view goes bad.
        let mut cpu_frame = golden_rt("rtc_cpu_8001");
        for off in [5, 8, 18] {
            cpu_frame[RT_CSDU_OFF + off] = 0x60;
        }
        e.on_frame(&cpu_frame, Instant::now());
        assert!(e.rx_iops_good().iter().all(|g| !*g));

        let out = e.on_tick(1, &inputs).to_vec();
        assert_eq!(
            (
                out[RT_CSDU_OFF + 5],
                out[RT_CSDU_OFF + 8],
                out[RT_CSDU_OFF + 18]
            ),
            (IOXS_GOOD, IOXS_GOOD, IOXS_GOOD)
        );
    }

    #[test]
    fn cycle_counter_steps_and_missed_ticks() {
        let mut e = engine();
        let inputs = vec![0u8; 40];
        e.on_tick(1, &inputs);
        assert_eq!(e.cycle_counter(), 1024);
        e.on_tick(3, &inputs);
        assert_eq!(e.cycle_counter(), 4096);
        assert_eq!(e.stats_snapshot().missed_ticks, 2);
        for _ in 0..70 {
            e.on_tick(1, &inputs);
        }
        assert_eq!(e.cycle_counter(), (4096u32 + 70 * 1024) as u16); // wraps
    }

    #[test]
    fn consumes_cpu_frame_with_echo_data() {
        let mut e = engine();
        let v = e.on_frame(&golden_rt("echo_cpu_8001"), Instant::now());
        assert_eq!(
            v,
            RxVerdict::Accepted {
                provider_run: true,
                primary: true,
                data_valid: true
            }
        );
        let c = e.rx_csdu();
        assert_eq!(c[4], 0x01); // QB0
        assert_eq!(
            &c[10..18],
            &[0x12, 0x34, 0x56, 0x78, 0x3f, 0xc0, 0x00, 0x00]
        );
        assert!(e.rx_iops_good().iter().all(|g| *g));
        assert!(e.rx_iocs_good().iter().all(|g| *g));
        assert_eq!(e.stats_snapshot().rx_accepted, 1);
    }

    #[test]
    fn ignores_foreign_and_own_frames() {
        let mut e = engine();
        assert_eq!(
            e.on_frame(&golden_rt("rtc_dev_8000"), Instant::now()),
            RxVerdict::Ignored
        ); // our own frame id / src
        let mut other = golden_rt("rtc_cpu_8001");
        other[6] = 0x02; // src MAC changed
        assert_eq!(e.on_frame(&other, Instant::now()), RxVerdict::Ignored);
        assert_eq!(e.stats_snapshot().rx_ignored, 2);
    }

    #[test]
    fn drops_bad_transfer_status_and_short_csdu() {
        let mut e = engine();
        let mut f = golden_rt("rtc_cpu_8001");
        f[63] = 0x01;
        assert_eq!(
            e.on_frame(&f, Instant::now()),
            RxVerdict::Dropped(DropReason::TransferStatus(1))
        );
        assert_eq!(
            e.on_frame(&golden_rt("rtc_cpu_8001")[..50], Instant::now()),
            RxVerdict::Dropped(DropReason::Malformed)
        );
        assert_eq!(e.stats_snapshot().rx_dropped, 2);
    }

    #[test]
    fn cpu_stop_and_invalid_data() {
        let mut e = engine();
        let mut stop = golden_rt("echo_cpu_8001");
        stop[62] = 0x25; // ProviderState = Stop, still DataValid
        let v = e.on_frame(&stop, Instant::now());
        assert_eq!(
            v,
            RxVerdict::Accepted {
                provider_run: false,
                primary: true,
                data_valid: true
            }
        );
        assert_eq!(e.rx_csdu()[4], 0x01); // data still copied
        let mut invalid = golden_rt("rtc_cpu_8001");
        invalid[62] = 0x31; // DataValid cleared
        let v = e.on_frame(&invalid, Instant::now());
        assert_eq!(
            v,
            RxVerdict::Accepted {
                provider_run: true,
                primary: true,
                data_valid: false
            }
        );
        assert_eq!(e.rx_csdu()[4], 0x01); // not overwritten
        assert_eq!(e.stats_snapshot().rx_invalid, 1);
    }

    #[test]
    fn reordered_frames_are_counted_but_accepted() {
        let mut e = engine();
        let t = Instant::now();
        e.on_frame(&golden_rt("echo_cpu_8001"), t); // cc 0xe400
        let v = e.on_frame(&golden_rt("rtc_cpu_8001"), t); // cc 0xb800 (older)
        assert!(matches!(v, RxVerdict::Accepted { .. }));
        assert_eq!(e.stats_snapshot().reordered, 1);
    }

    #[test]
    fn watchdog_arms_on_first_frame_and_expires_once() {
        let mut e = engine();
        let t = Instant::now();
        assert_eq!(e.check_watchdog(t), WatchdogVerdict::NotArmed);
        e.on_frame(&golden_rt("rtc_cpu_8001"), t);
        assert_eq!(
            e.check_watchdog(t + Duration::from_millis(96)),
            WatchdogVerdict::Ok
        );
        assert_eq!(
            e.check_watchdog(t + Duration::from_millis(97)),
            WatchdogVerdict::Expired
        );
        assert_eq!(
            e.check_watchdog(t + Duration::from_millis(200)),
            WatchdogVerdict::Stopped
        );
        assert_eq!(e.stats_snapshot().watchdog_expirations, 1);
        e.on_frame(&golden_rt("rtc_cpu_8001"), t + Duration::from_millis(300));
        assert_eq!(
            e.check_watchdog(t + Duration::from_millis(310)),
            WatchdogVerdict::Ok
        );
    }

    #[test]
    fn mark_outputs_stale_forces_every_iops_bad() {
        let mut e = engine();
        e.on_frame(&golden_rt("echo_cpu_8001"), Instant::now());
        assert!(e.rx_iops_good().iter().all(|g| *g));
        e.mark_outputs_stale();
        assert!(e.rx_iops_good().iter().all(|g| !*g));
    }

    #[test]
    fn replay_whole_capture_cpu_frames() {
        // every 0x8001 golden we have parses and is accepted
        let mut e = engine();
        for name in ["rtc_cpu_8001", "echo_cpu_8001"] {
            let bytes = golden_rt(name);
            let (_, rt) = RtFrame::parse(&bytes).unwrap();
            assert_eq!(rt.frame_id, 0x8001);
            assert!(matches!(
                e.on_frame(&golden_rt(name), Instant::now()),
                RxVerdict::Accepted { .. }
            ));
        }
    }
}

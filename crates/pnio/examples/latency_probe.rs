//! HIL latency probe: measure the full edge → controller cyclic task → edge loop.
//!
//! The application writes an incrementing cycle counter (raw `u32` bits) into slot 1
//! REAL 0 every iteration; a cyclic interrupt OB on the controller (`LatencyEcho`,
//! 5 ms, process image partition TPA 1 with automatic update) copies it to slot 3
//! REAL 0 and publishes its own execution counter in slot 3 REAL 1. This probe
//! histograms:
//! - the **echo age** (our counter minus the echoed one, in app cycles → µs): the
//!   complete edge → IO → OB pickup → IO → edge latency, distribution included;
//! - the **OB cadence seen on the wire**: iterations between two changes of the OB
//!   counter (expected: the OB period), plus a jump counter (an OB increment of ≥ 2
//!   between two sightings means the wire missed one OB output — stale/overwritten);
//! - anomalies: echo going backwards, echo frozen longer than a threshold.
//!
//! Same station/config as `typed_bringup` (16 REAL + 32 BOOL per direction); needs
//! `cap_net_raw,cap_net_admin,cap_sys_nice,cap_ipc_lock+eip`.
use clap::Parser;
use pnio::api::{ApiError, IoDevice, StartOptions};
use pnio::config::{DeviceConfig, Slot};
use pnio::data::FieldType::*;
use pnio::device::RtOptions;
use pnio::rt::Histogram;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

#[derive(Parser)]
struct Args {
    /// Interface facing the controller (e.g. eno2)
    #[arg(long)]
    iface: String,
    /// PROFINET station name
    #[arg(long, default_value = "pnio-dev")]
    station: String,
    /// IPv4 address configured on the interface (must equal the one TIA assigns)
    #[arg(long)]
    ip: std::net::Ipv4Addr,
    /// SCHED_FIFO priority for the RT thread, if set
    #[arg(long)]
    rt_priority: Option<u8>,
    /// CPU to pin the RT thread to, if set
    #[arg(long)]
    cpu: Option<usize>,
    /// CPUs for the acyclic/application threads, e.g. "0-1"
    #[arg(long)]
    app_cpus: Option<String>,
    /// Lock process memory (mlockall) and pre-fault the RT stack
    #[arg(long)]
    lock_memory: bool,
    /// Probe period in microseconds (write/read cadence; use the IO update time)
    #[arg(long, default_value_t = 500)]
    period_us: u64,
    /// Stop after this many seconds (0 = run until SIGINT/SIGTERM)
    #[arg(long, default_value_t = 0)]
    duration: u64,
    /// How often (seconds) to log interim stats
    #[arg(long, default_value_t = 10)]
    stats_every: u64,
    /// Echo frozen longer than this many milliseconds counts as a stall
    #[arg(long, default_value_t = 50)]
    stall_ms: u64,
}

fn sample_config(station: &str) -> DeviceConfig {
    DeviceConfig::builder(station)
        .station_type("pnio latency probe")
        .input(Slot(1), &[Real; 16])
        .input(Slot(2), &[Bool; 32])
        .output(Slot(3), &[Real; 16])
        .output(Slot(4), &[Bool; 32])
        .build()
        .expect("sample config is valid")
}

static STOP: OnceLock<Arc<AtomicBool>> = OnceLock::new();

extern "C" fn on_signal(_: libc::c_int) {
    if let Some(stop) = STOP.get() {
        stop.store(true, Ordering::Relaxed);
    }
}

fn install_signal_handlers(stop: Arc<AtomicBool>) {
    let _ = STOP.set(stop);
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
}

/// Age of `echo` relative to `now`, both free-running u32 counters (wrap-safe).
fn age(now: u32, echo: u32) -> u32 {
    now.wrapping_sub(echo)
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let a = Args::parse();
    let app_cpus = match a.app_cpus.as_deref().map(parse_cpu_list) {
        Some(Ok(cpus)) => Some(cpus),
        Some(Err(e)) => {
            eprintln!("--app-cpus: {e}");
            std::process::exit(2);
        }
        None => None,
    };
    let dev = IoDevice::start(
        sample_config(&a.station),
        StartOptions {
            iface: a.iface.clone(),
            ip: a.ip.octets(),
            rt: Some(RtOptions {
                iface: a.iface.clone(),
                cpu_pin: a.cpu,
                rt_priority: a.rt_priority,
                lock_memory: a.lock_memory,
            }),
            app_cpus,
        },
    )
    .expect("start (need cap_net_raw/cap_net_admin/cap_sys_nice/cap_ipc_lock)");
    let stop = Arc::new(AtomicBool::new(false));
    install_signal_handlers(stop.clone());
    log::info!(
        "latency probe up on {}, waiting for the controller",
        a.iface
    );

    let period = Duration::from_micros(a.period_us);
    let cycles_per_ms = (1000.0 / a.period_us as f64).max(0.001);
    let stall_cycles = (a.stall_ms * 1000 / a.period_us).max(1) as u32;

    // Echo age and OB cadence, in probe cycles (1 bin = 1 cycle = period_us).
    let age_hist = Histogram::new();
    let ob_gap_hist = Histogram::new();

    let mut n: u32 = 0; // our counter (already published value)
    let mut last_echo: u32 = 0;
    let mut echo_frozen: u32 = 0; // iterations since the echo last changed
    let mut stalls: u64 = 0;
    let mut backwards: u64 = 0;
    let mut ob_last: u32 = 0;
    let mut ob_seen = false;
    let mut ob_gap: u32 = 0; // iterations since the OB counter last changed
    let mut ob_jumps: u64 = 0; // OB counter advanced by >= 2 between sightings
    let mut samples: u64 = 0;
    let started = std::time::Instant::now();
    let mut last_log = started;

    while !stop.load(Ordering::Relaxed) {
        // Publish the next counter value.
        n = n.wrapping_add(1);
        match dev.with_inputs(Slot(1), |w| w.real(0, f32::from_bits(n))) {
            Ok(()) | Err(ApiError::NoLayoutYet) => {}
            Err(e) => log::warn!("write: {e}"),
        }
        // Read the echo and the OB counter from the same consistent snapshot.
        match dev.outputs(Slot(3)) {
            Ok(snap) => {
                let echo = snap.real(0).map(f32::to_bits).unwrap_or(0);
                let ob = snap.real(1).map(f32::to_bits).unwrap_or(0);
                if echo != 0 {
                    samples += 1;
                    // The histogram bins are µs in the library; we feed cycle counts
                    // as "nanoseconds × 1000" so 1 bin = 1 cycle. Converted on output.
                    age_hist.record(u64::from(age(n, echo)) * 1000);
                    if age(n, echo) > u32::MAX / 2 {
                        backwards += 1; // echo from the future = it went backwards
                    }
                    if echo == last_echo {
                        echo_frozen += 1;
                        if echo_frozen == stall_cycles {
                            stalls += 1;
                        }
                    } else {
                        echo_frozen = 0;
                    }
                    last_echo = echo;
                }
                if ob != 0 {
                    if ob_seen {
                        ob_gap += 1;
                        let delta = ob.wrapping_sub(ob_last);
                        if delta != 0 {
                            ob_gap_hist.record(u64::from(ob_gap) * 1000);
                            if delta >= 2 {
                                ob_jumps += 1;
                            }
                            ob_gap = 0;
                        }
                    } else {
                        ob_seen = true;
                    }
                    ob_last = ob;
                }
            }
            Err(ApiError::NoLayoutYet) => {}
            Err(e) => log::warn!("read: {e}"),
        }
        if last_log.elapsed() >= Duration::from_secs(a.stats_every) {
            log::info!(
                "n={n} samples={samples} age p50={:?}c max={:.0}c ob_gap p50={:?}c stalls={stalls} jumps={ob_jumps} freshness={:?}",
                age_hist.percentile(50.0),
                age_hist.max_ns() as f64 / 1000.0,
                ob_gap_hist.percentile(50.0),
                dev.freshness()
            );
            last_log = std::time::Instant::now();
        }
        if a.duration > 0 && started.elapsed() >= Duration::from_secs(a.duration) {
            break;
        }
        std::thread::sleep(period);
    }
    let final_stats = dev.stats();
    let r = dev.stop();

    let ms = |cycles: f64| cycles / cycles_per_ms;
    let line = |name: &str, h: &Histogram| {
        let p = |q: f64| h.percentile(q).map(|c| ms(c as f64)).unwrap_or(0.0);
        eprintln!(
            "{name:<10} n={:<9} p50={:>6.2}ms p99={:>6.2}ms p99.99={:>6.2}ms max={:>7.2}ms",
            h.count(),
            p(50.0),
            p(99.0),
            p(99.99),
            ms(h.max_ns() as f64 / 1000.0),
        );
    };
    eprintln!(
        "--- latency_probe summary ({} s, period {} us) ---",
        started.elapsed().as_secs(),
        a.period_us
    );
    line("echo_age", &age_hist);
    line("ob_period", &ob_gap_hist);
    eprintln!(
        "samples={samples} stalls(>{} ms)={stalls} backwards={backwards} ob_jumps={ob_jumps}",
        a.stall_ms
    );
    eprintln!("device stats: {final_stats:?}");
    if let Err(e) = r {
        log::error!("device loop ended: {e}");
        std::process::exit(1);
    }
    // Informative probe: exit code reflects anomalies only.
    std::process::exit(if stalls == 0 && backwards == 0 { 0 } else { 1 });
}

/// Parse "0-2", "0,1,2" or "3" into a CPU list.
fn parse_cpu_list(s: &str) -> Result<Vec<usize>, String> {
    let mut cpus = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let a: usize = a.parse().map_err(|_| format!("bad cpu '{a}'"))?;
                let b: usize = b.parse().map_err(|_| format!("bad cpu '{b}'"))?;
                if a > b {
                    return Err(format!("bad range '{part}'"));
                }
                cpus.extend(a..=b);
            }
            None => cpus.push(part.parse().map_err(|_| format!("bad cpu '{part}'"))?),
        }
    }
    if cpus.is_empty() {
        return Err("empty cpu list".into());
    }
    Ok(cpus)
}

#[cfg(test)]
mod tests {
    use super::age;

    #[test]
    fn age_is_wrap_safe() {
        assert_eq!(age(10, 7), 3);
        assert_eq!(age(2, u32::MAX - 1), 4); // wrapped counter
        assert_eq!(age(5, 5), 0);
    }
}

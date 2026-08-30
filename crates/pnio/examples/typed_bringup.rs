//! HIL bring-up: run the device on a real interface facing an S7-1500 configured with our
//! own GSDML (station `pnio-dev`, see `gen_gsdml`), driven entirely through the typed
//! `IoDevice` facade instead of the low-level `Device`/`IoImage` API. The application
//! loop mirrors QB0..63 (slot 3, 16 REAL) -> IB0..63 (slot 1) and QB64..67 (slot 4, 32
//! BOOL) -> IB64..67 (slot 2), exactly like `tests/typed_replay.rs` does by hand.
//! Needs cap_net_raw + cap_net_admin (AF_PACKET) — e.g. `setcap cap_net_raw,cap_net_admin+eip`.
//!
//! Standalone by design (like `rt_bringup.rs`): the flags, CSV, verdict and signal-handling
//! code below is a deliberate duplicate of `rt_bringup.rs`'s, not shared through a module,
//! so each bring-up example can drift and be read on its own.
use clap::Parser;
use pnio::api::{ApiError, IoDevice, StartOptions};
use pnio::config::{DeviceConfig, Slot};
use pnio::data::FieldType::*;
use pnio::device::RtOptions;
use pnio::diag::{ChannelError, Severity};
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
    /// How often (seconds) to log the RT stats and freshness
    #[arg(long, default_value_t = 5)]
    stats_every: u64,
    /// Lock process memory (mlockall) and pre-fault the RT stack
    #[arg(long)]
    lock_memory: bool,
    /// CPUs for the main/acyclic/application threads, e.g. "0-2" or "0,1,2"
    #[arg(long)]
    app_cpus: Option<String>,
    /// Stop after this many seconds (0 = run until SIGINT/SIGTERM)
    #[arg(long, default_value_t = 0)]
    duration: u64,
    /// Write per-interval stats to this CSV; histograms go to <PATH>.hist.csv at exit
    #[arg(long)]
    csv: Option<std::path::PathBuf>,
    /// Verdict threshold: max tick lateness, us
    #[arg(long, default_value_t = 300)]
    max_lateness_us: u64,
    /// Verdict threshold: p99.99 tick lateness, us
    #[arg(long, default_value_t = 100)]
    p9999_lateness_us: u64,
    /// Verdict threshold: max interval between controller frames, us
    #[arg(long, default_value_t = 1500)]
    max_rx_interval_us: u64,
    /// Raise a channel diagnosis once the AR is up: <slot>:<channel>:<error-name>
    /// (repeatable). Cleared before shutdown. See `ChannelError::from_name` for the
    /// accepted error names.
    #[arg(long = "diag")]
    diag: Vec<String>,
    /// Backing file for the writable I&M1-3 records (kept in memory only, blank at
    /// startup, if unset)
    #[arg(long = "im-store")]
    im_store: Option<std::path::PathBuf>,
}

/// One `--diag` entry, parsed and slot-validated up front (fails fast like `--csv`).
struct DiagSpec {
    slot: Slot,
    channel: u16,
    error: ChannelError,
}

/// `<slot>:<channel>:<error-name>` -> `DiagSpec`. Errors name the bad field; the
/// caller prints the accepted error names on an unknown one.
fn parse_diag_spec(s: &str) -> Result<DiagSpec, String> {
    let parts: Vec<&str> = s.split(':').collect();
    let [slot, channel, error] = parts[..] else {
        return Err(format!(
            "'{s}': expected <slot>:<channel>:<error-name>, got {} field(s)",
            parts.len()
        ));
    };
    let slot: u16 = slot
        .parse()
        .map_err(|_| format!("'{s}': bad slot '{slot}'"))?;
    let channel: u16 = channel
        .parse()
        .map_err(|_| format!("'{s}': bad channel '{channel}'"))?;
    let error =
        ChannelError::from_name(error).ok_or_else(|| format!("'{s}': unknown error '{error}'"))?;
    Ok(DiagSpec {
        slot: Slot(slot),
        channel,
        error,
    })
}

/// Same builder as `gen_gsdml`'s `sample_config` (inline copy — see the module doc: each
/// bring-up example stays standalone).
fn sample_config(station: &str) -> DeviceConfig {
    DeviceConfig::builder(station)
        .station_type("pnio sample device")
        .identity(0xFFFF, 0x0001)
        .min_device_interval(32)
        .input(Slot(1), &[Real; 16])
        .input(Slot(2), &[Bool; 32])
        .output(Slot(3), &[Real; 16])
        .output(Slot(4), &[Bool; 32])
        .build()
        .expect("sample config is valid")
}

/// Set by the SIGINT/SIGTERM handler below; the main loop polls it every 1ms.
static STOP: OnceLock<Arc<AtomicBool>> = OnceLock::new();

extern "C" fn on_signal(_: libc::c_int) {
    if let Some(stop) = STOP.get() {
        stop.store(true, Ordering::Relaxed);
    }
}

/// Minimal SIGINT/SIGTERM hook without a crate: publish the stop flag into a static so the
/// C signal handler (which can't capture anything) can reach it, then install the handler.
fn install_signal_handlers(stop: Arc<AtomicBool>) {
    let _ = STOP.set(stop);
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let a = Args::parse();

    let app_cpus = a.app_cpus.as_deref().map(|list| {
        let cpus = parse_cpu_list(list).unwrap_or_else(|e| {
            eprintln!("--app-cpus: {e}");
            std::process::exit(2);
        });
        // `IoDevice::start` pins its own acyclic thread to this list; setting it on the
        // calling thread too means everything this `main` thread later spawns (this
        // loop stays on `main`, nothing else) inherits it as well.
        if let Err(e) = pnio::rt::sched::set_affinity(&cpus) {
            log::warn!("app affinity {cpus:?}: {e}");
        }
        if a.cpu.is_none() {
            log::warn!(
                "--app-cpus given without --cpu: the RT thread will inherit the \
                 application affinity {cpus:?} instead of running unrestricted"
            );
        }
        cpus
    });

    // Process-wide: doing it once here is equivalent to the RT thread doing it again
    // (the second `mlockall` is idempotent), but only here can we observe the result.
    let memory_locked = if a.lock_memory {
        match pnio::rt::sched::lock_memory() {
            Ok(()) => true,
            Err(e) => {
                log::warn!("mlockall: {e}");
                false
            }
        }
    } else {
        false
    };

    // Fail fast on a bad --csv path (missing parent dir, permissions, full disk):
    // create the file and write its header here, before the device starts, so a
    // bad path never leaves the RT stack running and the process can still take
    // the same `--app-cpus`-style exit(2) path instead of panicking mid-run.
    let csv_file = a.csv.as_ref().map(|p| {
        open_csv(p).unwrap_or_else(|e| {
            eprintln!("--csv: {e}");
            std::process::exit(2);
        })
    });

    // Fail fast on a bad --diag spec too, same reasoning as --csv above.
    let diags: Vec<DiagSpec> = a
        .diag
        .iter()
        .map(|s| {
            parse_diag_spec(s).unwrap_or_else(|e| {
                eprintln!("--diag {e}");
                eprintln!("accepted error names: {}", ChannelError::names().join(", "));
                std::process::exit(2);
            })
        })
        .collect();

    let cfg = sample_config(&a.station);
    let dev = IoDevice::start(
        cfg,
        StartOptions {
            iface: a.iface.clone(),
            ip: a.ip.octets(),
            rt: Some(RtOptions {
                iface: a.iface.clone(),
                cpu_pin: a.cpu,
                rt_priority: a.rt_priority,
                lock_memory: a.lock_memory,
            }),
            app_cpus: app_cpus.clone(),
            im_store: a.im_store.clone(),
        },
    )
    .expect("start (need cap_net_raw/cap_net_admin/cap_sys_nice/cap_ipc_lock)");

    let stop = Arc::new(AtomicBool::new(false));
    install_signal_handlers(stop.clone());

    log::info!(
        "device up on {} as station {:?}, waiting for the controller",
        a.iface,
        a.station
    );

    // Everything `rt_bringup.rs` splits between a blocking `dev.run(&stop)` on `main`
    // and a separate application thread lives on `main` here instead: `IoDevice::start`
    // already runs its own acyclic + RT threads, so there is nothing left to block on.
    let stats = dev.rt_stats();
    let started = std::time::Instant::now();
    let mut last_ar_state = dev.ar_state();
    log::info!("AR state: {last_ar_state:?}");
    let mut last_log = started;
    let mut last_err_log = started - Duration::from_secs(1);
    let stats_every = Duration::from_secs(a.stats_every);
    let mut csv = csv_file;
    let mut diags_raised = false;
    while !stop.load(Ordering::Relaxed) {
        let st = dev.ar_state();
        if st != last_ar_state {
            match dev.last_abort() {
                None => log::info!("AR state: {st:?}"),
                Some(r) => log::warn!("AR state: {st:?} (abort: {r:?})"),
            }
            last_ar_state = st;
        }

        // Raise every `--diag` once, the first time the AR (and its I/O layout) is
        // actually up — not on `ar_state()` alone, which can transiently lag the
        // layout (see `IoDevice::ready`'s doc).
        if !diags_raised && dev.ready() {
            for d in &diags {
                match dev.raise_diagnosis(d.slot, d.channel, d.error, Severity::Fault) {
                    Ok(()) => log::info!(
                        "raised diagnosis: slot {:?} channel {} error {:?}",
                        d.slot,
                        d.channel,
                        d.error
                    ),
                    Err(e) => log::warn!("--diag slot {:?}: {e}", d.slot),
                }
            }
            diags_raised = true;
        }

        match run_app_cycle(&dev) {
            Ok(()) => {}
            Err(e) => {
                // Rate-limited: the app cycle runs every 1ms, logging every miss
                // would flood the log for a submodule that stays unavailable.
                if last_err_log.elapsed() >= Duration::from_secs(1) {
                    log::warn!("application cycle error: {e}");
                    last_err_log = std::time::Instant::now();
                }
            }
        }

        if last_log.elapsed() >= stats_every {
            let s = dev.stats();
            log::info!("rt stats: {s:?}, freshness: {:?}", dev.freshness());
            if let Some(f) = csv.as_mut() {
                use std::io::Write;
                // Non-fatal by design: one row every `stats_every` seconds, unlike
                // the one-time create+header in `open_csv` where failing fast
                // matters — a transient write hiccup here shouldn't abort the run.
                let _ = writeln!(
                    f,
                    "{},{},{},{},{},{},{},{},{},{},{},{}",
                    started.elapsed().as_secs(),
                    s.tx,
                    s.rx_accepted,
                    s.rx_dropped,
                    s.missed_ticks,
                    s.watchdog_expirations,
                    s.input_snapshot_reused,
                    s.output_publish_deferred,
                    s.max_tick_lateness_ns / 1000,
                    stats.tick_lateness.percentile(99.99).unwrap_or(0),
                    s.max_cycle_work_ns / 1000,
                    s.max_rx_interval_ns / 1000,
                );
            }
            last_log = std::time::Instant::now();
        }
        if a.duration > 0 && started.elapsed() >= Duration::from_secs(a.duration) {
            log::info!("--duration reached, stopping");
            stop.store(true, Ordering::Relaxed);
        }
        // 1 ms: at a 1 ms update time a slower application loop would make
        // `input_snapshot_reused` meaningless.
        std::thread::sleep(Duration::from_millis(1));
    }

    // Clear every diagnosis we raised before tearing the AR down (SIGINT/SIGTERM or
    // --duration reached): a no-op for one never actually raised (AR never came up),
    // so this is safe to run unconditionally.
    for d in &diags {
        if let Err(e) = dev.clear_diagnosis(d.slot, d.channel, d.error) {
            log::warn!("--diag slot {:?}: clearing on shutdown: {e}", d.slot);
        }
    }
    // `clear_diagnosis` only queues the command: the acyclic thread picks it up on
    // its next step, emits the "disappears" alarm and only then drops the diagnosis
    // from the active list. Stopping right away would race that, and the CPU would
    // keep a stale diagnosis. Wait (up to 1 s) for the store to drain.
    if !diags.is_empty() {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !dev.diagnoses().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !dev.diagnoses().is_empty() {
            log::warn!(
                "--diag: {} diagnosis/es still active 1 s after clearing; stopping anyway",
                dev.diagnoses().len()
            );
        }
    }
    let alarm_stats = dev.alarm_stats();
    let alarm_rx_no_channel = dev.alarm_rx_no_channel();

    let stats = dev.rt_stats();
    let r = dev.stop();
    if let Some(p) = a.csv.as_ref() {
        let mut hist = p.clone().into_os_string();
        hist.push(".hist.csv");
        if let Err(e) = write_hist_csv(std::path::Path::new(&hist), &stats) {
            log::error!("--csv: writing histogram csv failed: {e}");
        }
    }
    let thresholds = Thresholds {
        max_lateness_us: a.max_lateness_us,
        p9999_lateness_us: a.p9999_lateness_us,
        max_rx_interval_us: a.max_rx_interval_us,
    };
    let pass = verdict(
        &stats,
        &thresholds,
        memory_locked,
        started.elapsed().as_secs(),
        &alarm_stats,
        alarm_rx_no_channel,
    );
    if let Err(e) = r {
        log::error!("device loop ended: {e}");
        std::process::exit(1);
    }
    std::process::exit(if pass { 0 } else { 1 });
}

/// One application cycle: mirror QB0..63 (slot 3) -> IB0..63 (slot 1), QB64..67 (slot 4)
/// -> IB64..67 (slot 2). `NoLayoutYet` (no AR yet, or one still coming up) is not an
/// error worth reporting — everything else is passed through to the caller's
/// rate-limited log.
fn run_app_cycle(dev: &IoDevice) -> Result<(), ApiError> {
    match dev.outputs(Slot(3)) {
        Ok(snap) => dev.with_inputs(Slot(1), |w| {
            for i in 0..16 {
                w.real(i, snap.real(i)?)?;
            }
            Ok(())
        })?,
        Err(ApiError::NoLayoutYet) => return Ok(()),
        Err(e) => return Err(e),
    }
    match dev.outputs(Slot(4)) {
        Ok(bits) => dev.with_inputs(Slot(2), |w| {
            for i in 0..32 {
                w.bool(i, bits.bool(i)?)?;
            }
            Ok(())
        }),
        Err(ApiError::NoLayoutYet) => Ok(()),
        Err(e) => Err(e),
    }
}

struct Thresholds {
    max_lateness_us: u64,
    p9999_lateness_us: u64,
    max_rx_interval_us: u64,
}

/// Print the summary and return true on PASS.
fn verdict(
    stats: &pnio::rt::RtStats,
    t: &Thresholds,
    memory_locked: bool,
    secs: u64,
    alarm: &pnio::alarm::AlarmStats,
    alarm_rx_no_channel: u64,
) -> bool {
    use pnio::rt::Histogram;
    let s = stats.snapshot();
    let line = |name: &str, h: &Histogram| {
        eprintln!(
            "{name:14} n={:<8} p50={:>5}us p99={:>5}us p99.99={:>5}us max={:>8.1}us",
            h.count(),
            h.percentile(50.0).unwrap_or(0),
            h.percentile(99.0).unwrap_or(0),
            h.percentile(99.99).unwrap_or(0),
            h.max_ns() as f64 / 1000.0
        );
    };
    eprintln!("--- typed_bringup summary ({secs} s) ---");
    line("tick_lateness", &stats.tick_lateness);
    line("cycle_work", &stats.cycle_work);
    line("rx_interval", &stats.rx_interval);
    eprintln!(
        "tx={} rx_accepted={} rx_dropped={} missed_ticks={} watchdog_expirations={} reused={} \
         deferred={} memory_locked={}",
        s.tx,
        s.rx_accepted,
        s.rx_dropped,
        s.missed_ticks,
        s.watchdog_expirations,
        s.input_snapshot_reused,
        s.output_publish_deferred,
        if memory_locked { "yes" } else { "no" }
    );
    // Per-AR alarm channel counters (reset on every reconnect, per `AlarmStats`'
    // doc) plus the one cumulative counter, `rx_no_channel`.
    eprintln!(
        "alarm: sent={} acked={} retries={} unexpected_rx={} send_failures={} \
         rx_err_rta={} rx_no_channel={}",
        alarm.sent,
        alarm.acked,
        alarm.retries,
        alarm.unexpected_rx,
        alarm.send_failures,
        alarm.rx_err_rta,
        alarm_rx_no_channel,
    );
    let mut fails = Vec::new();
    if s.missed_ticks != 0 {
        fails.push(format!("missed_ticks={}", s.missed_ticks));
    }
    if s.watchdog_expirations != 0 {
        fails.push(format!("watchdog_expirations={}", s.watchdog_expirations));
    }
    let lat_max = s.max_tick_lateness_ns / 1000;
    if lat_max >= t.max_lateness_us {
        fails.push(format!(
            "lateness max {lat_max}us >= {}us",
            t.max_lateness_us
        ));
    }
    let p = stats.tick_lateness.percentile(99.99).unwrap_or(0);
    if p >= t.p9999_lateness_us {
        fails.push(format!(
            "lateness p99.99 {p}us >= {}us",
            t.p9999_lateness_us
        ));
    }
    let rx = s.max_rx_interval_ns / 1000;
    if rx >= t.max_rx_interval_us {
        fails.push(format!(
            "rx_interval max {rx}us >= {}us",
            t.max_rx_interval_us
        ));
    }
    if s.tx == 0 {
        fails.push("no cyclic exchange happened".into());
    }
    let short = if secs < 60 { " (short run)" } else { "" };
    if fails.is_empty() {
        eprintln!("VERDICT: PASS{short}");
        true
    } else {
        eprintln!("VERDICT: FAIL{short} ({})", fails.join(", "));
        false
    }
}

/// Create the per-interval stats CSV and write its header. Called once in `main`,
/// before the device starts, so a bad `--csv` path fails fast (see `main`) instead
/// of panicking mid-run.
fn open_csv(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    writeln!(
        f,
        "t_s,tx,rx_accepted,rx_dropped,missed_ticks,watchdog_expirations,reused,\
         deferred,lat_max_us,lat_p9999_us,work_max_us,rxint_max_us"
    )?;
    Ok(f)
}

fn write_hist_csv(path: &std::path::Path, stats: &pnio::rt::RtStats) -> std::io::Result<()> {
    use std::io::Write;
    let (a, b, c) = (
        stats.tick_lateness.snapshot(),
        stats.cycle_work.snapshot(),
        stats.rx_interval.snapshot(),
    );
    let mut f = std::fs::File::create(path)?;
    writeln!(f, "bin_us,tick_lateness,cycle_work,rx_interval")?;
    for i in 0..pnio::rt::HIST_BINS {
        writeln!(f, "{i},{},{},{}", a.bins[i], b.bins[i], c.bins[i])?;
    }
    Ok(())
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
    use super::parse_cpu_list;

    #[test]
    fn parses_ranges_and_lists() {
        assert_eq!(parse_cpu_list("0-2").unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_cpu_list("0,1,2").unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_cpu_list("3").unwrap(), vec![3]);
        assert_eq!(parse_cpu_list("0-1,3").unwrap(), vec![0, 1, 3]);
        assert!(parse_cpu_list("").is_err());
        assert!(parse_cpu_list("2-1").is_err());
        assert!(parse_cpu_list("x").is_err());
    }
}

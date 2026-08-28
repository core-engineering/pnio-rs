//! HIL bring-up: run the device on a real interface facing an S7-1500 configured with the
//! p-net sample GSDML (station `rt-labs-dev`), with the cyclic (RT) thread enabled. The
//! application thread mirrors QB0 -> IB0, QB1 -> IB1, and echoes the Echo module's outputs
//! back into its inputs, exactly like `tests/rt_replay.rs` does by hand.
//! Needs cap_net_raw + cap_net_admin (AF_PACKET) — e.g. `setcap cap_net_raw,cap_net_admin+eip`.
use clap::Parser;
use profinet_rt::cm::model::DeviceModel;
use profinet_rt::dcp::{DeviceConfig, DeviceProperties};
use profinet_rt::device::{Device, DeviceSetup, RtOptions};
use profinet_rt::eth::{AfPacketTransport, MacAddr};
use profinet_rt::rpc::{UdpRpcTransport, Uuid, PNIO_UDP_PORT};
use profinet_rt::rt::{ImageError, IoImage};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

#[derive(Parser)]
struct Args {
    /// Interface facing the controller (e.g. eno2)
    #[arg(long)]
    iface: String,
    /// PROFINET station name
    #[arg(long, default_value = "rt-labs-dev")]
    name: String,
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
    /// Verdict threshold: max tick lateness, µs
    #[arg(long, default_value_t = 300)]
    max_lateness_us: u64,
    /// Verdict threshold: p99.99 tick lateness, µs
    #[arg(long, default_value_t = 100)]
    p9999_lateness_us: u64,
    /// Verdict threshold: max interval between controller frames, µs
    #[arg(long, default_value_t = 1500)]
    max_rx_interval_us: u64,
}

fn mac_of(iface: &str) -> MacAddr {
    let s = std::fs::read_to_string(format!("/sys/class/net/{iface}/address")).expect("iface mac");
    let mut m = [0u8; 6];
    for (i, p) in s.trim().split(':').enumerate() {
        m[i] = u8::from_str_radix(p, 16).expect("mac");
    }
    MacAddr(m)
}

/// Set by the SIGINT/SIGTERM handler below; `Device::run` polls it at least every 200ms.
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

    if let Some(list) = a.app_cpus.as_deref() {
        let cpus = parse_cpu_list(list).unwrap_or_else(|e| {
            eprintln!("--app-cpus: {e}");
            std::process::exit(2);
        });
        // Threads spawned later (application loop) inherit this; the RT thread sets
        // its own affinity from --cpu.
        if let Err(e) = profinet_rt::rt::sched::set_affinity(&cpus) {
            log::warn!("app affinity {cpus:?}: {e}");
        }
    }
    // Process-wide: doing it once here is equivalent to the RT thread doing it again
    // (the second `mlockall` is idempotent), but only here can we observe the result.
    let memory_locked = if a.lock_memory {
        match profinet_rt::rt::sched::lock_memory() {
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

    let mac = mac_of(&a.iface);
    let ip = a.ip.octets();
    let setup = DeviceSetup {
        dcp: DeviceConfig {
            mac,
            properties: DeviceProperties {
                name_of_station: a.name.clone(),
                type_of_station: "profinet-rt bring-up".into(),
                vendor_id: 0x0493,
                device_id: 0x0002,
                device_role: 0x0100,
                device_instance: 1,
                device_options: vec![1, 2, 2, 2, 2, 3],
                ip,
                subnet: [255, 255, 255, 0],
                gateway: ip,
                ip_block_info: 1,
            },
        },
        model: {
            let mut m = DeviceModel::pnet_sample(mac);
            m.station_name = a.name;
            m
        },
        activity_seed: {
            let mut b = [
                0x14, 0xaf, 0x19, 0x8a, 0x12, 0x34, 0x10, 0x56, 0x80, 0x79, 0, 0, 0, 0, 0, 0,
            ];
            b[10..].copy_from_slice(&mac.0);
            Uuid(b)
        },
        rt: Some(RtOptions {
            iface: a.iface.clone(),
            cpu_pin: a.cpu,
            rt_priority: a.rt_priority,
            lock_memory: a.lock_memory,
        }),
    };
    let eth = AfPacketTransport::open(&a.iface).expect("AF_PACKET (need cap_net_raw)");
    eth.attach_filter(&profinet_rt::eth::bpf::acyclic_filter())
        .expect("attach acyclic BPF filter");
    let rpc = UdpRpcTransport::bind(std::net::SocketAddr::from(([0, 0, 0, 0], PNIO_UDP_PORT)))
        .expect("udp 34964");
    let stop = Arc::new(AtomicBool::new(false));
    install_signal_handlers(stop.clone());
    let mut dev = Device::new(setup, eth, rpc);
    dev.on_state_change(|st, why| match why {
        None => log::info!("AR state: {st:?}"),
        Some(r) => log::warn!("AR state: {st:?} (abort: {r:?})"),
    });

    // Application loop: mirrors QB0 -> IB0, QB1 -> IB1, and echoes the Echo module's
    // outputs into its inputs, every 1ms. Runs on its own thread so it doesn't block
    // the acyclic `Device::run` loop; the image is lock-free to read/write from here
    // while the RT thread publishes/consumes it concurrently.
    let image = dev.image();
    let stats_main = dev.rt_stats();
    let stats = dev.rt_stats();
    let app_stop = stop.clone();
    let duration = a.duration;
    let app = std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let mut last_log = started;
        let mut last_err_log = started - Duration::from_secs(1);
        let stats_every = Duration::from_secs(a.stats_every);
        let mut csv = csv_file;
        while !app_stop.load(Ordering::Relaxed) {
            for r in run_app_cycle(&image) {
                match r {
                    Ok(()) | Err(ImageError::UnknownSubmodule { .. }) => {} // no AR yet: retry
                    Err(e) => {
                        // Rate-limited: the app cycle runs every 1ms, logging every miss
                        // would flood the log for a submodule that stays unavailable.
                        if last_err_log.elapsed() >= Duration::from_secs(1) {
                            log::warn!("application cycle error: {e}");
                            last_err_log = std::time::Instant::now();
                        }
                    }
                }
            }
            if last_log.elapsed() >= stats_every {
                let s = stats.snapshot();
                log::info!(
                    "rt stats: {s:?}, freshness: {:?}",
                    image.validity().freshness()
                );
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
            if duration > 0 && started.elapsed() >= Duration::from_secs(duration) {
                log::info!("--duration reached, stopping");
                app_stop.store(true, Ordering::Relaxed);
            }
            // 1 ms: at a 1 ms update time a slower application loop would make
            // `input_snapshot_reused` meaningless.
            std::thread::sleep(Duration::from_millis(1));
        }
    });

    log::info!(
        "device up on {} as {:?}, waiting for the controller",
        a.iface,
        mac
    );
    let started = std::time::Instant::now();
    let run_result = dev.run(&stop);
    stop.store(true, Ordering::Relaxed);
    let _ = app.join();
    // Stops and bounded-joins the RT thread (Device's Drop -> stop_runner), so the
    // histograms read below are final rather than still being written concurrently.
    drop(dev);
    if let Some(p) = a.csv.as_ref() {
        let mut hist = p.clone().into_os_string();
        hist.push(".hist.csv");
        if let Err(e) = write_hist_csv(std::path::Path::new(&hist), &stats_main) {
            log::error!("--csv: writing histogram csv failed: {e}");
        }
    }
    let thresholds = Thresholds {
        max_lateness_us: a.max_lateness_us,
        p9999_lateness_us: a.p9999_lateness_us,
        max_rx_interval_us: a.max_rx_interval_us,
    };
    let pass = verdict(
        &stats_main,
        &thresholds,
        memory_locked,
        started.elapsed().as_secs(),
    );
    if let Err(e) = run_result {
        log::error!("device loop ended: {e}");
        std::process::exit(1);
    }
    std::process::exit(if pass { 0 } else { 1 });
}

/// One application cycle: mirror QB0 -> IB0, QB1 -> IB1, echo the Echo module's outputs
/// back into its inputs. Each mirror is attempted independently and reports its own
/// result, so one submodule going away (e.g. a slot pulled) never skips the others.
fn run_app_cycle(image: &IoImage) -> [Result<(), ImageError>; 3] {
    [
        image
            .read_outputs(2, 1, |b, _| b[0])
            .and_then(|v| image.write_inputs(1, 1, &[v])),
        image
            .read_outputs(3, 1, |b, _| b[0])
            .and_then(|v| image.write_inputs(3, 1, &[v])),
        image
            .read_outputs(4, 1, |b, _| b.to_vec())
            .and_then(|v| image.write_inputs(4, 1, &v)),
    ]
}

struct Thresholds {
    max_lateness_us: u64,
    p9999_lateness_us: u64,
    max_rx_interval_us: u64,
}

/// Print the summary and return true on PASS.
fn verdict(
    stats: &profinet_rt::rt::RtStats,
    t: &Thresholds,
    memory_locked: bool,
    secs: u64,
) -> bool {
    use profinet_rt::rt::Histogram;
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
    eprintln!("--- rt_bringup summary ({secs} s) ---");
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

fn write_hist_csv(path: &std::path::Path, stats: &profinet_rt::rt::RtStats) -> std::io::Result<()> {
    use std::io::Write;
    let (a, b, c) = (
        stats.tick_lateness.snapshot(),
        stats.cycle_work.snapshot(),
        stats.rx_interval.snapshot(),
    );
    let mut f = std::fs::File::create(path)?;
    writeln!(f, "bin_us,tick_lateness,cycle_work,rx_interval")?;
    for i in 0..profinet_rt::rt::HIST_BINS {
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

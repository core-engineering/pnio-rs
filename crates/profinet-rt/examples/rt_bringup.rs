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
            lock_memory: false, // Task 5 adds the flag
        }),
    };
    let eth = AfPacketTransport::open(&a.iface).expect("AF_PACKET (need cap_net_raw)");
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
    // outputs into its inputs, every 10ms. Runs on its own thread so it doesn't block
    // the acyclic `Device::run` loop; the image is lock-free to read/write from here
    // while the RT thread publishes/consumes it concurrently.
    let image = dev.image();
    let stats = dev.rt_stats();
    let app_stop = stop.clone();
    let app = std::thread::spawn(move || {
        let mut last_log = std::time::Instant::now();
        let mut last_err_log = std::time::Instant::now() - Duration::from_secs(1);
        let stats_every = Duration::from_secs(a.stats_every);
        while !app_stop.load(Ordering::Relaxed) {
            for r in run_app_cycle(&image) {
                match r {
                    Ok(()) | Err(ImageError::UnknownSubmodule { .. }) => {} // no AR yet: retry
                    Err(e) => {
                        // Rate-limited: the app cycle runs every 10ms, logging every miss
                        // would flood the log for a submodule that stays unavailable.
                        if last_err_log.elapsed() >= Duration::from_secs(1) {
                            log::warn!("application cycle error: {e}");
                            last_err_log = std::time::Instant::now();
                        }
                    }
                }
            }
            if last_log.elapsed() >= stats_every {
                log::info!(
                    "rt stats: {:?}, freshness: {:?}",
                    stats.snapshot(),
                    image.validity().freshness()
                );
                last_log = std::time::Instant::now();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });

    log::info!(
        "device up on {} as {:?}, waiting for the controller",
        a.iface,
        mac
    );
    if let Err(e) = dev.run(&stop) {
        log::error!("device loop ended: {e}");
        stop.store(true, Ordering::Relaxed);
        let _ = app.join();
        std::process::exit(1);
    }
    stop.store(true, Ordering::Relaxed);
    let _ = app.join();
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

//! HIL bring-up: run the device on a real interface facing an S7-1500 configured with the
//! p-net sample GSDML (station `rt-labs-dev`). Success = a log line `AR state: Data`.
//! Needs cap_net_raw + cap_net_admin (AF_PACKET) — e.g. `setcap cap_net_raw,cap_net_admin+eip`.
//!
//! Linux only (`AF_PACKET`, `SCHED_FIFO`): on other platforms this example only prints a note.

#[cfg(target_os = "linux")]
#[path = "linux/ar_bringup.rs"]
mod app;

#[cfg(target_os = "linux")]
fn main() {
    app::main()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the `ar_bringup` example needs Linux (AF_PACKET raw sockets and SCHED_FIFO)");
    std::process::exit(2);
}

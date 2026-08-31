//! HIL bring-up: run the device on a real interface facing an S7-1500 configured with the
//! p-net sample GSDML (station `rt-labs-dev`), with the cyclic (RT) thread enabled. The
//! application thread mirrors QB0 -> IB0, QB1 -> IB1, and echoes the Echo module's outputs
//! back into its inputs, exactly like `tests/rt_replay.rs` does by hand.
//! Needs cap_net_raw + cap_net_admin (AF_PACKET) — e.g. `setcap cap_net_raw,cap_net_admin+eip`.
//!
//! Linux only (`AF_PACKET`, `SCHED_FIFO`): on other platforms this example only prints a note.

#[cfg(target_os = "linux")]
#[path = "linux/rt_bringup.rs"]
mod app;

#[cfg(target_os = "linux")]
fn main() {
    app::main()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the `rt_bringup` example needs Linux (AF_PACKET raw sockets and SCHED_FIFO)");
    std::process::exit(2);
}

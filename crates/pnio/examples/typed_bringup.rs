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
//!
//! Linux only (`AF_PACKET`, `SCHED_FIFO`): on other platforms this example only prints a note.

#[cfg(target_os = "linux")]
#[path = "linux/typed_bringup.rs"]
mod app;

#[cfg(target_os = "linux")]
fn main() {
    app::main()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the `typed_bringup` example needs Linux (AF_PACKET raw sockets and SCHED_FIFO)");
    std::process::exit(2);
}

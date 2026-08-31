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
//!
//! Linux only (`AF_PACKET`, `SCHED_FIFO`): on other platforms this example only prints a note.

#[cfg(target_os = "linux")]
#[path = "linux/latency_probe.rs"]
mod app;

#[cfg(target_os = "linux")]
fn main() {
    app::main()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the `latency_probe` example needs Linux (AF_PACKET raw sockets and SCHED_FIFO)");
    std::process::exit(2);
}

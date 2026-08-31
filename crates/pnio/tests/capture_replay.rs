#![cfg(feature = "capture")]
//! Integration test: replays a pcap fixture if present (otherwise skipped).
use pnio::capture::PcapFrames;
use std::path::Path;

#[test]
fn replay_fixture_if_present() {
    let p = Path::new("tests/fixtures/sample.pcap");
    if !p.exists() {
        eprintln!("no sample.pcap fixture — test skipped");
        return;
    }
    let frames: Vec<Vec<u8>> = PcapFrames::open(p)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let n = frames.len();
    assert!(n > 0, "the pcap fixture must not be empty");
}

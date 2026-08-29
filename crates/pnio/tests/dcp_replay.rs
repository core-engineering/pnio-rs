//! End-to-end: replay a pcapng holding a real DCP Identify request through the
//! capture -> eth -> dcp pipeline and check the device produces the golden response.

use pnio::capture::PcapFrames;
use pnio::dcp::{handle_dcp_frame, DeviceConfig, DeviceProperties};
use pnio::eth::MacAddr;
use std::io::Cursor;

// Golden Identify REQUEST frame (56 bytes), docs/dcp-golden-frames.md.
const REQ_FRAME: &[u8] = &[
    0x01, 0x0e, 0xcf, 0x00, 0x00, 0x00, 0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f, 0x88, 0x92, 0xfe, 0xfe,
    0x05, 0x00, 0x03, 0x00, 0x01, 0x52, 0x00, 0x01, 0x00, 0x0c, 0x02, 0x02, 0x00, 0x08, 0x69, 0x2d,
    0x64, 0x65, 0x76, 0x69, 0x63, 0x65, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

// Golden Identify RESPONSE frame (114 bytes), docs/dcp-golden-frames.md.
const RESP_FRAME: &[u8] = &[
    0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f, 0x02, 0xc0, 0xa8, 0x01, 0x0f, 0x02, 0x88, 0x92, 0xfe, 0xff,
    0x05, 0x01, 0x03, 0x00, 0x01, 0x52, 0x00, 0x00, 0x00, 0x58, 0x02, 0x02, 0x00, 0x0a, 0x00, 0x00,
    0x69, 0x2d, 0x64, 0x65, 0x76, 0x69, 0x63, 0x65, 0x02, 0x05, 0x00, 0x04, 0x00, 0x00, 0x02, 0x07,
    0x02, 0x01, 0x00, 0x12, 0x00, 0x00, 0x53, 0x37, 0x2d, 0x31, 0x35, 0x30, 0x30, 0x20, 0x28, 0x50,
    0x4c, 0x43, 0x53, 0x49, 0x4d, 0x29, 0x02, 0x03, 0x00, 0x06, 0x00, 0x00, 0x00, 0x2a, 0x01, 0x0e,
    0x02, 0x04, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x02, 0x07, 0x00, 0x04, 0x00, 0x00, 0x10, 0x64,
    0x01, 0x02, 0x00, 0x0e, 0x00, 0x01, 0xc0, 0xa8, 0x01, 0x3d, 0xff, 0xff, 0xff, 0x00, 0xc0, 0xa8,
    0x01, 0x3d,
];

/// Build an in-memory pcapng holding the given Ethernet frames.
/// Mirrors the proven helper in `src/capture.rs`'s test module:
/// PcapNgWriter + InterfaceDescriptionBlock(ETHERNET) + EnhancedPacketBlock per frame.
fn make_pcapng(frames: &[&[u8]]) -> Vec<u8> {
    use pcap_file::pcapng::blocks::enhanced_packet::EnhancedPacketBlock;
    use pcap_file::pcapng::blocks::interface_description::InterfaceDescriptionBlock;
    use pcap_file::pcapng::PcapNgWriter;
    use pcap_file::DataLink;
    use std::borrow::Cow;
    use std::time::Duration;

    let mut buf = Vec::new();
    {
        let mut w = PcapNgWriter::new(&mut buf).unwrap();
        let iface = InterfaceDescriptionBlock {
            linktype: DataLink::ETHERNET,
            snaplen: 0xFFFF,
            options: vec![],
        };
        w.write_pcapng_block(iface).unwrap();
        for f in frames {
            let pkt = EnhancedPacketBlock {
                interface_id: 0,
                timestamp: Duration::ZERO,
                original_len: f.len() as u32,
                data: Cow::Borrowed(f),
                options: vec![],
            };
            w.write_pcapng_block(pkt).unwrap();
        }
    }
    buf
}

fn device_cfg() -> DeviceConfig {
    DeviceConfig {
        mac: MacAddr([0x02, 0xc0, 0xa8, 0x01, 0x0f, 0x02]),
        properties: DeviceProperties {
            name_of_station: "i-device".to_string(),
            type_of_station: "S7-1500 (PLCSIM)".to_string(),
            vendor_id: 0x002a,
            device_id: 0x010e,
            device_role: 0x0000,
            device_instance: 0x1064,
            device_options: vec![0x02, 0x07],
            ip: [192, 168, 1, 61],
            subnet: [255, 255, 255, 0],
            gateway: [192, 168, 1, 61],
            ip_block_info: 0x0001,
        },
    }
}

#[test]
fn replay_identify_request_produces_golden_response() {
    let pcapng = make_pcapng(&[REQ_FRAME]);
    let cfg = device_cfg();

    let mut responses = Vec::new();
    for frame in PcapFrames::from_reader(Cursor::new(pcapng)).unwrap() {
        let frame = frame.expect("frame read");
        if let Some(resp) = handle_dcp_frame(&frame, &cfg).unwrap() {
            responses.push(resp);
        }
    }

    assert_eq!(responses.len(), 1, "exactly one Identify response expected");
    assert_eq!(
        responses[0], RESP_FRAME,
        "response must be byte-exact golden"
    );
}

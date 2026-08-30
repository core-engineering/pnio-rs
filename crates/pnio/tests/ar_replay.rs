//! Replay the 2026-08-27 reference AR exchange through Device with mock transports and check
//! every emitted PDU is byte-identical to what p-net sent to the real S7-1500.
mod common;

use common::{golden, RPC_OFF};
use pnio::cm::model::DeviceModel;
use pnio::cm::ArState;
use pnio::dcp::{DcpConfig, DeviceProperties};
use pnio::device::{Device, DeviceSetup};
use pnio::eth::{MacAddr, MockTransport};
use pnio::im::Im0;
use pnio::rpc::{MockRpcTransport, Uuid};
use std::time::{Duration, Instant};

const MAC: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);

#[test]
fn reference_exchange_replays_byte_exact() {
    let setup = DeviceSetup {
        dcp: DcpConfig {
            mac: MAC,
            properties: DeviceProperties {
                name_of_station: "rt-labs-dev".into(),
                type_of_station: "P-Net Sample Application".into(),
                vendor_id: 0x0493,
                device_id: 0x0002,
                device_role: 0x0100,
                device_instance: 1,
                device_options: vec![1, 2, 2, 2, 2, 3],
                ip: [172, 16, 2, 10],
                subnet: [255, 255, 255, 0],
                gateway: [172, 16, 2, 10],
                ip_block_info: 1,
            },
        },
        model: DeviceModel::pnet_sample(MAC),
        activity_seed: Uuid::parse_str("14af198a-1234-1056-8079-8cf319cd19f8").unwrap(),
        rt: None,
        im0: Im0::default(),
        im_store: None,
    };
    let eth = MockTransport::new();
    let rpc = MockRpcTransport::new();
    let cpu = "172.16.2.100:54766".parse().unwrap();
    let cpu_cm = "172.16.2.100:34964".parse().unwrap();
    eth.push_rx(golden("dcp_set_req"));
    for name in ["connect_req", "write_req", "prmend_req"] {
        rpc.push_rx(golden(name)[RPC_OFF..].to_vec(), cpu);
    }
    rpc.push_rx(golden("appready_res")[RPC_OFF..].to_vec(), cpu_cm);
    let mut dev = Device::new(setup, eth, rpc);
    dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    assert_eq!(dev.state(), ArState::Data);
    assert_eq!(dev.eth().sent(), vec![golden("dcp_set_res")]);
    let sent: Vec<Vec<u8>> = dev.rpc().sent().into_iter().map(|(b, _)| b).collect();
    let expected: Vec<Vec<u8>> = ["connect_res", "write_res", "prmend_res", "appready_req"]
        .iter()
        .map(|n| golden(n)[RPC_OFF..].to_vec())
        .collect();
    assert_eq!(sent, expected);
}

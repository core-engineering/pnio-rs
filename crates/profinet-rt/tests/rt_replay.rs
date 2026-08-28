//! Cyclic replay: AR to Data through Device (mocks), then the engine consumes the bench CPU frames
//! and produces ours; the application reads/writes through IoImage.
mod common;

use common::{golden, golden_rt, RPC_OFF, RT_CSDU_OFF};
use profinet_rt::cm::{ArState, DeviceModel};
use profinet_rt::dcp::{DeviceConfig, DeviceProperties};
use profinet_rt::device::{Device, DeviceSetup};
use profinet_rt::eth::{MacAddr, MockTransport};
use profinet_rt::rpc::{MockRpcTransport, Uuid};
use profinet_rt::rt::{
    Freshness, IoImage, Layout, RtEngine, RtStats, RxVerdict, Validity, WatchdogState, IOXS_GOOD,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

const CPU: MacAddr = MacAddr([0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f]);
const DEV: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);

fn setup() -> DeviceSetup {
    DeviceSetup {
        dcp: DeviceConfig {
            mac: DEV,
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
        model: DeviceModel::pnet_sample(DEV),
        activity_seed: Uuid::parse_str("14af198a-1234-1056-8079-8cf319cd19f8").unwrap(),
        rt: None,
    }
}

#[test]
fn cyclic_round_trip_over_the_bench_frames() {
    let rpc = MockRpcTransport::new();
    let cpu = "172.16.2.100:54766".parse().unwrap();
    let cpu_cm = "172.16.2.100:34964".parse().unwrap();
    for n in ["connect_req", "write_req", "prmend_req"] {
        rpc.push_rx(golden(n)[RPC_OFF..].to_vec(), cpu);
    }
    rpc.push_rx(golden("appready_res")[RPC_OFF..].to_vec(), cpu_cm);
    let mut dev = Device::new(setup(), MockTransport::new(), rpc);
    dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    assert_eq!(dev.state(), ArState::Data);

    // What the runner would do at Data:
    let params = dev.ar_params().expect("params in Data");
    let layout = Layout::from_ar(&params, &DeviceModel::pnet_sample(DEV)).unwrap();
    let image = Arc::new(IoImage::new(&layout));
    let stats = Arc::new(RtStats::default());
    let mut engine = RtEngine::new(layout, DEV, CPU, stats.clone());

    // Application mirrors QB0 -> IB0 and echoes the Echo module, like rt_bringup does.
    let t0 = Instant::now();
    let v = engine.on_frame(&golden_rt("echo_cpu_8001"), t0);
    assert!(matches!(
        v,
        RxVerdict::Accepted {
            data_valid: true,
            ..
        }
    ));
    assert!(image.rt_publish(
        engine.rx_csdu(),
        Validity {
            provider_run: true,
            primary: true,
            watchdog: WatchdogState::Ok,
            last_rx_age: Some(Duration::ZERO),
            cycle: 1,
        }
    ));
    let qb0 = image
        .read_outputs(2, 1, |b, v| {
            assert_eq!(v.freshness(), Freshness::Fresh);
            b[0]
        })
        .unwrap();
    let echo = image.read_outputs(4, 1, |b, _| b.to_vec()).unwrap();
    image.write_inputs(1, 1, &[qb0]).unwrap();
    image.write_inputs(4, 1, &echo).unwrap();

    let mut snap = vec![0u8; 40];
    assert!(image.rt_snapshot_inputs(&mut snap));
    let frame = engine.on_tick(1, &snap).to_vec();
    assert_eq!(
        &frame[..12],
        &[0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f, 0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]
    );
    assert_eq!(
        &frame[12..20],
        &[0x81, 0x00, 0xc0, 0x00, 0x88, 0x92, 0x80, 0x00]
    );
    let c = &frame[RT_CSDU_OFF..RT_CSDU_OFF + 40];
    assert_eq!(c[3], 0x01); // IB0 mirrors QB0
    assert_eq!(&c[9..17], &[0x12, 0x34, 0x56, 0x78, 0x3f, 0xc0, 0x00, 0x00]); // true echo
    for off in [0, 1, 2, 4, 5, 7, 8, 17, 18] {
        assert_eq!(c[off], IOXS_GOOD, "ioxs at {off}");
    }
    assert_eq!(&frame[60..64], &[0x04, 0x00, 0x35, 0x00]);
    assert_eq!(stats.snapshot().tx, 1);
}

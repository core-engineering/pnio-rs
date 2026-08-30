//! Replay the 2026-08-30 alarm/I&M capture through `Device` with mock transports: the
//! device must emit p-net's bytes for every frame it originates.
mod common;

use common::{golden, golden_alarm, RPC_OFF};
use pnio::alarm::{parse_frame, AlarmType, RtaBody, RtaData};
use pnio::cm::model::DeviceModel;
use pnio::cm::{ArState, PnioStatus};
use pnio::config::{Direction, Slot};
use pnio::dcp::{DcpConfig, DeviceProperties};
use pnio::device::{Device, DeviceSetup, DiagCommand};
use pnio::diag::{ChannelError, Diagnosis, Severity};
use pnio::eth::{MacAddr, MockTransport};
use pnio::im::{Im0, SwRevision};
use pnio::rpc::{Drep, MockRpcTransport, Uuid};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const MAC: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);

/// The controller MAC baked into the 2026-08-30 alarm capture differs from the `cm`
/// goldens' `initiator_mac` in the last byte (two different bench runs — the alarm
/// capture was taken on the CPU's other port); `AlarmChannel::on_frame` drops frames
/// whose source is not the AR's `initiator_mac`, so inbound alarm goldens must be
/// retargeted to it before they are fed to the device.
const CPU_MAC: [u8; 6] = [0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f];

fn cpu_alarm(name: &str) -> Vec<u8> {
    let mut f = golden_alarm(name);
    f[6..12].copy_from_slice(&CPU_MAC);
    f
}

/// The mirror of `cpu_alarm` for a golden the *device* sent (a `..._dev` file): our
/// AR's `initiator_mac` is the `cm` goldens' one, not the alarm capture's own CPU
/// MAC, so a frame our device addresses to the controller carries `CPU_MAC` as its
/// destination. Retarget the golden's destination MAC the same way before comparing
/// it byte-exact against what the device actually sent.
fn dev_alarm(name: &str) -> Vec<u8> {
    let mut f = golden_alarm(name);
    f[0..6].copy_from_slice(&CPU_MAC);
    f
}

/// Replaces every occurrence of `old`'s 16 big-endian bytes with `new`'s, wherever it
/// appears in `bytes`. Mirrors `cm::mod::tests::retag_ar_uuid` (a private test-only
/// helper of the library crate, unreachable from here).
fn retag_ar_uuid(mut bytes: Vec<u8>, old: Uuid, new: Uuid) -> Vec<u8> {
    let mut old_b = Vec::new();
    old.write(&mut old_b, Drep::BIG);
    let mut new_b = Vec::new();
    new.write(&mut new_b, Drep::BIG);
    let mut i = 0;
    while i + 16 <= bytes.len() {
        if bytes[i..i + 16] == old_b[..] {
            bytes[i..i + 16].copy_from_slice(&new_b);
        }
        i += 1;
    }
    bytes
}

fn pnet_setup() -> DeviceSetup {
    DeviceSetup {
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
        // The p-net sample application's own I&M0 identity (matches the 2026-08-30
        // I&M0 Read golden byte-for-byte, see `im::tests::pnet_im0`).
        im0: Im0 {
            order_id: "12345 Abcdefghijk".to_string(),
            serial_number: "007".to_string(),
            hardware_revision: 3,
            software_revision: SwRevision {
                prefix: 'V',
                functional: 0,
                bug_fix: 2,
                internal: 0,
            },
            revision_counter: 0,
            profile_id: 0x1234,
            profile_specific_type: 0x5678,
        },
        im_store: None,
    }
}

/// Feeds the `cm` goldens' bring-up sequence (Connect/Write/PrmEnd/AppReady) to `dev`,
/// retagged from the `cm` goldens' `ar_uuid` (`e5e1aecc-...`) to the alarm capture's
/// (`ef796d60-...`, a different p-net session) so the I&M0 Read that follows — which
/// carries that `ar_uuid` — is answered by the AR this brings up rather than refused
/// as foreign (see `cm::mod::tests::read_response_matches_the_pnet_im0_golden_byte_exact`).
///
/// `seq_base`, when set, renumbers the three Connect/Write/PrmEnd requests' DCE-RPC
/// `seq_num` (LE, offset 64) so a second bring-up is not answered from `Cm`'s
/// per-`(activity, seq_num)` response cache as an exact retransmission — needed for
/// the reconnect after the controller's ERR-RTA has dropped the AR back to `Idle`.
fn feed_bring_up(dev: &Device<MockTransport, MockRpcTransport>, seq_base: Option<u32>) {
    let old = Uuid::parse_str("e5e1aecc-b133-4b4d-b187-cc68b0211ed2").unwrap();
    let new = Uuid::parse_str("ef796d60-ef2b-9946-b39e-8531f5b7f966").unwrap();
    let cpu = "172.16.2.100:54766".parse().unwrap();
    let cpu_cm = "172.16.2.100:34964".parse().unwrap();
    for (i, name) in ["connect_req", "write_req", "prmend_req"]
        .iter()
        .enumerate()
    {
        let mut pdu = retag_ar_uuid(golden(name)[RPC_OFF..].to_vec(), old, new);
        if let Some(base) = seq_base {
            pdu[64..68].copy_from_slice(&(base + i as u32).to_le_bytes());
        }
        dev.rpc().push_rx(pdu, cpu);
    }
    dev.rpc()
        .push_rx(golden("appready_res")[RPC_OFF..].to_vec(), cpu_cm);
}

#[test]
fn alarm_handshake_err_rta_and_im0_read_replay_byte_exact() {
    let setup = pnet_setup();
    let eth = MockTransport::new();
    let rpc = MockRpcTransport::new();
    let mut dev = Device::new(setup, eth, rpc);
    feed_bring_up(&dev, None);
    dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    assert_eq!(dev.state(), ArState::Data);

    // I&M0 read on the DAP -> p-net's exact response blocks (RPC header included).
    let cpu = "172.16.2.100:54766".parse().unwrap();
    dev.rpc()
        .push_rx(golden_alarm("im0_read_req")[RPC_OFF..].to_vec(), cpu);
    dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    let last = dev.rpc().sent().last().unwrap().0.clone();
    assert_eq!(
        last[80 + 20..],
        golden_alarm("im0_read_res")[RPC_OFF + 80 + 20..]
    );

    // Diagnosis raise -> notification. Slot 1's idents (0x30/0x130) come from
    // `DeviceModel::pnet_sample`.
    dev.diag_shared()
        .queue
        .lock()
        .unwrap()
        .push_back(DiagCommand::Raise(Diagnosis {
            slot: Slot(1),
            channel: 4,
            error: ChannelError::ShortCircuit,
            severity: Severity::Fault,
            direction: Direction::Input,
        }));
    dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    let notif = dev.eth().sent().last().unwrap().clone();
    let pdu = parse_frame(&notif).unwrap();
    assert_eq!((pdu.header.send_seq, pdu.header.ack_seq), (0xFFFF, 0xFFFE));
    let RtaBody::Data(RtaData::Notification(n)) = pdu.body else {
        panic!("expected an AlarmNotification, got {:?}", pdu.body)
    };
    assert_eq!(
        (
            n.alarm_type,
            n.slot,
            n.subslot,
            n.module_ident,
            n.submodule_ident,
            n.usi
        ),
        (AlarmType::Diagnosis, 1, 1, 0x30, 0x130, 0x8000)
    );

    // CPU's ack-rta + alarm-ack -> our ack-rta golden, byte-exact.
    dev.eth().push_rx(cpu_alarm("alarm_ack_rta_low_cpu"));
    dev.eth().push_rx(cpu_alarm("alarm_diag_ack_cpu"));
    dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    assert_eq!(
        dev.eth().sent().last().unwrap(),
        &dev_alarm("alarm_ack_rta_low_dev")
    );
    assert!(dev.problem_indicator());
    assert_eq!(dev.diag_shared().acked.load(Ordering::Relaxed), 1);

    // Clear -> DiagnosisDisappears, problem indicator clears.
    dev.diag_shared()
        .queue
        .lock()
        .unwrap()
        .push_back(DiagCommand::Clear {
            slot: Slot(1),
            channel: 4,
            error: ChannelError::ShortCircuit,
        });
    dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    let RtaBody::Data(RtaData::Notification(n)) =
        parse_frame(dev.eth().sent().last().unwrap()).unwrap().body
    else {
        panic!("expected a DiagnosisDisappears notification")
    };
    assert_eq!(n.alarm_type, AlarmType::DiagnosisDisappears);
    assert!(!dev.problem_indicator());

    // Controller ERR-RTA -> Idle, no reply on the wire.
    let before = dev.eth().sent().len();
    dev.eth().push_rx(cpu_alarm("alarm_err_rta_cpu_removed"));
    dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    assert_eq!(dev.state(), ArState::Idle);
    assert_eq!(dev.eth().sent().len(), before);

    // Reconnect (fresh RPC seq_nums so `Cm`'s response cache does not answer with the
    // first bring-up's cached Connect response) -> Data.
    feed_bring_up(&dev, Some(0x100));
    dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    assert_eq!(dev.state(), ArState::Data);

    // Stop from Data -> ERR-RTA AR removed.
    dev.shutdown(Instant::now()).unwrap();
    let err = parse_frame(dev.eth().sent().last().unwrap()).unwrap();
    assert_eq!(
        err.body,
        RtaBody::Err(PnioStatus::rta_abort(PnioStatus::RTA_ABORT_AR_REMOVED))
    );
}

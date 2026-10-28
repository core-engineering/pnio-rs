#![cfg(target_os = "linux")] // AF_PACKET, network namespaces
//! End-to-end over real sockets: `IoDevice::start` on one end of a veth pair, a scripted
//! controller on the other end, in its own network namespace.
//!
//! Unlike the other replay tests (mock transports), this exercises what only a kernel
//! does: the `AF_PACKET` sockets and their BPF filters, the multicast memberships, the
//! VLAN tag the kernel strips on receive, the UDP socket, the ARP-less IP path, the
//! interface MAC read from sysfs.
//!
//! The pair is built by `scripts/veth-e2e.sh`, which runs these tests inside a throwaway
//! user + network namespace (no root, nothing left on the host):
//!
//! ```text
//! [ test process: IoDevice ]  veth-dev 172.16.2.10   8c:f3:19:cd:19:f8
//!               |
//! [ netns pnio-cpu: Cpu ]     veth-cpu (no IP)       ec:1c:5d:61:e7:3f
//! ```
//!
//! The controller side has no IP stack configuration: it reads and writes whole frames
//! (DCP, LLDP, RTC1, and IPv4/UDP built by hand for DCE-RPC).
mod common;
use common::{golden, synthetic_connect_req, RPC_OFF};
use pnio::api::{IoDevice, RtOptions, StartOptions};
use pnio::config::{DeviceConfig, Slot};
use pnio::data::FieldType::*;
use pnio::dcp::{
    parse_blocks, write_blocks, DcpBlock, DcpHeader, ServiceId, ServiceType, DCP_MULTICAST_MAC,
};
use pnio::eth::{AfPacketTransport, EthHeader, EthProto, EthTransport, MacAddr, MAX_FRAME_LEN};
use pnio::lldp::{build_device_pdu, parse_frame};
use pnio::rpc::{NdrResponse, PacketType, RpcHeader};
use pnio::rt::{DataStatus, IoObject, Layout, RtFrame};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DEV_IF: &str = "veth-dev";
const CPU_IF: &str = "veth-cpu";
const CPU_NETNS: &str = "/run/netns/pnio-cpu";
/// The bench MACs: the controller's is the initiator MAC in the golden Connect request.
const DEV: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);
const CPU: MacAddr = MacAddr([0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f]);
const DEV_IP: [u8; 4] = [172, 16, 2, 10];
const CPU_IP: [u8; 4] = [172, 16, 2, 100];
/// The controller's client port for the calls it places (Connect, Write, PrmEnd).
const CPU_CLIENT_PORT: u16 = 54766;
const PNIO_PORT: u16 = 34964;
const ETHERTYPE_IPV4: u16 = 0x0800;
const WAIT: Duration = Duration::from_secs(3);

/// The tests share the interfaces and UDP port 34964: one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

/// The scripted controller: three `AF_PACKET` sockets on `veth-cpu`, opened from inside
/// the `pnio-cpu` namespace (a socket stays in the namespace it was created in).
struct Cpu {
    pn: AfPacketTransport,
    lldp: AfPacketTransport,
    ip: AfPacketTransport,
}

impl Cpu {
    fn open() -> Cpu {
        std::thread::spawn(|| {
            let ns = File::open(CPU_NETNS)
                .unwrap_or_else(|e| panic!("{CPU_NETNS}: {e} (run scripts/veth-e2e.sh)"));
            // Safety: `ns` is an open namespace file for the duration of the call;
            // `setns` only moves this short-lived thread.
            let r = unsafe { libc::setns(ns.as_raw_fd(), libc::CLONE_NEWNET) };
            assert_eq!(r, 0, "setns: {}", std::io::Error::last_os_error());
            let open = |p| AfPacketTransport::open_with(CPU_IF, p).expect("open veth-cpu");
            Cpu {
                pn: open(EthProto::PROFINET),
                lldp: open(EthProto::LLDP),
                ip: open(EthProto {
                    ethertype: ETHERTYPE_IPV4,
                    multicast: None,
                }),
            }
        })
        .join()
        .unwrap()
    }

    fn send(&self, frame: &[u8]) {
        self.pn.send(frame).expect("send on veth-cpu");
    }

    /// The first frame from the device that `pick` accepts, skipping the others.
    fn expect<T>(&self, what: &str, mut pick: impl FnMut(&[u8]) -> Option<T>) -> T {
        let t0 = Instant::now();
        let mut buf = vec![0u8; MAX_FRAME_LEN];
        loop {
            assert!(
                t0.elapsed() < WAIT,
                "no {what} from the device within {WAIT:?}"
            );
            for t in [&self.pn, &self.lldp, &self.ip] {
                let Some(n) = t
                    .recv_into(&mut buf, Some(Duration::from_millis(1)))
                    .unwrap()
                else {
                    continue;
                };
                let f = &buf[..n];
                if f[6..12] == DEV.0 {
                    if let Some(v) = pick(f) {
                        return v;
                    }
                }
            }
        }
    }

    /// An IPv4/UDP datagram from the controller's address (no UDP checksum).
    fn send_udp(&self, src_port: u16, dst_port: u16, payload: &[u8]) {
        let mut f = Vec::new();
        EthHeader {
            dst: DEV,
            src: CPU,
            vlan: None,
            ethertype: ETHERTYPE_IPV4,
        }
        .write(&mut f);
        let total = (20 + 8 + payload.len()) as u16;
        let mut ip = vec![0x45, 0];
        ip.extend_from_slice(&total.to_be_bytes());
        ip.extend_from_slice(&[0, 0, 0x40, 0, 64, 17, 0, 0]);
        ip.extend_from_slice(&CPU_IP);
        ip.extend_from_slice(&DEV_IP);
        let sum = ip_checksum(&ip);
        ip[10..12].copy_from_slice(&sum.to_be_bytes());
        f.extend_from_slice(&ip);
        f.extend_from_slice(&src_port.to_be_bytes());
        f.extend_from_slice(&dst_port.to_be_bytes());
        f.extend_from_slice(&(8 + payload.len() as u16).to_be_bytes());
        f.extend_from_slice(&[0, 0]);
        f.extend_from_slice(payload);
        self.send(&f);
    }

    /// A DCE-RPC call to the device: the response PDU, checked to be a Response.
    fn call(&self, what: &str, pdu: &[u8]) -> Vec<u8> {
        self.send_udp(CPU_CLIENT_PORT, PNIO_PORT, pdu);
        let res = self.expect(what, |f| {
            udp(f)
                .filter(|u| u.src_port == PNIO_PORT && u.dst_port == CPU_CLIENT_PORT)
                .map(|u| u.payload.to_vec())
        });
        let h = RpcHeader::parse(&res).unwrap();
        assert_eq!(h.ptype, PacketType::Response, "{what}");
        res
    }

    /// A DCP request; the blocks of the device's response with the same `xid`.
    fn dcp(
        &self,
        dst: MacAddr,
        frame_id: u16,
        service: ServiceId,
        xid: u32,
        blocks: &[DcpBlock],
    ) -> Vec<DcpBlock> {
        let mut body = Vec::new();
        write_blocks(blocks, &mut body);
        let mut f = Vec::new();
        EthHeader {
            dst,
            src: CPU,
            vlan: None,
            ethertype: 0x8892,
        }
        .write(&mut f);
        f.extend_from_slice(&frame_id.to_be_bytes());
        DcpHeader {
            service_id: service,
            service_type: ServiceType::Request,
            xid,
            response_delay: if service == ServiceId::Identify { 1 } else { 0 },
            data_length: body.len() as u16,
        }
        .write(&mut f);
        f.extend_from_slice(&body);
        f.resize(f.len().max(60), 0);
        self.send(&f);
        self.expect("DCP response", |f| {
            let (eth, off) = EthHeader::parse(f).ok()?;
            if eth.ethertype != 0x8892 || ![0xfeff, 0xfefd].contains(&be16(&f[off..])) {
                return None;
            }
            let (h, blocks) = DcpHeader::parse(&f[off + 2..]).ok()?;
            if h.xid != xid || h.service_type != ServiceType::ResponseSuccess {
                return None;
            }
            // Identify responses carry a BlockInfo per block; the Set response's single
            // Control/Response block does not.
            Some(parse_blocks(blocks, service == ServiceId::Identify).unwrap())
        })
    }

    /// The next LLDPDU from the device that `pick` accepts.
    fn lldpdu(&self, what: &str, pick: impl Fn(&Lldp) -> bool) -> Lldp {
        self.expect(what, |f| {
            let (_, pdu) = parse_frame(f).ok()?;
            let l = Lldp {
                chassis: pdu.chassis_id.as_text(),
                port: pdu.port_id.as_text(),
                ttl: pdu.ttl,
            };
            pick(&l).then_some(l)
        })
    }
}

#[derive(Debug, PartialEq)]
struct Lldp {
    chassis: String,
    port: String,
    ttl: u16,
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn ip_checksum(hdr: &[u8]) -> u16 {
    let mut sum: u32 = hdr.chunks(2).map(|w| be16(w) as u32).sum();
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

struct Udp<'a> {
    src_port: u16,
    dst_port: u16,
    payload: &'a [u8],
}

/// The UDP datagram in an untagged IPv4 frame.
fn udp(f: &[u8]) -> Option<Udp<'_>> {
    if f.len() < 42 || be16(&f[12..]) != ETHERTYPE_IPV4 || f[23] != 17 {
        return None;
    }
    let u = 14 + (f[14] & 0x0f) as usize * 4;
    let len = be16(&f[u + 4..]) as usize;
    Some(Udp {
        src_port: be16(&f[u..]),
        dst_port: be16(&f[u + 2..]),
        payload: f.get(u + 8..u + len)?,
    })
}

/// The FrameID and the bytes after it, VLAN tag or not (the kernel strips it on receive).
fn pn_payload(f: &[u8]) -> Option<(u16, &[u8])> {
    let (eth, off) = EthHeader::parse(f).ok()?;
    (eth.ethertype == 0x8892 && f.len() >= off + 2).then(|| (be16(&f[off..]), &f[off + 2..]))
}

fn data_off(objects: &[IoObject], slot: u16) -> usize {
    objects.iter().find(|o| o.slot == slot).unwrap().data_off
}

fn sample() -> DeviceConfig {
    DeviceConfig::builder("pnio-dev")
        .input(Slot(1), &[Real; 16])
        .input(Slot(2), &[Bool; 32])
        .output(Slot(3), &[Real; 16])
        .output(Slot(4), &[Bool; 32])
        .build()
        .unwrap()
}

fn start(cfg: DeviceConfig, on_signal: Option<Arc<dyn Fn() + Send + Sync>>) -> IoDevice {
    let mac = std::fs::read_to_string(format!("/sys/class/net/{DEV_IF}/address"))
        .unwrap_or_else(|e| panic!("{DEV_IF}: {e} (run scripts/veth-e2e.sh)"));
    let want: Vec<String> = DEV.0.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(mac.trim(), want.join(":"), "{DEV_IF} MAC");
    IoDevice::start(
        cfg,
        StartOptions {
            iface: DEV_IF.into(),
            ip: DEV_IP,
            rt: Some(RtOptions {
                iface: DEV_IF.into(),
                cpu_pin: None,
                rt_priority: None,
                lock_memory: false,
            }),
            app_cpus: None,
            im_store: None,
            net_store: None,
            apply_ip: false,
            lldp: true,
            on_signal,
        },
    )
    .expect("IoDevice::start on veth-dev")
}

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !ok() {
        assert!(t0.elapsed() < WAIT, "{what} not reached within {WAIT:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
#[ignore = "needs the veth pair: run scripts/veth-e2e.sh"]
fn identify_ar_and_cyclic_exchange_over_a_veth_pair() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let cpu = Cpu::open();
    let cfg = sample();
    let model = cfg.model(DEV);
    let dev = start(cfg, None);

    // DCP Identify All, multicast: the device answers with its name and IP.
    let blocks = cpu.dcp(
        DCP_MULTICAST_MAC,
        0xfefe,
        ServiceId::Identify,
        0x1001,
        &[DcpBlock {
            option: 0xff,
            suboption: 0xff,
            block_info: None,
            value: vec![],
        }],
    );
    let name = blocks.iter().find(|b| (b.option, b.suboption) == (2, 2));
    assert_eq!(name.map(|b| b.value.as_slice()), Some(&b"pnio-dev"[..]));
    let ip = blocks.iter().find(|b| (b.option, b.suboption) == (1, 2));
    assert_eq!(ip.map(|b| &b.value[..4]), Some(&DEV_IP[..]));

    // Connect, Write, PrmEnd over UDP; then the device calls ApplicationReady.
    for (what, pdu) in [
        ("Connect", synthetic_connect_req(&model)),
        ("Write", golden("write_req")[RPC_OFF..].to_vec()),
        ("PrmEnd", golden("prmend_req")[RPC_OFF..].to_vec()),
    ] {
        let res = cpu.call(what, &pdu);
        let h = RpcHeader::parse(&res).unwrap();
        let (ndr, _) = NdrResponse::parse(&res[RpcHeader::LEN..], h.drep).unwrap();
        assert_eq!(ndr.status, 0, "{what} PNIO status");
    }
    let (dev_port, app_ready) = cpu.expect("ApplicationReady request", |f| {
        udp(f)
            .filter(|u| u.dst_port == PNIO_PORT)
            .map(|u| (u.src_port, u.payload.to_vec()))
    });
    let req = RpcHeader::parse(&app_ready).unwrap();
    assert_eq!(req.ptype, PacketType::Request);
    let mut res = golden("appready_res")[RPC_OFF..].to_vec();
    let mut h = RpcHeader::parse(&res).unwrap();
    h.activity = req.activity;
    h.seq_num = req.seq_num;
    let mut hdr = Vec::new();
    h.write(&mut hdr);
    res[..RpcHeader::LEN].copy_from_slice(&hdr);
    cpu.send_udp(PNIO_PORT, dev_port, &res);
    wait_for("AR ready", || dev.ready());

    // Cyclic exchange, both ways. Our inputs: REAL 1.0 at slot 1 index 0, bit 31 of
    // slot 2. The controller's outputs: REAL -2.5 at slot 3 index 15, bit 31 of slot 4.
    dev.with_inputs(Slot(1), |w| w.real(0, 1.0)).unwrap();
    dev.with_inputs(Slot(2), |w| w.bool(31, true)).unwrap();
    let params = dev.ar_params().unwrap();
    let layout = Layout::from_ar(&params, &model).unwrap();
    let mut csdu = vec![0u8; layout.output_cr.data_length];
    for o in &layout.output_cr.objects {
        csdu[o.iops_off] = 0x80;
    }
    for c in &layout.output_cr.iocs {
        csdu[c.iocs_off] = 0x80;
    }
    let s3 = data_off(&layout.output_cr.objects, 3);
    csdu[s3 + 60..s3 + 64].copy_from_slice(&(-2.5f32).to_be_bytes());
    csdu[data_off(&layout.output_cr.objects, 4) + 3] = 0x80;

    // A real controller sends every cycle (1 ms here): keep the device's watchdog fed
    // until it has stopped. The guard also stops the feeder when an assertion fails, or
    // the scope would wait for it forever.
    struct StopOnDrop<'a>(&'a AtomicBool);
    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Relaxed);
        }
    }
    let feeding = AtomicBool::new(true);
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut buf = vec![0u8; MAX_FRAME_LEN];
            let mut cycle: u16 = 0;
            while feeding.load(Ordering::Relaxed) {
                let n = RtFrame {
                    frame_id: 0x8001,
                    cycle_counter: cycle,
                    data_status: DataStatus(0x35),
                    transfer_status: 0,
                    csdu: &csdu,
                }
                .write(&mut buf, DEV, CPU)
                .unwrap();
                cpu.send(&buf[..n]);
                cycle = cycle.wrapping_add(32);
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let _stop = StopOnDrop(&feeding);
        exchange(&cpu, dev, &layout);
    });
}

/// With the controller feeding its outputs: check both directions, then stop.
fn exchange(cpu: &Cpu, dev: IoDevice, layout: &Layout) {
    wait_for("controller outputs in the image", || {
        dev.read_real(Slot(3), 15) == Ok(-2.5)
    });
    assert!(dev.read_bool(Slot(4), 31).unwrap());
    assert!(!dev.read_bool(Slot(4), 30).unwrap());

    let s1 = data_off(&layout.input_cr.objects, 1);
    let s2 = data_off(&layout.input_cr.objects, 2);
    cpu.expect("Input CR frame carrying our inputs", |f| {
        let (id, rest) = pn_payload(f)?;
        (id == 0x8000 && rest[s1..s1 + 4] == 1.0f32.to_be_bytes() && rest[s2 + 3] == 0x80)
            .then_some(())
    });

    // Stop: TTL-0 LLDPDU and ERR-RTA "AR removed" (alarm FrameID 0xFC01/0xFE01, PDU
    // type 4) before the controller's own watchdog would notice.
    dev.stop().unwrap();
    cpu.expect("ERR-RTA after stop", |f| {
        let (id, rta) = pn_payload(f)?;
        ([0xfc01, 0xfe01].contains(&id) && rta.len() > 4 && rta[4] & 0x0f == 4).then_some(())
    });
}

#[test]
#[ignore = "needs the veth pair: run scripts/veth-e2e.sh"]
fn lldp_neighbour_and_dcp_set_over_a_veth_pair() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let cpu = Cpu::open();
    let flashes = Arc::new(AtomicU32::new(0));
    let on_signal: Arc<dyn Fn() + Send + Sync> = {
        let flashes = flashes.clone();
        Arc::new(move || {
            flashes.fetch_add(1, Ordering::Relaxed);
        })
    };
    let dev = start(sample(), Some(on_signal));

    // LLDP at start, then the controller's PDU makes it the device's neighbour.
    let first = cpu.lldpdu("LLDPDU at start", |_| true);
    assert_eq!(
        (first.chassis.as_str(), first.port.as_str(), first.ttl),
        ("pnio-dev", "port-001.pnio-dev", 20)
    );
    cpu.send(&build_device_pdu("plc", CPU.0, "port-001.plc", 20).frame(CPU));
    wait_for("LLDP neighbour", || dev.peer().is_some());
    let peer = dev.peer().unwrap();
    assert_eq!(
        (peer.chassis_id.as_str(), peer.port_id.as_str(), peer.mac),
        ("plc", "port-001.plc", CPU.0)
    );

    // DCP Set NameOfStation (temporary), unicast: accepted, the identity follows and a
    // fresh LLDPDU announces the new name.
    let set = |xid, option, suboption, value: &[u8]| {
        cpu.dcp(
            DEV,
            0xfefd,
            ServiceId::Set,
            xid,
            &[DcpBlock {
                option,
                suboption,
                block_info: Some(0),
                value: value.to_vec(),
            }],
        )
    };
    let res = set(0x2001, 2, 2, b"renamed");
    assert_eq!(res[0].value, [2, 2, 0], "Set NameOfStation response");
    wait_for("new name", || dev.net_params().name == "renamed");
    let renamed = cpu.lldpdu("LLDPDU with the new name", |l| l.chassis == "renamed");
    assert_eq!(
        (renamed.port.as_str(), renamed.ttl),
        ("port-001.renamed", 20)
    );

    // DCP Signal (Flash Once): accepted, counted, callback run.
    let res = set(0x2002, 5, 3, &[0x01, 0x00]);
    assert_eq!(res[0].value, [5, 3, 0], "Signal response");
    wait_for("signal", || dev.signal_count() == 1);
    assert_eq!(flashes.load(Ordering::Relaxed), 1);

    // Stop: the shutdown LLDPDU (TTL 0) withdraws the device from the neighbour tables.
    dev.stop().unwrap();
    cpu.lldpdu("shutdown LLDPDU (TTL 0)", |l| l.ttl == 0);
}

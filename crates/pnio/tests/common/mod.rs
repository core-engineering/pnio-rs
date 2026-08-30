#![allow(dead_code)]
//! Test-only helpers: golden frame loading from `testdata/cm/*.hex`.
//!
//! Duplicated from `src/testutil.rs` because integration tests cannot see
//! `crate::testutil` (it is a private, `#[cfg(test)]`-only module of the library crate).

/// Offset of the DCE-RPC PDU inside an untagged IPv4/UDP Ethernet frame (14 + 20 + 8).
pub const RPC_OFF: usize = 42;
/// Offset of the PROFINET FrameID inside a VLAN-tagged Ethernet frame (14 + 4).
pub const VLAN_PAYLOAD_OFF: usize = 18;

/// Parse a hex dump: `#` starts a comment to end of line, whitespace separates bytes.
pub fn parse_hex(text: &str) -> Vec<u8> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .flat_map(|l| {
            l.split_whitespace()
                .map(|b| u8::from_str_radix(b, 16).expect("hex byte"))
        })
        .collect::<Vec<u8>>()
}

/// Load `testdata/cm/<name>.hex` relative to the crate root.
pub fn golden(name: &str) -> Vec<u8> {
    let path = format!("{}/testdata/cm/{name}.hex", env!("CARGO_MANIFEST_DIR"));
    parse_hex(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")))
}

/// Offsets inside a VLAN-tagged RTC1 golden frame.
pub const RT_FRAMEID_OFF: usize = 18;
pub const RT_CSDU_OFF: usize = 20;
pub const RT_APDU_OFF: usize = 60;

/// Load `testdata/rt/<name>.hex` relative to the crate root.
pub fn golden_rt(name: &str) -> Vec<u8> {
    let path = format!("{}/testdata/rt/{name}.hex", env!("CARGO_MANIFEST_DIR"));
    parse_hex(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")))
}

/// Load `testdata/alarm/<name>.hex` (2026-08-30 p-net alarm/I&M capture) relative to the crate root.
pub fn golden_alarm(name: &str) -> Vec<u8> {
    let path = format!("{}/testdata/alarm/{name}.hex", env!("CARGO_MANIFEST_DIR"));
    parse_hex(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")))
}

/// Offset of the first PNIO block inside a Connect request PDU (RPC header 80 + NDR 20).
///
/// Duplicated from `src/testutil.rs` because integration tests cannot see
/// `crate::testutil` (it is a private, `#[cfg(test)]`-only module of the library crate).
pub const SYNTH_BLOCKS_OFF: usize = pnio::rpc::RpcHeader::LEN + pnio::rpc::NdrRequest::LEN;

/// Build a complete DCE-RPC Connect request PDU (header + NDR + blocks) for `model`,
/// the way the S7-1500 does it on the bench: ARBlockReq (advanced startup, activity
/// timeout 200, station "plcxbbench.profinetxainterfacexb25fbd"), Input CR
/// (FrameID 0x8000) and Output CR (FrameID 0x8001) with the §6b object order (DAP IOxS
/// first, then per slot data + IOxS, IOCS of the opposite direction last), one
/// ExpectedSubmoduleBlockReq per slot, AlarmCRBlockReq. The RPC header is the golden
/// `connect_req` one with `frag_len` recomputed; `cm` checks only the interface UUID.
pub fn synthetic_connect_req(model: &pnio::cm::DeviceModel) -> Vec<u8> {
    use pnio::cm::block::{ty, BlockHeader};
    use pnio::rpc::{Drep, NdrRequest, RpcHeader};

    fn block(out: &mut Vec<u8>, block_type: u16, body: &[u8]) {
        BlockHeader::write(out, block_type, body.len() as u16);
        out.extend_from_slice(body);
    }
    fn u16(b: &mut Vec<u8>, v: u16) {
        b.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(b: &mut Vec<u8>, v: u32) {
        b.extend_from_slice(&v.to_be_bytes());
    }

    let golden = golden("connect_req");
    let mut hdr = RpcHeader::parse(&golden[RPC_OFF..]).unwrap();
    let ar = pnio::cm::ArBlockReq::parse(&golden[RPC_OFF + SYNTH_BLOCKS_OFF + 6..]).unwrap();

    // --- ARBlockReq: same values as the bench CPU, station name included ---
    let mut body = Vec::new();
    u16(&mut body, 1);
    ar.ar_uuid.write(&mut body, Drep::BIG);
    u16(&mut body, ar.session_key);
    body.extend_from_slice(&ar.initiator_mac.0);
    ar.initiator_object_uuid.write(&mut body, Drep::BIG);
    u32(&mut body, ar.ar_properties);
    u16(&mut body, ar.activity_timeout_factor);
    u16(&mut body, 0x8892);
    u16(&mut body, ar.station_name.len() as u16);
    body.extend_from_slice(ar.station_name.as_bytes());
    let mut blocks = Vec::new();
    block(&mut blocks, ty::AR_BLOCK_REQ, &body);

    // --- IOCR objects in §6b order ---
    struct Obj {
        slot: u16,
        subslot: u16,
        off: u16,
    }
    let mut in_data = Vec::new(); // objects we produce (Input CR): DAP IOPS, then inputs
    let mut in_iocs = Vec::new(); // our IOCS for the outputs we consume
    let mut out_data = Vec::new(); // Output CR: outputs the CPU produces
    let mut out_iocs = Vec::new(); // CPU's IOCS for our inputs
    let mut in_off: u16 = 0;
    let mut out_off: u16 = 0;
    for s in &model.slots {
        for sm in &s.submodules {
            let is_dap = s.slot == 0;
            let has_in = sm.input_len > 0 || is_dap;
            let has_out = sm.output_len > 0;
            if has_in {
                in_data.push(Obj {
                    slot: s.slot,
                    subslot: sm.subslot,
                    off: in_off,
                });
                in_off += sm.input_len + 1;
                out_iocs.push(Obj {
                    slot: s.slot,
                    subslot: sm.subslot,
                    off: out_off,
                });
                out_off += 1;
            }
            if has_out {
                out_data.push(Obj {
                    slot: s.slot,
                    subslot: sm.subslot,
                    off: out_off,
                });
                out_off += sm.output_len + 1;
                in_iocs.push(Obj {
                    slot: s.slot,
                    subslot: sm.subslot,
                    off: in_off,
                });
                in_off += 1;
            }
        }
    }
    let iocr =
        |iocr_type: u16, reference: u16, frame_id: u16, len: u16, data: &[Obj], iocs: &[Obj]| {
            let mut b = Vec::new();
            u16(&mut b, iocr_type);
            u16(&mut b, reference);
            u16(&mut b, 0x8892); // LT
            u32(&mut b, 0x0000_0002); // IOCRProperties: RTClass 2 (what the CPU sends)
            u16(&mut b, len.max(40));
            u16(&mut b, frame_id);
            u16(&mut b, 32); // send clock factor
            u16(&mut b, 1); // reduction ratio (1 ms)
            u16(&mut b, 1); // phase
            u16(&mut b, 0); // sequence
            u32(&mut b, 0xffff_ffff); // frame send offset
            u16(&mut b, 3); // watchdog factor
            u16(&mut b, 3); // data hold factor
            u16(&mut b, 0xc000); // tag header
            b.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // multicast MAC
            u16(&mut b, 1); // number of APIs
            u32(&mut b, 0);
            u16(&mut b, data.len() as u16);
            for o in data {
                u16(&mut b, o.slot);
                u16(&mut b, o.subslot);
                u16(&mut b, o.off);
            }
            u16(&mut b, iocs.len() as u16);
            for o in iocs {
                u16(&mut b, o.slot);
                u16(&mut b, o.subslot);
                u16(&mut b, o.off);
            }
            b
        };
    block(
        &mut blocks,
        ty::IOCR_BLOCK_REQ,
        &iocr(1, 1, 0x8000, in_off, &in_data, &in_iocs),
    );
    block(
        &mut blocks,
        ty::IOCR_BLOCK_REQ,
        &iocr(2, 2, 0x8001, out_off, &out_data, &out_iocs),
    );

    // --- one ExpectedSubmoduleBlockReq per slot ---
    for s in &model.slots {
        let mut b = Vec::new();
        u16(&mut b, 1); // number of APIs
        u32(&mut b, 0);
        u16(&mut b, s.slot);
        u32(&mut b, s.module_ident);
        u16(&mut b, 0); // module properties
        u16(&mut b, s.submodules.len() as u16);
        for sm in &s.submodules {
            u16(&mut b, sm.subslot);
            u32(&mut b, sm.submodule_ident);
            let props: u16 = match (sm.input_len > 0, sm.output_len > 0) {
                (false, false) => 0,
                (true, false) => 1,
                (false, true) => 2,
                (true, true) => 3,
            };
            u16(&mut b, props);
            let desc = |b: &mut Vec<u8>, tag: u16, len: u16| {
                u16(b, tag);
                u16(b, len);
                b.push(1);
                b.push(1);
            };
            match props {
                0 | 1 => desc(&mut b, 1, sm.input_len),
                2 => desc(&mut b, 2, sm.output_len),
                _ => {
                    desc(&mut b, 1, sm.input_len);
                    desc(&mut b, 2, sm.output_len);
                }
            }
        }
        block(&mut blocks, ty::EXPECTED_SUBMODULE_BLOCK_REQ, &b);
    }

    // --- AlarmCRBlockReq (bench values) ---
    let mut b = Vec::new();
    u16(&mut b, 1);
    u16(&mut b, 0x8892);
    u32(&mut b, 0);
    u16(&mut b, 1);
    u16(&mut b, 3);
    u16(&mut b, 1);
    u16(&mut b, 200);
    u16(&mut b, 0xc000);
    u16(&mut b, 0xa000);
    block(&mut blocks, ty::ALARM_CR_BLOCK_REQ, &b);

    // --- PDU: header (frag_len = NDR + blocks) + NDR + blocks ---
    let ndr = NdrRequest::for_blocks(blocks.len() as u32 + 16, blocks.len() as u32);
    hdr.frag_len = (NdrRequest::LEN + blocks.len()) as u16;
    let mut pdu = Vec::new();
    hdr.write(&mut pdu);
    ndr.write(&mut pdu, hdr.drep);
    pdu.extend_from_slice(&blocks);
    pdu
}

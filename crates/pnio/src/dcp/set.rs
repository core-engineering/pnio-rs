//! DCP Set: parse the Set request (IP suite, NameOfStation, Signal, ResetToFactory),
//! decide each block, build the response.
//!
//! This module is the pure decision layer: [`decide_set`] validates each block and
//! returns the [`Effect`] to apply when it is accepted. Applying it (pushing the IP to
//! the interface, persisting the name, flashing the LED, resetting) belongs to
//! `device::Device`, which turns an application failure into
//! [`SetBlockError::SetNotPossible`].

use crate::dcp::block::{blocks_encoded_len, parse_blocks, write_blocks, DcpBlock};
use crate::dcp::frame::{DcpHeader, FrameId, ServiceId, ServiceType};
use crate::dcp::{validate_station_name, DcpError};
use crate::eth::{EthHeader, MacAddr, ETHERTYPE_PROFINET};

/// One block from a Set request, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetBlock {
    /// IP option (1), suboption 2 (IPParameter): the qualifier and requested triple.
    IpSuite {
        /// `BlockQualifier`: whether the change should be temporary or persist across reboot.
        qualifier: u16,
        /// Requested IP address.
        ip: [u8; 4],
        /// Requested subnet mask.
        subnet: [u8; 4],
        /// Requested default gateway.
        gateway: [u8; 4],
    },
    /// DeviceProperties option (2), suboption 2 (NameOfStation): the requested name.
    NameOfStation {
        /// `BlockQualifier`: bit 0 ([`QUALIFIER_PERMANENT`]) set = persist across restarts.
        qualifier: u16,
        /// Requested NameOfStation (validated by [`decide_set`], not by the parser).
        name: String,
    },
    /// Control option (5), suboption 3 (Signal): flash the identification LED.
    Signal {
        /// `BlockQualifier` (reserved for Signal; carried for completeness).
        qualifier: u16,
        /// `SignalValue`: `0x0100` = "Flash Once", the only value defined.
        value: u16,
    },
    /// Control option (5), suboption 6 (ResetToFactory): no value, the mode sits in
    /// the qualifier (see [`reset_mode`]).
    ResetToFactory {
        /// `BlockQualifier`: the reset mode shifted left by one (`mode << 1`).
        qualifier: u16,
    },
    /// Any other option/suboption we don't implement.
    Other {
        /// The block's `Option` byte.
        option: u8,
        /// The block's `Suboption` byte.
        suboption: u8,
    },
}

/// A parsed DCP Set request: the ordered list of blocks it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetRequest {
    /// The request's blocks, in request order.
    pub blocks: Vec<SetBlock>,
}

/// Per-block Set result, carried in the Control/Response block of the reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SetBlockError {
    /// The block was accepted (and, once `device::Device` applies the effect, applied).
    Ok = 0x00,
    /// The requested `Option` is not supported at all.
    OptionNotSupported = 0x01,
    /// The `Option` is known but this `Suboption` is not supported.
    SuboptionNotSupported = 0x02,
    /// The suboption is supported but has no value configured yet.
    SuboptionNotSet = 0x03,
    /// A resource needed to apply the change is unavailable.
    ResourceError = 0x04,
    /// The block was accepted but applying it failed (e.g. the interface refused the
    /// address or the name could not be persisted); set by `device::Device`, never by
    /// [`decide_set`].
    SetNotPossible = 0x05,
}

/// `BlockQualifier` bit 0: the change persists across restarts.
pub const QUALIFIER_PERMANENT: u16 = 0x0001;

/// The reset mode a ResetToFactory `BlockQualifier` carries: the mode sits above bit 0
/// (`docs/pdev-golden-frames.md` §1.2: mode = `BlockQualifier >> 1`; modes 1-9 are
/// defined, so bits 1-4 are kept). Mode 2 = `0x0004`, mode 4 = `0x0008`, mode 8 = `0x0010`.
pub fn reset_mode(qualifier: u16) -> u8 {
    ((qualifier >> 1) & 0xF) as u8
}

/// What `device::Device` must do for one accepted block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Adopt a new NameOfStation.
    SetName {
        /// The validated name.
        name: String,
        /// Persist across restarts (`BlockQualifier` bit 0).
        permanent: bool,
    },
    /// Adopt a new IP suite.
    SetIp {
        /// New IP address.
        ip: [u8; 4],
        /// New subnet mask.
        subnet: [u8; 4],
        /// New default gateway.
        gateway: [u8; 4],
        /// Persist across restarts (`BlockQualifier` bit 0).
        permanent: bool,
    },
    /// Flash the identification LED ("Flash Once").
    Signal,
    /// Reset to factory settings in the given mode (2 or 4, see [`reset_mode`]).
    Reset {
        /// The decoded reset mode.
        mode: u8,
    },
}

/// One block's outcome: the Control/Response tuple plus the effect to apply when `Ok`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetDecision {
    /// The request block's `Option` byte, echoed in the response.
    pub option: u8,
    /// The request block's `Suboption` byte, echoed in the response.
    pub suboption: u8,
    /// The `BlockError` to answer with.
    pub error: SetBlockError,
    /// The effect to apply; `Some` exactly when `error` is [`SetBlockError::Ok`].
    pub effect: Option<Effect>,
}

impl SetDecision {
    fn ok(option: u8, suboption: u8, effect: Effect) -> Self {
        SetDecision {
            option,
            suboption,
            error: SetBlockError::Ok,
            effect: Some(effect),
        }
    }

    fn err(option: u8, suboption: u8, error: SetBlockError) -> Self {
        SetDecision {
            option,
            suboption,
            error,
            effect: None,
        }
    }

    /// The `(option, suboption, error)` tuple [`build_set_response`] consumes.
    pub fn tuple(&self) -> (u8, u8, SetBlockError) {
        (self.option, self.suboption, self.error)
    }
}

/// Parse the block bytes of a Set request. Set request blocks carry a 2-byte
/// BlockQualifier where responses carry BlockInfo, so this reads with
/// `has_block_info = true` and treats `block_info` as the qualifier.
pub fn parse_set_request(block_bytes: &[u8]) -> Result<SetRequest, DcpError> {
    let blocks = parse_blocks(block_bytes, true)?;
    let mut out = Vec::with_capacity(blocks.len());
    for b in blocks {
        let qualifier = b
            .block_info
            .expect("parse_blocks(.., true) always yields Some block_info");
        match (b.option, b.suboption) {
            (1, 2) => {
                if b.value.len() != 12 {
                    return Err(DcpError::Malformed("IPParameter value must be 12 bytes"));
                }
                let mut ip = [0u8; 4];
                let mut subnet = [0u8; 4];
                let mut gateway = [0u8; 4];
                ip.copy_from_slice(&b.value[0..4]);
                subnet.copy_from_slice(&b.value[4..8]);
                gateway.copy_from_slice(&b.value[8..12]);
                out.push(SetBlock::IpSuite {
                    qualifier,
                    ip,
                    subnet,
                    gateway,
                });
            }
            (2, 2) => {
                // Lossy: a non-UTF-8 byte becomes U+FFFD, which `validate_station_name`
                // rejects, so the block is answered `SuboptionNotSet` instead of the
                // whole request being dropped.
                let name = String::from_utf8_lossy(&b.value).into_owned();
                out.push(SetBlock::NameOfStation { qualifier, name });
            }
            (5, 3) => {
                if b.value.len() != 2 {
                    return Err(DcpError::Malformed("Signal value must be 2 bytes"));
                }
                out.push(SetBlock::Signal {
                    qualifier,
                    value: u16::from_be_bytes([b.value[0], b.value[1]]),
                });
            }
            (5, 6) => out.push(SetBlock::ResetToFactory { qualifier }),
            (option, suboption) => out.push(SetBlock::Other { option, suboption }),
        }
    }
    Ok(SetRequest { blocks: out })
}

/// Decide each block of a Set request. Pure: validation only — the effects are applied
/// (and the IP pushed to the interface, the file written, the AR aborted) by
/// `device::Device`, which also turns an application failure into `SetNotPossible`.
///
/// NameOfStation failing [`validate_station_name`], an IP suite whose subnet mask is not
/// contiguous and a Signal other than "Flash Once" are refused with `SuboptionNotSet`; a
/// ResetToFactory mode other than 2 or 4 and any unimplemented option/suboption with
/// `SuboptionNotSupported`. Any other IP suite (including all-zero) is accepted.
pub fn decide_set(req: &SetRequest) -> Vec<SetDecision> {
    req.blocks
        .iter()
        .map(|b| match b {
            SetBlock::NameOfStation { qualifier, name } => match validate_station_name(name) {
                Ok(()) => SetDecision::ok(
                    2,
                    2,
                    Effect::SetName {
                        name: name.clone(),
                        permanent: qualifier & QUALIFIER_PERMANENT != 0,
                    },
                ),
                Err(e) => {
                    log::warn!("DCP Set NameOfStation {name:?} refused: {e}");
                    SetDecision::err(2, 2, SetBlockError::SuboptionNotSet)
                }
            },
            SetBlock::IpSuite { subnet, .. } if !is_contiguous_mask(*subnet) => {
                log::warn!("DCP Set IP refused: non-contiguous subnet mask {subnet:?}");
                SetDecision::err(1, 2, SetBlockError::SuboptionNotSet)
            }
            SetBlock::IpSuite {
                qualifier,
                ip,
                subnet,
                gateway,
            } => SetDecision::ok(
                1,
                2,
                Effect::SetIp {
                    ip: *ip,
                    subnet: *subnet,
                    gateway: *gateway,
                    permanent: qualifier & QUALIFIER_PERMANENT != 0,
                },
            ),
            SetBlock::Signal { value: 0x0100, .. } => SetDecision::ok(5, 3, Effect::Signal),
            SetBlock::Signal { .. } => SetDecision::err(5, 3, SetBlockError::SuboptionNotSet),
            SetBlock::ResetToFactory { qualifier } => match reset_mode(*qualifier) {
                mode @ (2 | 4) => SetDecision::ok(5, 6, Effect::Reset { mode }),
                _ => SetDecision::err(5, 6, SetBlockError::SuboptionNotSupported),
            },
            SetBlock::Other { option, suboption } => {
                SetDecision::err(*option, *suboption, SetBlockError::SuboptionNotSupported)
            }
        })
        .collect()
}

/// A subnet mask is well formed when its set bits are all leading (`/0` to `/32`).
fn is_contiguous_mask(subnet: [u8; 4]) -> bool {
    let m = u32::from_be_bytes(subnet);
    m.leading_ones() + m.trailing_zeros() == 32
}

/// Build the full Ethernet frame of a Set response (untagged, no BlockInfo on the
/// Control/Response blocks) carrying one result per input tuple.
pub fn build_set_response(
    dst: MacAddr,
    src: MacAddr,
    xid: u32,
    results: &[(u8, u8, SetBlockError)],
) -> Vec<u8> {
    let blocks: Vec<DcpBlock> = results
        .iter()
        .map(|(option, suboption, err)| DcpBlock {
            option: 5,
            suboption: 4,
            block_info: None,
            value: vec![*option, *suboption, *err as u8],
        })
        .collect();

    let header = DcpHeader {
        service_id: ServiceId::Set,
        service_type: ServiceType::ResponseSuccess,
        xid,
        response_delay: 0,
        data_length: blocks_encoded_len(&blocks),
    };

    let mut out = Vec::new();
    EthHeader {
        dst,
        src,
        vlan: None,
        ethertype: ETHERTYPE_PROFINET,
    }
    .write(&mut out);
    out.extend_from_slice(&FrameId::GetSet.to_u16().to_be_bytes());
    header.write(&mut out);
    write_blocks(&blocks, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dcp::{
        handle_dcp_frame, DcpBlock, DcpConfig, DcpHeader, DeviceProperties, NetParams,
    };
    use crate::eth::{EthHeader, MacAddr};
    use crate::testutil::{golden, VLAN_PAYLOAD_OFF};

    fn cfg(ip: [u8; 4]) -> DcpConfig {
        cfg_full(ip, [255, 255, 255, 0], ip)
    }

    fn cfg_full(ip: [u8; 4], subnet: [u8; 4], gateway: [u8; 4]) -> DcpConfig {
        DcpConfig {
            mac: MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]),
            properties: DeviceProperties {
                name_of_station: "rt-labs-dev".into(),
                type_of_station: "P-Net Sample Application".into(),
                vendor_id: 0x0493,
                device_id: 0x0002,
                device_role: 0x0100,
                device_instance: 1,
                device_options: vec![1, 2, 2, 2, 2, 3],
                ip,
                subnet,
                gateway,
                ip_block_info: 1,
            },
        }
    }

    #[test]
    fn parse_golden_set_ip_request() {
        let f = golden("dcp_set_req");
        // FrameID at 18 (VLAN), DCP header at 20, blocks after the 10-byte header
        let (h, blocks) = crate::dcp::DcpHeader::parse(&f[VLAN_PAYLOAD_OFF + 2..]).unwrap();
        assert_eq!(h.xid, 0x0300_012d);
        let req = parse_set_request(blocks).unwrap();
        assert_eq!(
            req.blocks,
            vec![SetBlock::IpSuite {
                qualifier: 0,
                ip: [172, 16, 2, 10],
                subnet: [255, 255, 255, 0],
                gateway: [172, 16, 2, 10]
            }]
        );
    }

    #[test]
    fn set_ok_response_is_byte_exact_via_dispatch() {
        let resp = handle_dcp_frame(&golden("dcp_set_req"), &cfg([172, 16, 2, 10]))
            .unwrap()
            .unwrap();
        assert_eq!(resp, golden("dcp_set_res"));
    }

    #[test]
    fn unsupported_option_gets_suboption_not_supported() {
        let req = SetRequest {
            blocks: vec![SetBlock::Other {
                option: 2,
                suboption: 2,
            }],
        };
        assert_eq!(
            decide_set(&req)
                .iter()
                .map(SetDecision::tuple)
                .collect::<Vec<_>>(),
            vec![(2, 2, SetBlockError::SuboptionNotSupported)]
        );
    }

    #[test]
    fn get_request_is_ignored() {
        let mut f = golden("dcp_set_req");
        f[VLAN_PAYLOAD_OFF + 2] = 3; // ServiceID Get
        assert_eq!(handle_dcp_frame(&f, &cfg([172, 16, 2, 10])).unwrap(), None);
    }

    /// The DCP blocks of a Set request frame, tagged or not: Ethernet header (14 or
    /// 18 bytes) + FrameID 2 + DCP header 10, sliced to `DCPDataLength` so the
    /// Ethernet minimum-size padding of a captured frame is not read as blocks.
    fn blocks_of(frame: &[u8]) -> &[u8] {
        let (_, off) = EthHeader::parse(frame).unwrap();
        let (_, blocks) = DcpHeader::parse(&frame[off + 2..]).unwrap();
        blocks
    }

    // synthetic — replace by the Task 2 golden (`golden_in("dcp", name)`).
    // docs/pdev-golden-frames.md §1: NameOfStation (2/2) qualifier 0x0001 = permanent;
    // Signal (5/3) qualifier 0 + SignalValue 0x0100 "Flash Once"; ResetToFactory (5/6)
    // BlockQualifier = mode << 1 (mode 2 → 0x0004, mode 4 → 0x0008), no value.
    fn tia_set(name: &str) -> Vec<u8> {
        let block = |option, suboption, qualifier, value: &[u8]| DcpBlock {
            option,
            suboption,
            block_info: Some(qualifier),
            value: value.to_vec(),
        };
        let b = match name {
            "set_name_req" => block(2, 2, 0x0001, b"pnio-dev-b"),
            "signal_req" => block(5, 3, 0x0000, &[0x01, 0x00]),
            "reset_keep_im_req" => block(5, 6, 0x0004, &[]),
            "reset_all_req" => block(5, 6, 0x0008, &[]),
            other => panic!("no synthetic DCP request {other}"),
        };
        crate::testutil::dcp_set_frame(
            MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]),
            MacAddr([0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f]),
            0x0300_0200,
            &[b],
        )
    }

    #[test]
    fn parses_set_name_request() {
        let req = parse_set_request(blocks_of(&tia_set("set_name_req"))).unwrap();
        assert_eq!(
            req.blocks,
            vec![SetBlock::NameOfStation {
                qualifier: QUALIFIER_PERMANENT,
                name: "pnio-dev-b".into()
            }]
        );
    }

    #[test]
    fn parses_signal_and_reset_requests() {
        let s = parse_set_request(blocks_of(&tia_set("signal_req"))).unwrap();
        assert!(matches!(
            s.blocks[0],
            SetBlock::Signal { value: 0x0100, .. }
        ));
        let keep = parse_set_request(blocks_of(&tia_set("reset_keep_im_req"))).unwrap();
        let all = parse_set_request(blocks_of(&tia_set("reset_all_req"))).unwrap();
        let SetBlock::ResetToFactory { qualifier: q_keep } = keep.blocks[0] else {
            panic!()
        };
        let SetBlock::ResetToFactory { qualifier: q_all } = all.blocks[0] else {
            panic!()
        };
        assert_eq!(reset_mode(q_keep), 2);
        assert_eq!(reset_mode(q_all), 4);
    }

    #[test]
    fn reset_mode_reads_bits_1_to_4() {
        assert_eq!(reset_mode(0x0004), 2);
        assert_eq!(reset_mode(0x0008), 4);
        assert_eq!(reset_mode(0x0010), 8); // mode 8 survives the decode (no truncation to 0)
        assert_eq!(reset_mode(0x0012), 9);
        assert_eq!(reset_mode(0x0005), 2); // bit 0 is not part of the mode
    }

    #[test]
    fn decides_every_block_kind() {
        let req = SetRequest {
            blocks: vec![
                SetBlock::NameOfStation {
                    qualifier: 1,
                    name: "ok-name".into(),
                },
                SetBlock::NameOfStation {
                    qualifier: 0,
                    name: "Bad".into(),
                },
                SetBlock::IpSuite {
                    qualifier: 0,
                    ip: [10, 0, 0, 2],
                    subnet: [255, 255, 0, 0],
                    gateway: [10, 0, 0, 1],
                },
                SetBlock::Signal {
                    qualifier: 0,
                    value: 0x0100,
                },
                SetBlock::Signal {
                    qualifier: 0,
                    value: 0x0200,
                },
                SetBlock::ResetToFactory { qualifier: 2 << 1 },
                SetBlock::ResetToFactory { qualifier: 8 << 1 },
                SetBlock::Other {
                    option: 2,
                    suboption: 9,
                },
            ],
        };
        let d = decide_set(&req);
        let tuples: Vec<_> = d.iter().map(SetDecision::tuple).collect();
        assert_eq!(
            tuples,
            vec![
                (2, 2, SetBlockError::Ok),
                (2, 2, SetBlockError::SuboptionNotSet),
                (1, 2, SetBlockError::Ok),
                (5, 3, SetBlockError::Ok),
                (5, 3, SetBlockError::SuboptionNotSet),
                (5, 6, SetBlockError::Ok),
                (5, 6, SetBlockError::SuboptionNotSupported),
                (2, 9, SetBlockError::SuboptionNotSupported),
            ]
        );
        let effects: Vec<_> = d.iter().map(|x| x.effect.clone()).collect();
        assert_eq!(
            effects,
            vec![
                Some(Effect::SetName {
                    name: "ok-name".into(),
                    permanent: true
                }),
                None,
                Some(Effect::SetIp {
                    ip: [10, 0, 0, 2],
                    subnet: [255, 255, 0, 0],
                    gateway: [10, 0, 0, 1],
                    permanent: false
                }),
                Some(Effect::Signal),
                None,
                Some(Effect::Reset { mode: 2 }),
                None,
                None,
            ]
        );
        assert_eq!(
            reset_mode(8 << 1),
            8,
            "refused because mode 8 is unsupported, not because it decoded to 0"
        );
    }

    /// A synthetic Set request carrying `blocks` (qualifier in `block_info`).
    fn set_frame(blocks: &[DcpBlock]) -> Vec<u8> {
        crate::testutil::dcp_set_frame(
            MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]),
            MacAddr([0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f]),
            0x0300_0201,
            blocks,
        )
    }

    #[test]
    fn non_utf8_name_is_refused_and_the_other_blocks_still_answered() {
        let f = set_frame(&[
            DcpBlock {
                option: 2,
                suboption: 2,
                block_info: Some(QUALIFIER_PERMANENT),
                value: vec![b'd', b'e', b'v', 0xff],
            },
            DcpBlock {
                option: 5,
                suboption: 3,
                block_info: Some(0),
                value: vec![0x01, 0x00],
            },
        ]);
        let req = parse_set_request(blocks_of(&f)).unwrap();
        let d = decide_set(&req);
        assert_eq!(
            d.iter().map(SetDecision::tuple).collect::<Vec<_>>(),
            vec![
                (2, 2, SetBlockError::SuboptionNotSet),
                (5, 3, SetBlockError::Ok)
            ]
        );
        assert_eq!(d[0].effect, None);
        assert_eq!(d[1].effect, Some(Effect::Signal));
        // the dispatch answers instead of dropping the request
        let resp = handle_dcp_frame(&f, &cfg([172, 16, 2, 10])).unwrap();
        assert!(resp.is_some());
    }

    #[test]
    fn non_contiguous_subnet_is_refused() {
        let suite = |subnet| SetBlock::IpSuite {
            qualifier: 0,
            ip: [10, 0, 0, 2],
            subnet,
            gateway: [10, 0, 0, 1],
        };
        let req = SetRequest {
            blocks: vec![
                suite([255, 0, 255, 0]),
                suite([255, 255, 255, 255]),
                suite([255, 255, 255, 128]),
            ],
        };
        let d = decide_set(&req);
        assert_eq!(d[0].tuple(), (1, 2, SetBlockError::SuboptionNotSet));
        assert_eq!(d[0].effect, None);
        assert_eq!(d[1].error, SetBlockError::Ok);
        assert_eq!(d[2].error, SetBlockError::Ok);
    }

    #[test]
    fn signal_with_one_byte_value_is_malformed() {
        let f = set_frame(&[DcpBlock {
            option: 5,
            suboption: 3,
            block_info: Some(0),
            value: vec![0x01],
        }]);
        assert_eq!(
            parse_set_request(blocks_of(&f)),
            Err(DcpError::Malformed("Signal value must be 2 bytes"))
        );
    }

    #[test]
    fn any_ip_suite_is_accepted_now() {
        let req = SetRequest {
            blocks: vec![SetBlock::IpSuite {
                qualifier: 1,
                ip: [0; 4],
                subnet: [0; 4],
                gateway: [0; 4],
            }],
        };
        assert_eq!(decide_set(&req)[0].error, SetBlockError::Ok);
    }

    #[test]
    fn net_params_helpers() {
        let p = NetParams {
            name: String::new(),
            ip: [0; 4],
            subnet: [0; 4],
            gateway: [0; 4],
        };
        assert!(p.is_blank());
        let q = NetParams {
            name: "x".into(),
            ip: [172, 16, 2, 10],
            subnet: [255, 255, 255, 0],
            gateway: [172, 16, 2, 10],
        };
        assert!(!q.is_blank());
        assert_eq!(q.prefix_len(), 24);
    }
}

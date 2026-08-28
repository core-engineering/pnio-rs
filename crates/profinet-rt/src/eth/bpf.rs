//! Classic BPF programs attached to the `AF_PACKET` sockets so each one wakes up only
//! for the frames it handles: the RT socket for RTC1 (`0x8000..=0xBFFF`), the acyclic
//! socket for alarms and DCP (`0xFC00..=0xFFFF`).
//!
//! The program accepts EtherType `0x8892` directly or behind an 802.1Q tag; the
//! kernel usually strips the tag before the filter runs, but a NIC without VLAN RX
//! offload would not, so both shapes are handled.
//!
//! Only the handful of opcodes we need are defined here, from the classic BPF
//! encoding (`BPF_CLASS | BPF_SIZE | BPF_MODE` for loads, `BPF_JMP | op | BPF_K` for
//! jumps): no `libc` constants exist for them.

/// One classic BPF instruction; same layout as the kernel's `struct sock_filter`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SockFilter {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

/// `A = half-word at [k]` (`BPF_LD | BPF_H | BPF_ABS`).
pub const LD_H_ABS: u16 = 0x28;
/// `A = half-word at [X + k]` (`BPF_LD | BPF_H | BPF_IND`).
pub const LD_H_IND: u16 = 0x48;
/// `X = k` (`BPF_LDX | BPF_W | BPF_IMM`).
pub const LDX_IMM: u16 = 0x01;
/// `pc += k` (`BPF_JMP | BPF_JA`).
pub const JA: u16 = 0x05;
/// `pc += (A == k) ? jt : jf` (`BPF_JMP | BPF_JEQ | BPF_K`).
pub const JEQ: u16 = 0x15;
/// `pc += (A >= k) ? jt : jf` (`BPF_JMP | BPF_JGE | BPF_K`).
pub const JGE: u16 = 0x35;
/// `pc += (A > k) ? jt : jf` (`BPF_JMP | BPF_JGT | BPF_K`).
pub const JGT: u16 = 0x25;
/// `return k` (`BPF_RET | BPF_K`): 0 drops, anything else accepts that many bytes.
pub const RET: u16 = 0x06;

const fn insn(code: u16, jt: u8, jf: u8, k: u32) -> SockFilter {
    SockFilter { code, jt, jf, k }
}

/// Accept PROFINET (`0x8892`) frames, untagged or 802.1Q-tagged, whose FrameID is in
/// `lo..=hi`; drop everything else.
///
/// ```text
///  0: ldh [12]                 ethertype
///  1: jeq 0x8892  → 2 else 4
///  2: ldx #14                  FrameID offset, untagged
///  3: ja  → 8
///  4: jeq 0x8100  → 5 else 11
///  5: ldh [16]                 inner ethertype
///  6: jeq 0x8892  → 7 else 11
///  7: ldx #18                  FrameID offset, tagged
///  8: ldh [x+0]                FrameID
///  9: jge lo      → 10 else 11
/// 10: jgt hi      → 11 else 12
/// 11: ret 0
/// 12: ret 0xFFFF
/// ```
pub fn frame_id_filter(lo: u16, hi: u16) -> Vec<SockFilter> {
    vec![
        insn(LD_H_ABS, 0, 0, 12),
        insn(JEQ, 0, 2, 0x8892),
        insn(LDX_IMM, 0, 0, 14),
        insn(JA, 0, 0, 4),
        insn(JEQ, 0, 6, 0x8100),
        insn(LD_H_ABS, 0, 0, 16),
        insn(JEQ, 0, 4, 0x8892),
        insn(LDX_IMM, 0, 0, 18),
        insn(LD_H_IND, 0, 0, 0),
        insn(JGE, 0, 1, u32::from(lo)),
        insn(JGT, 0, 1, u32::from(hi)),
        insn(RET, 0, 0, 0),
        insn(RET, 0, 0, 0xFFFF),
    ]
}

/// Filter for the RT socket: RTC1 frames only.
pub fn rt_filter() -> Vec<SockFilter> {
    frame_id_filter(0x8000, 0xBFFF)
}

/// Filter for the acyclic socket: alarms (`0xFC01`, `0xFE01`) and DCP (`0xFEFC..=0xFEFF`).
pub fn acyclic_filter() -> Vec<SockFilter> {
    frame_id_filter(0xFC00, 0xFFFF)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{golden, golden_rt};

    /// Just enough classic BPF to run our own programs: LD_H_ABS, LDX_IMM, JA, JEQ,
    /// JGE, JGT, LD_H_IND, RET. Returns the accepted length (0 = rejected).
    fn run(prog: &[SockFilter], pkt: &[u8]) -> u32 {
        let ldh = |off: usize| -> Option<u32> {
            pkt.get(off..off + 2)
                .map(|b| u32::from(u16::from_be_bytes([b[0], b[1]])))
        };
        let (mut a, mut x, mut pc) = (0u32, 0u32, 0usize);
        loop {
            let i = prog[pc];
            pc += 1;
            match i.code {
                LD_H_ABS => match ldh(i.k as usize) {
                    Some(v) => a = v,
                    None => return 0,
                },
                LD_H_IND => match ldh((x + i.k) as usize) {
                    Some(v) => a = v,
                    None => return 0,
                },
                LDX_IMM => x = i.k,
                JA => pc += i.k as usize,
                JEQ => pc += if a == i.k { i.jt } else { i.jf } as usize,
                JGE => pc += if a >= i.k { i.jt } else { i.jf } as usize,
                JGT => pc += if a > i.k { i.jt } else { i.jf } as usize,
                RET => return i.k,
                other => panic!("opcode {other:#x} not in the test interpreter"),
            }
        }
    }

    fn untag(tagged: &[u8]) -> Vec<u8> {
        let mut f = tagged[..12].to_vec();
        f.extend_from_slice(&tagged[16..]);
        f
    }

    fn tag(untagged: &[u8]) -> Vec<u8> {
        let mut f = untagged[..12].to_vec();
        f.extend_from_slice(&[0x81, 0x00, 0xc0, 0x00]);
        f.extend_from_slice(&untagged[12..]);
        f
    }

    #[test]
    fn frame_id_filter_has_the_documented_shape() {
        let p = frame_id_filter(0x8000, 0xBFFF);
        assert_eq!(p.len(), 13);
        assert_eq!(
            p[0],
            SockFilter {
                code: LD_H_ABS,
                jt: 0,
                jf: 0,
                k: 12
            }
        );
        assert_eq!(
            p[1],
            SockFilter {
                code: JEQ,
                jt: 0,
                jf: 2,
                k: 0x8892
            }
        );
        assert_eq!(
            p[2],
            SockFilter {
                code: LDX_IMM,
                jt: 0,
                jf: 0,
                k: 14
            }
        );
        assert_eq!(
            p[3],
            SockFilter {
                code: JA,
                jt: 0,
                jf: 0,
                k: 4
            }
        );
        assert_eq!(
            p[4],
            SockFilter {
                code: JEQ,
                jt: 0,
                jf: 6,
                k: 0x8100
            }
        );
        assert_eq!(
            p[5],
            SockFilter {
                code: LD_H_ABS,
                jt: 0,
                jf: 0,
                k: 16
            }
        );
        assert_eq!(
            p[6],
            SockFilter {
                code: JEQ,
                jt: 0,
                jf: 4,
                k: 0x8892
            }
        );
        assert_eq!(
            p[7],
            SockFilter {
                code: LDX_IMM,
                jt: 0,
                jf: 0,
                k: 18
            }
        );
        assert_eq!(
            p[8],
            SockFilter {
                code: LD_H_IND,
                jt: 0,
                jf: 0,
                k: 0
            }
        );
        assert_eq!(
            p[9],
            SockFilter {
                code: JGE,
                jt: 0,
                jf: 1,
                k: 0x8000
            }
        );
        assert_eq!(
            p[10],
            SockFilter {
                code: JGT,
                jt: 0,
                jf: 1,
                k: 0xBFFF
            }
        );
        assert_eq!(
            p[11],
            SockFilter {
                code: RET,
                jt: 0,
                jf: 0,
                k: 0
            }
        );
        assert_eq!(
            p[12],
            SockFilter {
                code: RET,
                jt: 0,
                jf: 0,
                k: 0xFFFF
            }
        );
    }

    #[test]
    fn rt_filter_accepts_rtc1_frames_tagged_or_not_and_rejects_the_rest() {
        let rt = rt_filter();
        let cpu = golden_rt("rtc_cpu_8001"); // tagged, FrameID 0x8001
        assert_eq!(run(&rt, &cpu), 0xFFFF);
        assert_eq!(run(&rt, &untag(&cpu)), 0xFFFF);
        let dev = golden_rt("rtc_dev_8000");
        assert_eq!(run(&rt, &dev), 0xFFFF);
        let dcp = golden("ident_ok_pnet"); // untagged, FrameID 0xFEFF
        assert_eq!(run(&rt, &dcp), 0);
        assert_eq!(run(&rt, &tag(&dcp)), 0);
    }

    #[test]
    fn acyclic_filter_accepts_dcp_and_rejects_rtc1() {
        let ac = acyclic_filter();
        let dcp = golden("ident_ok_pnet");
        assert_eq!(run(&ac, &dcp), 0xFFFF);
        assert_eq!(run(&ac, &tag(&dcp)), 0xFFFF);
        let cpu = golden_rt("rtc_cpu_8001");
        assert_eq!(run(&ac, &cpu), 0);
        assert_eq!(run(&ac, &untag(&cpu)), 0);
    }

    #[test]
    fn both_filters_reject_ipv4_and_short_frames() {
        let mut ipv4 = golden("ident_ok_pnet");
        ipv4[12] = 0x08;
        ipv4[13] = 0x00;
        assert_eq!(run(&rt_filter(), &ipv4), 0);
        assert_eq!(run(&acyclic_filter(), &ipv4), 0);
        assert_eq!(run(&rt_filter(), &ipv4[..13]), 0);
        assert_eq!(run(&acyclic_filter(), &[0x81, 0x00]), 0);
    }
}

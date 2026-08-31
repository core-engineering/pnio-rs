//! RTC1 cyclic frame codec.
//!
//! Wire layout of an RTC1 frame (VLAN-tagged, priority 6):
//!
//! ```text
//! | dst (6) | src (6) | 0x8100 | TCI_RT (2) | 0x8892 | FrameID (2) | C-SDU (>=40) | cycle counter (2) | data status (1) | transfer status (1) |
//! ```
//!
//! An untagged frame omits the `0x8100`/TCI pair and starts the payload 4 bytes earlier.

use std::time::Duration;

use thiserror::Error;

use crate::eth::{EthHeader, MacAddr, ETHERTYPE_PROFINET, ETHERTYPE_VLAN};

/// VLAN TCI for PROFINET RT: priority 6 (RT_CLASS_1), VLAN ID 0.
pub const TCI_RT: u16 = 0xC000;
/// Minimum C-SDU length in bytes (Ethernet minimum payload floor).
pub const CSDU_MIN: usize = 40;
/// APDU status trailer length: cycle counter (2) + data status (1) + transfer status (1).
pub const APDU_LEN: usize = 4;
/// RTC1 send-clock base unit (31.25 us), per IEC 61158-6-10.
pub const CYCLE_UNIT: Duration = Duration::from_nanos(31_250);

/// Errors from parsing or writing an RTC1 frame.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FrameError {
    /// The input is shorter than a minimal RTC1 frame (header + FrameID + [`CSDU_MIN`] + [`APDU_LEN`]).
    #[error("frame too short: need {need}, have {have}")]
    TooShort {
        /// Bytes a minimal frame of this shape would need.
        need: usize,
        /// Bytes actually present in the input.
        have: usize,
    },
    /// The Ethernet header's ethertype is not [`ETHERTYPE_PROFINET`].
    #[error("not a PROFINET frame")]
    NotProfinet,
    /// [`RtFrame::write`]'s output buffer is smaller than [`frame_len`] requires.
    #[error("output buffer too small: need {need}, have {have}")]
    BufferTooSmall {
        /// Bytes [`frame_len`] computed for this C-SDU.
        need: usize,
        /// Bytes actually available in the output buffer.
        have: usize,
    },
}

/// APDU DataStatus byte (IEC 61158-6-10 table): provider/consumer state bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataStatus(pub u8);

impl DataStatus {
    /// Primary provider, RUN, data valid, station OK: the steady-state cyclic value.
    pub const RUN_PRIMARY_VALID_OK: DataStatus = DataStatus(0x35);

    /// Same as [`Self::RUN_PRIMARY_VALID_OK`] but with bit 5 (`Station_Problem_Indicator`)
    /// cleared: the station reports a problem (e.g. a channel diagnosis is pending).
    pub const RUN_PRIMARY_VALID_PROBLEM: DataStatus = DataStatus(0x15);

    /// Bit 0: `State.Primary` — 1 = primary AR, 0 = backup AR.
    pub fn primary(self) -> bool {
        self.0 & 0x01 != 0
    }

    /// Bit 1: `State.Redundancy` — 1 = redundant path available.
    pub fn redundancy(self) -> bool {
        self.0 & 0x02 != 0
    }

    /// Bit 2: `State.DataValid` — 1 = data valid.
    pub fn data_valid(self) -> bool {
        self.0 & 0x04 != 0
    }

    /// Bit 4: `Provider_State.Run` — 1 = provider in RUN.
    pub fn provider_run(self) -> bool {
        self.0 & 0x10 != 0
    }

    /// Bit 5: `Station_Problem_Indicator` — 1 = station OK.
    pub fn station_ok(self) -> bool {
        self.0 & 0x20 != 0
    }
}

/// A parsed or to-be-written RTC1 cyclic frame (APDU: DataRequestPDU/DataResponsePDU).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtFrame<'a> {
    /// The frame's `FrameID`, identifying the Communication Relationship (e.g. `0x8000`/`0x8001` on the bench).
    pub frame_id: u16,
    /// The C-SDU payload, at least [`CSDU_MIN`] bytes.
    pub csdu: &'a [u8],
    /// APDU cycle counter: increments by the CR's `cycle_step` every send cycle, wraps at `u16::MAX`.
    pub cycle_counter: u16,
    /// APDU DataStatus byte; see [`DataStatus`].
    pub data_status: DataStatus,
    /// APDU TransferStatus byte: `0` means usable, non-zero means the provider marks this frame not to be used.
    pub transfer_status: u8,
}

/// Total on-wire length of a VLAN-tagged RTC1 frame carrying a C-SDU of `csdu_len` bytes
/// (padded up to [`CSDU_MIN`] if shorter).
pub fn frame_len(csdu_len: usize) -> usize {
    18 + 2 + csdu_len.max(CSDU_MIN) + APDU_LEN
}

impl<'a> RtFrame<'a> {
    /// Parses an RTC1 frame: Ethernet header (tagged or untagged) + FrameID + C-SDU + APDU status.
    pub fn parse(frame: &'a [u8]) -> Result<(EthHeader, RtFrame<'a>), FrameError> {
        let (eth, off) = EthHeader::parse(frame).map_err(|_| {
            // `EthHeader::parse` failed before it could tell us the real header length; infer
            // it from whatever bytes are visible (a VLAN tag makes the header 18 bytes, else 14).
            let off = if frame.len() >= 14 && frame[12..14] == [0x81, 0x00] {
                18
            } else {
                14
            };
            FrameError::TooShort {
                need: off + 2 + CSDU_MIN + APDU_LEN,
                have: frame.len(),
            }
        })?;

        if eth.ethertype != ETHERTYPE_PROFINET {
            return Err(FrameError::NotProfinet);
        }

        let need = off + 2 + CSDU_MIN + APDU_LEN;
        if frame.len() < need {
            return Err(FrameError::TooShort {
                need,
                have: frame.len(),
            });
        }

        let frame_id = u16::from_be_bytes([frame[off], frame[off + 1]]);
        let apdu_off = frame.len() - APDU_LEN;
        let csdu = &frame[off + 2..apdu_off];
        let apdu = &frame[apdu_off..];
        let cycle_counter = u16::from_be_bytes([apdu[0], apdu[1]]);
        let data_status = DataStatus(apdu[2]);
        let transfer_status = apdu[3];

        Ok((
            eth,
            RtFrame {
                frame_id,
                csdu,
                cycle_counter,
                data_status,
                transfer_status,
            },
        ))
    }

    /// Serializes the frame as a VLAN-tagged RTC1 frame into `out`, allocation-free.
    ///
    /// The C-SDU is zero-padded up to [`CSDU_MIN`] if shorter. Returns the number of bytes
    /// written. Does not call [`EthHeader::write`] (which allocates into a `Vec`); the 18
    /// header bytes are written directly into the caller's slice instead.
    pub fn write(&self, out: &mut [u8], dst: MacAddr, src: MacAddr) -> Result<usize, FrameError> {
        let csdu_len = self.csdu.len().max(CSDU_MIN);
        let need = frame_len(self.csdu.len());
        if out.len() < need {
            return Err(FrameError::BufferTooSmall {
                need,
                have: out.len(),
            });
        }

        out[0..6].copy_from_slice(&dst.0);
        out[6..12].copy_from_slice(&src.0);
        out[12..14].copy_from_slice(&ETHERTYPE_VLAN.to_be_bytes());
        out[14..16].copy_from_slice(&TCI_RT.to_be_bytes());
        out[16..18].copy_from_slice(&ETHERTYPE_PROFINET.to_be_bytes());
        out[18..20].copy_from_slice(&self.frame_id.to_be_bytes());

        let csdu_start = 20;
        let csdu_end = csdu_start + self.csdu.len();
        out[csdu_start..csdu_end].copy_from_slice(self.csdu);
        for b in &mut out[csdu_end..csdu_start + csdu_len] {
            *b = 0;
        }

        let apdu_off = csdu_start + csdu_len;
        out[apdu_off..apdu_off + 2].copy_from_slice(&self.cycle_counter.to_be_bytes());
        out[apdu_off + 2] = self.data_status.0;
        out[apdu_off + 3] = self.transfer_status;

        Ok(need)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eth::MacAddr;
    use crate::testutil::{golden_rt, RT_CSDU_OFF};

    const CPU: MacAddr = MacAddr([0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f]);
    const DEV: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);

    #[test]
    fn parse_cpu_frame() {
        let f = golden_rt("rtc_cpu_8001");
        let (eth, rt) = RtFrame::parse(&f).unwrap();
        assert_eq!(eth.src, CPU);
        assert_eq!(eth.vlan, Some(TCI_RT));
        assert_eq!(rt.frame_id, 0x8001);
        assert_eq!(rt.csdu.len(), 40);
        assert_eq!(rt.csdu, &f[RT_CSDU_OFF..RT_CSDU_OFF + 40]);
        assert_eq!(rt.cycle_counter, 0xb800);
        assert_eq!(rt.data_status, DataStatus::RUN_PRIMARY_VALID_OK);
        assert!(
            rt.data_status.provider_run()
                && rt.data_status.primary()
                && rt.data_status.data_valid()
        );
        assert_eq!(rt.transfer_status, 0);
    }

    #[test]
    fn parse_untagged_frame_too() {
        let f = golden_rt("rtc_cpu_8001");
        let mut untagged = f[..12].to_vec();
        untagged.extend_from_slice(&f[16..]);
        let (eth, rt) = RtFrame::parse(&untagged).unwrap();
        assert_eq!(eth.vlan, None);
        assert_eq!(rt.frame_id, 0x8001);
        assert_eq!(rt.cycle_counter, 0xb800);
    }

    #[test]
    fn write_is_byte_exact_against_cpu_golden() {
        let f = golden_rt("rtc_cpu_8001");
        let (_, rt) = RtFrame::parse(&f).unwrap();
        let mut out = [0u8; 128];
        let n = rt.write(&mut out, DEV, CPU).unwrap();
        assert_eq!(n, 64);
        assert_eq!(&out[..n], &f[..]);
    }

    #[test]
    fn write_pads_short_csdu_to_40() {
        let rt = RtFrame {
            frame_id: 0x8000,
            csdu: &[1, 2, 3],
            cycle_counter: 1024,
            data_status: DataStatus::RUN_PRIMARY_VALID_OK,
            transfer_status: 0,
        };
        let mut out = [0u8; 128];
        let n = rt.write(&mut out, CPU, DEV).unwrap();
        assert_eq!(n, frame_len(3));
        assert_eq!(n, 64);
        assert_eq!(&out[20..23], &[1, 2, 3]);
        assert!(out[23..60].iter().all(|b| *b == 0));
        assert_eq!(&out[60..64], &[0x04, 0x00, 0x35, 0x00]);
    }

    #[test]
    fn data_status_bits() {
        let stop = DataStatus(0x25);
        assert!(!stop.provider_run() && stop.primary() && stop.data_valid() && stop.station_ok());
        let backup = DataStatus(0x36);
        assert!(!backup.primary() && backup.redundancy());
    }

    #[test]
    fn errors() {
        assert_eq!(
            RtFrame::parse(&golden_rt("rtc_cpu_8001")[..30]).unwrap_err(),
            FrameError::TooShort { need: 64, have: 30 }
        );
        assert_eq!(
            RtFrame::parse(&golden_rt("rtc_cpu_8001")[..16]).unwrap_err(),
            FrameError::TooShort { need: 64, have: 16 }
        );
        assert_eq!(
            RtFrame::parse(&[0u8; 10]).unwrap_err(),
            FrameError::TooShort { need: 60, have: 10 }
        );
        let mut ip = golden_rt("rtc_cpu_8001");
        ip[16] = 0x08;
        ip[17] = 0x00;
        assert_eq!(RtFrame::parse(&ip).unwrap_err(), FrameError::NotProfinet);
        let rt = RtFrame {
            frame_id: 1,
            csdu: &[0; 40],
            cycle_counter: 0,
            data_status: DataStatus(0),
            transfer_status: 0,
        };
        assert_eq!(
            rt.write(&mut [0u8; 10], CPU, DEV).unwrap_err(),
            FrameError::BufferTooSmall { need: 64, have: 10 }
        );
    }
}

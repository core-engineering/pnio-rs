//! I&M0-3 (Identification & Maintenance) record codec and store.
//!
//! I&M0 (`INDEX_IM0` = 0xAFF0) is read-only device identity, computed on demand by
//! [`encode_im0`] from a [`Im0`] plus the vendor id and supported mask. I&M1-3
//! (`INDEX_IM1..=INDEX_IM3`) are operator-writable free text (tag function/location,
//! install date, descriptor); [`ImStore`] keeps their bodies as raw, already
//! space-padded bytes and persists them to a flat file when a path is configured.

use crate::cm::block::BlockHeader;
use thiserror::Error;

/// `SwRevision.prefix` values allowed by the PROFINET spec.
const VALID_SW_PREFIXES: &str = "VRPUT";

/// Block type of the encoded I&M0 record (distinct from `INDEX_IM0`, the record data
/// index used to address it over the PNIO Read/Write service).
const BLOCK_TYPE_IM0: u16 = 0x0020;
const BLOCK_TYPE_IM1: u16 = 0x0021;
const BLOCK_TYPE_IM2: u16 = 0x0022;
const BLOCK_TYPE_IM3: u16 = 0x0023;

/// Record data indices for the PNIO Read/Write service (§ I&M records).
pub const INDEX_IM0: u16 = 0xAFF0;
pub const INDEX_IM1: u16 = 0xAFF1;
pub const INDEX_IM2: u16 = 0xAFF2;
pub const INDEX_IM3: u16 = 0xAFF3;

/// `IM_Supported` bitmask: I&M1, I&M2 and I&M3 are supported (bits 1-3). The p-net
/// capture answers this same value on every submodule it knows — the DAP (slot 0
/// subslot 1) and the interface submodule (slot 0 subslot 0x8000) alike — so there is
/// no "nothing supported" variant to encode (see `docs/alarm-golden-frames.md`).
pub const IM_SUPPORTED_DAP: u16 = 0x000E;

/// Fixed body lengths (bytes after the 6-byte block header) of the I&M1-3 records.
pub const IM1_LEN: usize = 54;
pub const IM2_LEN: usize = 16;
pub const IM3_LEN: usize = 54;

/// `IM_Software_Revision`: a prefix letter ('V'ersion, 'R'evision, 'P'rototype,
/// 'U'nder test, 'T'est device) plus a three-part x.y.z revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwRevision {
    pub prefix: char,
    pub functional: u8,
    pub bug_fix: u8,
    pub internal: u8,
}

/// I&M0: read-only device identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Im0 {
    pub order_id: String,
    pub serial_number: String,
    pub hardware_revision: u16,
    pub software_revision: SwRevision,
    pub revision_counter: u16,
    pub profile_id: u16,
    pub profile_specific_type: u16,
}

impl Im0 {
    /// ASCII-only, `order_id` at most 20 bytes, `serial_number` at most 16 bytes,
    /// `software_revision.prefix` one of `VRPUT`.
    pub fn validate(&self) -> Result<(), ImError> {
        if !self.order_id.is_ascii() {
            return Err(ImError::NotAscii { field: "order_id" });
        }
        if self.order_id.len() > 20 {
            return Err(ImError::TooLong {
                field: "order_id",
                max: 20,
            });
        }
        if !self.serial_number.is_ascii() {
            return Err(ImError::NotAscii {
                field: "serial_number",
            });
        }
        if self.serial_number.len() > 16 {
            return Err(ImError::TooLong {
                field: "serial_number",
                max: 16,
            });
        }
        if !VALID_SW_PREFIXES.contains(self.software_revision.prefix) {
            return Err(ImError::BadPrefix(self.software_revision.prefix));
        }
        Ok(())
    }
}

impl Default for Im0 {
    fn default() -> Self {
        Im0 {
            order_id: "pnio device".to_string(),
            serial_number: String::new(),
            hardware_revision: 1,
            software_revision: SwRevision {
                prefix: 'V',
                functional: 0,
                bug_fix: 1,
                internal: 0,
            },
            revision_counter: 0,
            profile_id: 0,
            profile_specific_type: 0,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ImError {
    #[error("{field} is not ASCII")]
    NotAscii { field: &'static str },
    #[error("{field} longer than {max} bytes")]
    TooLong { field: &'static str, max: usize },
    #[error("bad software revision prefix {0:?}")]
    BadPrefix(char),
    #[error("record {index:#06x} has a bad shape: {why}")]
    BadRecord { index: u16, why: &'static str },
}

/// Push `s` into `out` as exactly `len` ASCII bytes: truncated if longer, space-padded
/// if shorter.
fn push_padded(out: &mut Vec<u8>, s: &str, len: usize) {
    let bytes = s.as_bytes();
    let n = bytes.len().min(len);
    out.extend_from_slice(&bytes[..n]);
    out.extend(std::iter::repeat(b' ').take(len - n));
}

/// Encode the 60-byte I&M0 record (6-byte block header, block type 0x0020, length 56,
/// version 1.0, followed by 54 bytes of identity fields).
pub fn encode_im0(vendor_id: u16, im0: &Im0, supported: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(60);
    BlockHeader::write(&mut out, BLOCK_TYPE_IM0, 54);
    out.extend_from_slice(&vendor_id.to_be_bytes());
    push_padded(&mut out, &im0.order_id, 20);
    push_padded(&mut out, &im0.serial_number, 16);
    out.extend_from_slice(&im0.hardware_revision.to_be_bytes());
    out.push(im0.software_revision.prefix as u8);
    out.push(im0.software_revision.functional);
    out.push(im0.software_revision.bug_fix);
    out.push(im0.software_revision.internal);
    out.extend_from_slice(&im0.revision_counter.to_be_bytes());
    out.extend_from_slice(&im0.profile_id.to_be_bytes());
    out.extend_from_slice(&im0.profile_specific_type.to_be_bytes());
    out.push(1); // IM_Version.major
    out.push(1); // IM_Version.minor
    out.extend_from_slice(&supported.to_be_bytes());
    out
}

/// Trim trailing spaces and NULs, then decode as (lossy) UTF-8.
fn trimmed(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_end_matches(['\0', ' '])
        .to_string()
}

/// The writable I&M1-3 records: raw record bodies (already space-padded to their
/// fixed length), optionally persisted to a flat file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImStore {
    im1: [u8; IM1_LEN],
    im2: [u8; IM2_LEN],
    im3: [u8; IM3_LEN],
    pub path: Option<std::path::PathBuf>,
}

impl ImStore {
    /// All fields blank (all spaces), no backing file.
    pub fn new() -> Self {
        ImStore {
            im1: [b' '; IM1_LEN],
            im2: [b' '; IM2_LEN],
            im3: [b' '; IM3_LEN],
            path: None,
        }
    }

    /// Load from `path` if given: a well-formed store file is exactly
    /// `IM1_LEN + IM2_LEN + IM3_LEN` (124) bytes, the three bodies back to back. A
    /// missing file or one of the wrong length falls back to [`ImStore::new`] (plus
    /// the configured `path`, so subsequent writes still persist) and logs a warning.
    pub fn load(path: Option<std::path::PathBuf>) -> Self {
        let mut store = Self::new();
        store.path = path.clone();
        let Some(path) = path else {
            return store;
        };
        match std::fs::read(&path) {
            Ok(data) if data.len() == IM1_LEN + IM2_LEN + IM3_LEN => {
                store.im1.copy_from_slice(&data[..IM1_LEN]);
                store.im2.copy_from_slice(&data[IM1_LEN..IM1_LEN + IM2_LEN]);
                store
                    .im3
                    .copy_from_slice(&data[IM1_LEN + IM2_LEN..IM1_LEN + IM2_LEN + IM3_LEN]);
            }
            Ok(data) => {
                log::warn!(
                    "I&M store {}: expected {} bytes, found {}; using blank records",
                    path.display(),
                    IM1_LEN + IM2_LEN + IM3_LEN,
                    data.len()
                );
            }
            Err(e) => {
                log::warn!("I&M store {}: {e}; using blank records", path.display());
            }
        }
        store
    }

    /// The full record (block header + body) for `INDEX_IM1..=INDEX_IM3`; `None` for
    /// any other index (I&M0 is computed by [`encode_im0`], not stored here).
    pub fn read(&self, index: u16) -> Option<Vec<u8>> {
        let (block_type, body): (u16, &[u8]) = match index {
            INDEX_IM1 => (BLOCK_TYPE_IM1, &self.im1[..]),
            INDEX_IM2 => (BLOCK_TYPE_IM2, &self.im2[..]),
            INDEX_IM3 => (BLOCK_TYPE_IM3, &self.im3[..]),
            _ => return None,
        };
        let mut out = Vec::with_capacity(6 + body.len());
        BlockHeader::write(&mut out, block_type, body.len() as u16);
        out.extend_from_slice(body);
        Some(out)
    }

    /// Validate and store a full record (block header + body) written by the
    /// controller. `record`'s block header must parse, its block type must match
    /// `index`, and its body must have the fixed length for that record. On success
    /// the store is persisted (temp file + rename) if a `path` is configured; a
    /// filesystem error is logged but does not fail the write.
    pub fn write(&mut self, index: u16, record: &[u8]) -> Result<(), ImError> {
        let (block_type, expected_len): (u16, usize) = match index {
            INDEX_IM1 => (BLOCK_TYPE_IM1, IM1_LEN),
            INDEX_IM2 => (BLOCK_TYPE_IM2, IM2_LEN),
            INDEX_IM3 => (BLOCK_TYPE_IM3, IM3_LEN),
            _ => {
                return Err(ImError::BadRecord {
                    index,
                    why: "unknown I&M record index",
                })
            }
        };
        let (header, body) = BlockHeader::parse(record).map_err(|_| ImError::BadRecord {
            index,
            why: "malformed block header",
        })?;
        if header.block_type != block_type {
            return Err(ImError::BadRecord {
                index,
                why: "block type does not match the record index",
            });
        }
        if body.len() != expected_len {
            return Err(ImError::BadRecord {
                index,
                why: "body length does not match the fixed record length",
            });
        }
        match index {
            INDEX_IM1 => self.im1.copy_from_slice(body),
            INDEX_IM2 => self.im2.copy_from_slice(body),
            INDEX_IM3 => self.im3.copy_from_slice(body),
            _ => unreachable!("index already matched above"),
        }
        self.persist();
        Ok(())
    }

    /// Write the three record bodies back to back to `path` (temp file in the same
    /// directory, then rename, for atomicity). No-op if no `path` is configured. A
    /// filesystem error is logged and otherwise ignored.
    fn persist(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let mut data = Vec::with_capacity(IM1_LEN + IM2_LEN + IM3_LEN);
        data.extend_from_slice(&self.im1);
        data.extend_from_slice(&self.im2);
        data.extend_from_slice(&self.im3);
        let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        let tmp = dir.join(format!(".im-{}.tmp", std::process::id()));
        let result = std::fs::write(&tmp, &data).and_then(|_| std::fs::rename(&tmp, path));
        if let Err(e) = result {
            log::error!("I&M store {}: failed to persist: {e}", path.display());
        }
    }

    pub fn tag_function(&self) -> String {
        trimmed(&self.im1[..32])
    }

    pub fn tag_location(&self) -> String {
        trimmed(&self.im1[32..IM1_LEN])
    }

    pub fn date(&self) -> String {
        trimmed(&self.im2[..IM2_LEN])
    }

    pub fn descriptor(&self) -> String {
        trimmed(&self.im3[..IM3_LEN])
    }
}

impl Default for ImStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::golden_alarm;

    fn pnet_im0() -> Im0 {
        Im0 {
            order_id: "12345 Abcdefghijk".into(),
            serial_number: "007".into(),
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
        }
    }

    #[test]
    fn im0_encoding_matches_the_pnet_read_response_record() {
        let res = golden_alarm("im0_read_res");
        // RPC header 80 + NDR response 20 + IODReadResHeader 64 = 164 bytes after the 42-byte Ethernet/IP/UDP prefix
        let record = &res[42 + 80 + 20 + 64..];
        assert_eq!(record.len(), 60);
        assert_eq!(encode_im0(0x0493, &pnet_im0(), IM_SUPPORTED_DAP), record);
    }

    #[test]
    fn im0_validation() {
        let mut i = pnet_im0();
        i.order_id = "x".repeat(21);
        assert_eq!(
            i.validate(),
            Err(ImError::TooLong {
                field: "order_id",
                max: 20
            })
        );
        i = pnet_im0();
        i.serial_number = "é".into();
        assert_eq!(
            i.validate(),
            Err(ImError::NotAscii {
                field: "serial_number"
            })
        );
        i = pnet_im0();
        i.software_revision.prefix = 'X';
        assert_eq!(i.validate(), Err(ImError::BadPrefix('X')));
        assert_eq!(Im0::default().validate(), Ok(()));
    }

    #[test]
    fn store_round_trips_records_and_persists() {
        let dir = std::env::temp_dir().join(format!("pnio-im-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("im.bin");
        let mut s = ImStore::load(Some(path.clone()));
        assert_eq!(s.tag_function(), "");
        let mut rec = Vec::new();
        // NOTE: body length is 54 (IM1_LEN = TagFunction 32 + TagLocation 22), so
        // BlockHeader::write's body_len argument is 54 (it stores block_length =
        // 54 + 2 = 56 on the wire, matching the record's documented "length 56").
        crate::cm::block::BlockHeader::write(&mut rec, 0x0021, 54);
        rec.extend_from_slice(format!("{:<32}{:<22}", "TEST-FUNC", "TEST-LOC").as_bytes());
        s.write(INDEX_IM1, &rec).unwrap();
        assert_eq!(s.read(INDEX_IM1).unwrap(), rec);
        assert_eq!(
            (s.tag_function(), s.tag_location()),
            ("TEST-FUNC".into(), "TEST-LOC".into())
        );
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 124);
        let again = ImStore::load(Some(path.clone()));
        assert_eq!(again.tag_function(), "TEST-FUNC");
        assert_eq!(s.read(0xAFF0), None);
        let bad = &rec[..30];
        assert!(matches!(
            s.write(INDEX_IM1, bad),
            Err(ImError::BadRecord { index: 0xAFF1, .. })
        ));
        let mut wrong_type = rec.clone();
        wrong_type[1] = 0x22;
        assert!(matches!(
            s.write(INDEX_IM1, &wrong_type),
            Err(ImError::BadRecord { .. })
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn short_or_missing_file_is_empty_store() {
        let s = ImStore::load(Some(std::path::PathBuf::from("/nonexistent/pnio-im.bin")));
        assert_eq!(
            s,
            ImStore {
                path: Some("/nonexistent/pnio-im.bin".into()),
                ..ImStore::new()
            }
        );
        assert_eq!(s.read(INDEX_IM2).unwrap().len(), 22);
    }
}

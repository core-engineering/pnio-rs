//! `profinet-rt` — pure-Rust PROFINET RT IO-Device stack.
//!
//! Community project, NOT affiliated with or endorsed by PROFIBUS & PROFINET
//! International. "PROFINET" is a registered trademark of PNO.
//!
//! # Example: answer a DCP Identify on the device side
//!
//! ```
//! use profinet_rt::dcp::{handle_dcp_frame, DeviceConfig, DeviceProperties};
//! use profinet_rt::eth::MacAddr;
//!
//! let cfg = DeviceConfig {
//!     mac: MacAddr([0x02, 0xc0, 0xa8, 0x01, 0x0f, 0x02]),
//!     properties: DeviceProperties {
//!         name_of_station: "i-device".to_string(),
//!         type_of_station: "demo".to_string(),
//!         vendor_id: 0x002a,
//!         device_id: 0x010e,
//!         device_role: 0,
//!         device_instance: 0x1064,
//!         device_options: vec![0x02, 0x07],
//!         ip: [192, 168, 1, 61],
//!         subnet: [255, 255, 255, 0],
//!         gateway: [192, 168, 1, 61],
//!         ip_block_info: 1,
//!     },
//! };
//!
//! // A DCP Identify request for "i-device" produces an Identify response.
//! let req = [
//!     0x01, 0x0e, 0xcf, 0x00, 0x00, 0x00, 0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f, 0x88, 0x92,
//!     0xfe, 0xfe, 0x05, 0x00, 0x03, 0x00, 0x01, 0x52, 0x00, 0x01, 0x00, 0x0c, 0x02, 0x02,
//!     0x00, 0x08, 0x69, 0x2d, 0x64, 0x65, 0x76, 0x69, 0x63, 0x65,
//! ];
//! let resp = handle_dcp_frame(&req, &cfg).unwrap();
//! assert!(resp.is_some());
//! ```

pub mod capture;
pub mod data;
pub mod dcp;
pub mod eth;
pub mod rpc;
#[cfg(test)]
pub(crate) mod testutil;

/// Crate version (foundations smoke test).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_not_empty() {
        assert!(!version().is_empty());
    }
}

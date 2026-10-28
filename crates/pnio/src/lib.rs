//! `pnio` — pure-Rust PROFINET RT IO-Device stack.
//!
//! Community project, NOT affiliated with or endorsed by PROFIBUS & PROFINET
//! International. "PROFINET" is a registered trademark of PNO.
//!
//! # API surface
//!
//! The supported API is the [`api`] facade ([`api::IoDevice`] and the types it re-exports), the
//! device declaration ([`config`], [`data`], [`im`]), channel diagnosis ([`diag`]), the GSDML
//! generator ([`gsdml`]) and the capture reader ([`capture`]). The protocol layers (`alarm`, `cm`,
//! `dcp`, `device`, `eth`, `lldp`, `pdev`, `rpc`, `rt`) stay `pub` for the integration tests and
//! the bring-up examples, but are hidden from this documentation and may change in any 0.x
//! release.
//!
//! # Example: declare a device and render its GSDML
//!
//! ```
//! use pnio::config::{DeviceConfig, Slot};
//! use pnio::data::FieldType;
//! use pnio::gsdml::{file_name, render, GsdmlMeta};
//!
//! let cfg = DeviceConfig::builder("pnio-dev")
//!     .input(Slot(1), &[FieldType::Real; 4]) // device -> controller (%I)
//!     .output(Slot(2), &[FieldType::Bool; 8]) // controller -> device (%Q)
//!     .build()
//!     .expect("valid declaration");
//! assert_eq!(cfg.input_len(Slot(1)), Some(16)); // 4 REAL, big-endian, no padding
//!
//! let meta = GsdmlMeta {
//!     vendor_name: "Example".into(),
//!     product_family: "pnio".into(),
//!     info_text: "4 REAL in, 8 BOOL out".into(),
//!     date: (2026, 10, 28),
//! };
//! let xml = render(&cfg, &meta); // save as `file_name(&meta)`, import in the engineering tool
//! assert!(xml.starts_with("<?xml"));
//! assert!(file_name(&meta).starts_with("GSDML-V2."));
//! ```
//!
//! On Linux, [`api::IoDevice::start`] then runs the declared device on a network interface.

// `unsafe` is confined to the raw-socket / scheduler layer: `eth::afpacket`, `eth::poll`,
// `rt::sched`, `rt::runner`. Every other module is checked by the compiler to contain none.
#![deny(unsafe_code)]
#![warn(missing_docs)]

#[doc(hidden)]
pub mod alarm;
#[cfg(target_os = "linux")]
pub mod api;
#[cfg(feature = "capture")]
pub mod capture;
#[doc(hidden)]
pub mod cm;
pub mod config;
pub mod data;
#[doc(hidden)]
pub mod dcp;
#[doc(hidden)]
pub mod device;
pub mod diag;
#[doc(hidden)]
pub mod eth;
pub mod gsdml;
pub mod im;
#[doc(hidden)]
pub mod lldp;
#[doc(hidden)]
pub mod pdev;
#[doc(hidden)]
pub mod rpc;
#[doc(hidden)]
pub mod rt;
#[cfg(test)]
pub(crate) mod testutil;

//! PROFINET Context Manager (CM): PNIO status, PNIO block header, and the
//! Connect-request block parsers (ARBlockReq, IOCRBlockReq, ExpectedSubmoduleBlockReq,
//! AlarmCRBlockReq) needed to establish an Application Relationship (AR).

pub mod block;
pub mod connect;
pub mod model;
pub mod status;
pub mod write;

pub use block::{
    ty, AlarmCrBlockReq, ArBlockReq, BlockHeader, Cursor, DataDescription, ExpectedApi,
    ExpectedSubmodule, ExpectedSubmoduleBlockReq, IocrApi, IocrBlockReq, IocrObject,
};
pub use connect::{build_connect_res, validate, ArParams, ConnectReq, IocrParams};
pub use model::{DeviceModel, SlotModel, SubmoduleModel};
pub use status::{ConnectBlock, PnioStatus};
pub use write::{build_write_res, Record, WriteReq, INDEX_MULTIPLE_WRITE};

use crate::rpc::Uuid;
use thiserror::Error;

/// Errors from parsing/serializing PNIO blocks (the 6-byte header and the per-type bodies).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BlockError {
    #[error("block buffer too short: need {need}, have {have}")]
    TooShort { need: usize, have: usize },
    #[error("unexpected block type: expected {expected:#06x}, got {got:#06x}")]
    UnexpectedType { expected: u16, got: u16 },
    #[error("bad block version {0}.{1} (expected 1.0)")]
    BadVersion(u8, u8),
    #[error("bad block length: declared {declared}, available {available}")]
    BadLength { declared: u16, available: usize },
    #[error("malformed block: {0}")]
    Malformed(&'static str),
}

/// Errors from the Context Manager's AR establishment / lifecycle handling.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CmError {
    #[error("block error: {0}")]
    Block(#[from] BlockError),
    #[error("connect rejected: {0:?}")]
    Reject(PnioStatus),
    #[error("wrong state for {event}: {state}")]
    WrongState {
        event: &'static str,
        state: &'static str,
    },
    #[error("unknown AR {0}")]
    UnknownAr(Uuid),
}

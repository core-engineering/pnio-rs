//! Cyclic RTC1 exchange: frame codec, I/O layout, and the real-time provider/consumer thread.

pub mod frame;

pub use frame::{
    frame_len, DataStatus, FrameError, RtFrame, APDU_LEN, CSDU_MIN, CYCLE_UNIT, TCI_RT,
};

use thiserror::Error;

/// Top-level error type for the `rt` cyclic exchange.
#[derive(Debug, Error)]
pub enum RtError {
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("scheduling error: {0}")]
    Sched(std::io::Error),
    #[error("stopped")]
    Stopped,
}

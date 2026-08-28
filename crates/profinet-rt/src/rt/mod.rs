//! Cyclic RTC1 exchange: frame codec, I/O layout, and the real-time provider/consumer thread.

pub mod engine;
pub mod frame;
pub mod image;
pub mod layout;

pub use engine::{
    DropReason, RtEngine, RtStats, RxVerdict, StatsSnapshot, WatchdogVerdict, IOXS_BAD, IOXS_GOOD,
};
pub use frame::{
    frame_len, DataStatus, FrameError, RtFrame, APDU_LEN, CSDU_MIN, CYCLE_UNIT, TCI_RT,
};
pub use image::{Freshness, ImageError, IoImage, Validity, WatchdogState};
pub use layout::{Cell, CrLayout, CsObject, IoObject, Layout, LayoutError};

use thiserror::Error;

/// Top-level error type for the `rt` cyclic exchange.
#[derive(Debug, Error)]
pub enum RtError {
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error(transparent)]
    Layout(#[from] LayoutError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("scheduling error: {0}")]
    Sched(std::io::Error),
    #[error("stopped")]
    Stopped,
}

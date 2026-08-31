//! Cyclic RTC1 exchange: frame codec, I/O layout, and the real-time provider/consumer thread.

pub mod engine;
pub mod frame;
pub mod hist;
pub mod image;
pub mod layout;
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub mod runner;
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub mod sched;

pub use engine::{
    DropReason, RtEngine, RtStats, RxVerdict, StatsSnapshot, WatchdogVerdict, IOXS_BAD, IOXS_GOOD,
};
pub use frame::{
    frame_len, DataStatus, FrameError, RtFrame, APDU_LEN, CSDU_MIN, CYCLE_UNIT, TCI_RT,
};
pub use hist::{HistSnapshot, Histogram, HIST_BINS};
pub use image::{Freshness, ImageError, IoImage, Validity, WatchdogState};
pub use layout::{Cell, CrLayout, CsObject, IoObject, Layout, LayoutError};
#[cfg(target_os = "linux")]
pub use runner::{RtConfig, RtEvent, RtHandle, RtRunner};

use thiserror::Error;

/// Top-level error type for the `rt` cyclic exchange.
#[derive(Debug, Error)]
pub enum RtError {
    /// A cyclic frame failed to parse or did not fit its buffer; see [`FrameError`].
    #[error(transparent)]
    Frame(#[from] FrameError),
    /// The submodule layout could not be built or an offset fell outside it; see
    /// [`LayoutError`].
    #[error(transparent)]
    Layout(#[from] LayoutError),
    /// A raw-socket or `epoll`/`timerfd` syscall failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The Ethernet transport layer failed to send or receive; see
    /// [`TransportError`](crate::eth::TransportError).
    #[error(transparent)]
    Transport(#[from] crate::eth::TransportError),
    /// Reserved for fatal real-time scheduling failures. Not constructed today: `SCHED_FIFO`,
    /// affinity and `mlockall` failures are reported as non-fatal [`RtEvent::SchedWarning`]s
    /// and the thread runs on at its inherited priority.
    #[error("scheduling error: {0}")]
    Sched(std::io::Error),
    /// The RT thread was still running when [`RtHandle::join`]'s timeout elapsed; it is left
    /// detached and releases its resources when it eventually exits.
    #[error("stopped")]
    Stopped,
}

//! PROFINET alarm channel (RTA over `0x8892`, FrameIDs `0xFC01` High / `0xFE01` Low):
//! the codec (`rta`) and the one-alarm-in-flight sender/receiver state machine
//! (`channel`). Pure: no sockets, no clock — the device loop drives both.
pub mod rta;
pub use rta::*;

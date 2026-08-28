#[cfg(target_os = "linux")]
mod afpacket;
pub mod bpf;
mod header;
pub(crate) mod poll;
mod transport;

#[cfg(target_os = "linux")]
pub use afpacket::AfPacketTransport;
pub use bpf::SockFilter;
pub use header::{EthError, EthHeader, MacAddr, ETHERTYPE_PROFINET, ETHERTYPE_VLAN};
pub use transport::{EthTransport, MockTransport, TransportError, MAX_FRAME_LEN};

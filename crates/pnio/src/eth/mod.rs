//! Raw Ethernet layer: the L2 header codec, the [`EthTransport`] send/receive
//! abstraction, a BPF filter builder, and the Linux `AF_PACKET` transport
//! ([`AfPacketTransport`], the only production backend).

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod afpacket;
pub mod bpf;
mod header;
#[allow(unsafe_code)]
pub(crate) mod poll;
mod transport;

#[cfg(target_os = "linux")]
pub use afpacket::AfPacketTransport;
pub use bpf::SockFilter;
pub use header::{EthError, EthHeader, MacAddr, ETHERTYPE_PROFINET, ETHERTYPE_VLAN};
pub use transport::{EthTransport, MockTransport, TransportError, MAX_FRAME_LEN};

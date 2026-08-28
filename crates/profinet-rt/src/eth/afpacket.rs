use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

use nix::sys::socket::{send, MsgFlags};

use super::poll::wait_readable;
use super::transport::{EthTransport, TransportError};
use super::{ETHERTYPE_PROFINET, ETHERTYPE_VLAN};
use crate::dcp::frame::DCP_MULTICAST_MAC;

impl From<nix::errno::Errno> for TransportError {
    fn from(e: nix::errno::Errno) -> Self {
        TransportError::Io(std::io::Error::from(e))
    }
}

/// Returns true if the raw frame is a PROFINET frame (VLAN-tagged or untagged).
fn is_profinet_frame(buf: &[u8]) -> bool {
    if buf.len() < 14 {
        return false;
    }
    let et = u16::from_be_bytes([buf[12], buf[13]]);
    if et == ETHERTYPE_PROFINET {
        return true;
    }
    et == ETHERTYPE_VLAN
        && buf.len() >= 18
        && u16::from_be_bytes([buf[16], buf[17]]) == ETHERTYPE_PROFINET
}

/// Raw AF_PACKET socket bound to a named interface and the PROFINET EtherType
/// (0x8892), with membership in the DCP multicast group, filtered on EtherType
/// PROFINET at recv time. In practice, live frames delivered to a
/// protocol-bound `AF_PACKET` socket always arrive with any 802.1Q tag already
/// stripped by the kernel (`__netif_receive_skb_core` untags before
/// protocol-keyed dispatch, and `packet_rcv` does not reinsert it) — the tag is
/// only observable via `PACKET_AUXDATA` + `recvmsg`, which this transport does
/// not use. `is_profinet_frame`'s VLAN-tagged branch therefore never matches
/// live `AfPacketTransport` traffic; it exists for the mock/replay path, where
/// captured frames can still carry the tag, and as a hook for a possible future
/// `ETH_P_ALL`-bound fallback that would see tagged frames.
pub struct AfPacketTransport {
    fd: OwnedFd,
}

impl AfPacketTransport {
    /// Open a raw AF_PACKET socket on `ifname`, bound to EtherType 0x8892 and
    /// joined to the DCP multicast group (01:0e:cf:00:00:00).
    ///
    /// Returns `Err(TransportError::Io)` if the interface does not exist or the
    /// process lacks `CAP_NET_RAW`.
    pub fn open(ifname: &str) -> Result<Self, TransportError> {
        let profinet_protocol = (ETHERTYPE_PROFINET).to_be() as i32;

        // Safety: `socket(2)` with valid, constant arguments; the returned fd is
        // immediately wrapped in an `OwnedFd`, which takes ownership and closes it
        // on drop.
        let raw_fd = unsafe {
            libc::socket(
                libc::AF_PACKET,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                profinet_protocol,
            )
        };
        if raw_fd < 0 {
            return Err(TransportError::Io(std::io::Error::last_os_error()));
        }
        // Safety: `raw_fd` was just returned by a successful `socket(2)` call above
        // and is not owned anywhere else.
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

        // Resolve interface name -> index.  Returns ENODEV if unknown.
        let ifindex = nix::net::if_::if_nametoindex(ifname)?;

        // nix 0.27 LinkAddr has no public constructor, so we build sockaddr_ll directly.
        // Safety: `sockaddr_ll` is a plain-old-data struct for which an all-zero bit
        // pattern is a valid value; every field we rely on is explicitly set below.
        let mut sll: libc::sockaddr_ll = unsafe { mem::zeroed() };
        sll.sll_family = libc::AF_PACKET as u16;
        sll.sll_protocol = (ETHERTYPE_PROFINET).to_be();
        sll.sll_ifindex = ifindex as libc::c_int;

        // Safety: `sll` is a valid, fully-initialized `sockaddr_ll` on the stack for
        // the duration of the call; `fd` is a valid, open socket.
        let ret = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                &sll as *const libc::sockaddr_ll as *const libc::sockaddr,
                mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            )
        };
        if ret < 0 {
            return Err(TransportError::Io(std::io::Error::last_os_error()));
        }

        // Join the DCP multicast group so multicast DCP frames (Identify, Hello)
        // reach this socket even when the NIC does not pass all multicast by default.
        // Safety: `packet_mreq` is a plain-old-data struct for which an all-zero bit
        // pattern is a valid value; every field we rely on is explicitly set below.
        let mut mreq: libc::packet_mreq = unsafe { mem::zeroed() };
        mreq.mr_ifindex = ifindex as libc::c_int;
        mreq.mr_type = libc::PACKET_MR_MULTICAST as u16;
        mreq.mr_alen = 6;
        mreq.mr_address[..6].copy_from_slice(&DCP_MULTICAST_MAC.0);

        // Safety: `mreq` is a valid, fully-initialized `packet_mreq` on the stack for
        // the duration of the call; `fd` is a valid, open socket.
        let ret = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_PACKET,
                libc::PACKET_ADD_MEMBERSHIP,
                &mreq as *const libc::packet_mreq as *const libc::c_void,
                mem::size_of::<libc::packet_mreq>() as libc::socklen_t,
            )
        };
        if ret < 0 {
            return Err(TransportError::Io(std::io::Error::last_os_error()));
        }

        Ok(Self { fd })
    }
}

impl EthTransport for AfPacketTransport {
    fn send(&self, frame: &[u8]) -> Result<(), TransportError> {
        send(self.fd.as_raw_fd(), frame, MsgFlags::empty())?;
        Ok(())
    }

    /// Returns `Ok(Some(frame))` only for PROFINET frames (untagged or VLAN-tagged).
    /// Returns `Ok(None)` for any other frame, or if `timeout` elapses before a
    /// frame arrives.
    fn recv(&self, timeout: Option<Duration>) -> Result<Option<Vec<u8>>, TransportError> {
        if !wait_readable(self.fd.as_raw_fd(), timeout)? {
            return Ok(None);
        }
        let mut buf = vec![0u8; 1522];
        // Safety: `buf` is a valid, writable allocation of `buf.len()` bytes and
        // `from`/`from_len` a valid, fully-initialized `sockaddr_ll` plus its length,
        // all live for the duration of the call; `fd` is a valid, open socket.
        let mut from: libc::sockaddr_ll = unsafe { mem::zeroed() };
        let mut from_len = mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t;
        let n = unsafe {
            libc::recvfrom(
                self.fd.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                0,
                &mut from as *mut libc::sockaddr_ll as *mut libc::sockaddr,
                &mut from_len,
            )
        };
        if n < 0 {
            return Err(TransportError::Io(std::io::Error::last_os_error()));
        }
        // Our own transmissions are looped back to every `AF_PACKET` socket on the
        // interface, including the one that sent them: drop them here so the cyclic
        // engine never sees its own provider frames.
        if from.sll_pkttype == libc::PACKET_OUTGOING {
            return Ok(None);
        }
        buf.truncate(n as usize);
        if is_profinet_frame(&buf) {
            Ok(Some(buf))
        } else {
            Ok(None)
        }
    }

    fn raw_fd(&self) -> Option<RawFd> {
        Some(self.fd.as_raw_fd())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_unknown_interface_errors() {
        let r = AfPacketTransport::open("nonexistent-iface-xyz");
        assert!(r.is_err());
    }

    #[test]
    #[ignore = "requires CAP_NET_RAW + a real interface; run: cargo test -- --ignored"]
    fn open_loopback_succeeds() {
        // Adapt the interface name to the test machine (e.g., "lo", "eth0").
        let t = AfPacketTransport::open("lo").expect("open lo");
        assert_eq!(t.recv(Some(Duration::from_millis(10))).unwrap(), None);
        assert!(t.raw_fd().is_some());
    }

    #[test]
    fn accepts_untagged_profinet() {
        // 6 dst + 6 src + 2 ethertype (0x8892) + 2 payload bytes
        let mut buf = vec![0u8; 16];
        buf[12] = 0x88;
        buf[13] = 0x92;
        assert!(is_profinet_frame(&buf));
    }

    #[test]
    fn accepts_vlan_tagged_profinet() {
        // 6 dst + 6 src + 2 (0x8100 VLAN) + 2 TCI + 2 ethertype (0x8892) + 2 payload
        let mut buf = vec![0u8; 20];
        buf[12] = 0x81;
        buf[13] = 0x00;
        buf[16] = 0x88;
        buf[17] = 0x92;
        assert!(is_profinet_frame(&buf));
    }

    #[test]
    fn rejects_non_profinet() {
        // IPv4 ethertype 0x0800
        let mut buf = vec![0u8; 16];
        buf[12] = 0x08;
        buf[13] = 0x00;
        assert!(!is_profinet_frame(&buf));
    }

    #[test]
    fn rejects_too_short() {
        let buf = vec![0u8; 10];
        assert!(!is_profinet_frame(&buf));
    }
}

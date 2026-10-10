use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

use nix::sys::socket::{send, MsgFlags};

use super::bpf::SockFilter;
use super::poll::wait_readable;
use super::transport::{EthTransport, TransportError, MAX_FRAME_LEN};
use super::EthProto;

impl From<nix::errno::Errno> for TransportError {
    fn from(e: nix::errno::Errno) -> Self {
        TransportError::Io(std::io::Error::from(e))
    }
}

/// Raw AF_PACKET socket bound to a named interface and one EtherType (see
/// [`EthProto`]), optionally joined to a multicast group, and filtered on that
/// EtherType at recv time ([`EthProto::accepts`]).
pub struct AfPacketTransport {
    fd: OwnedFd,
    proto: EthProto,
}

impl AfPacketTransport {
    /// Open a raw AF_PACKET socket on `ifname`, bound to EtherType 0x8892 and
    /// joined to the DCP multicast group (01:0e:cf:00:00:00); shorthand for
    /// [`open_with`](Self::open_with)`(ifname, EthProto::PROFINET)`.
    ///
    /// Returns `Err(TransportError::Io)` if the interface does not exist or the
    /// process lacks `CAP_NET_RAW`.
    pub fn open(ifname: &str) -> Result<Self, TransportError> {
        Self::open_with(ifname, EthProto::PROFINET)
    }

    /// Open a raw AF_PACKET socket on `ifname`, bound to `proto.ethertype` and
    /// joined to `proto.multicast` if any.
    ///
    /// Returns `Err(TransportError::Io)` if the interface does not exist or the
    /// process lacks `CAP_NET_RAW`.
    pub fn open_with(ifname: &str, proto: EthProto) -> Result<Self, TransportError> {
        let protocol = proto.ethertype.to_be() as i32;

        // Safety: `socket(2)` with valid, constant arguments; the returned fd is
        // immediately wrapped in an `OwnedFd`, which takes ownership and closes it
        // on drop.
        let raw_fd = unsafe {
            libc::socket(
                libc::AF_PACKET,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                protocol,
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
        sll.sll_protocol = proto.ethertype.to_be();
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

        if let Some(group) = proto.multicast {
            // Join the multicast group (e.g. DCP Identify/Hello, LLDP) so its frames
            // reach this socket even when the NIC does not pass all multicast by default.
            // Safety: `packet_mreq` is a plain-old-data struct for which an all-zero bit
            // pattern is a valid value; every field we rely on is explicitly set below.
            let mut mreq: libc::packet_mreq = unsafe { mem::zeroed() };
            mreq.mr_ifindex = ifindex as libc::c_int;
            mreq.mr_type = libc::PACKET_MR_MULTICAST as u16;
            mreq.mr_alen = 6;
            mreq.mr_address[..6].copy_from_slice(&group);

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
        }

        Ok(Self { fd, proto })
    }

    /// Attach a classic BPF program (see [`crate::eth::bpf`]) to the socket. Frames
    /// already queued before the call are still delivered.
    pub fn attach_filter(&self, prog: &[SockFilter]) -> Result<(), TransportError> {
        let len = u16::try_from(prog.len()).map_err(|_| {
            TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "BPF program too long",
            ))
        })?;
        // `SockFilter` is `repr(C)` with the exact field layout of `struct sock_filter`.
        let fprog = libc::sock_fprog {
            len,
            filter: prog.as_ptr() as *mut libc::sock_filter,
        };
        // Safety: `fprog` is fully initialized and points at `prog`, which outlives
        // the call (the kernel copies the program); `fd` is a valid, open socket.
        let ret = unsafe {
            libc::setsockopt(
                self.fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ATTACH_FILTER,
                &fprog as *const libc::sock_fprog as *const libc::c_void,
                mem::size_of::<libc::sock_fprog>() as libc::socklen_t,
            )
        };
        if ret < 0 {
            return Err(TransportError::Io(std::io::Error::last_os_error()));
        }
        Ok(())
    }
}

impl EthTransport for AfPacketTransport {
    fn send(&self, frame: &[u8]) -> Result<(), TransportError> {
        send(self.fd.as_raw_fd(), frame, MsgFlags::empty())?;
        Ok(())
    }

    /// Returns `Ok(Some(len))` only for frames this transport's [`EthProto`] accepts.
    /// Returns `Ok(None)` for any other frame, for our own looped-back frames, or if
    /// `timeout` elapses before a frame arrives.
    fn recv_into(
        &self,
        buf: &mut [u8],
        timeout: Option<Duration>,
    ) -> Result<Option<usize>, TransportError> {
        if buf.len() < MAX_FRAME_LEN {
            return Err(TransportError::BufferTooSmall);
        }
        if !wait_readable(self.fd.as_raw_fd(), timeout)? {
            return Ok(None);
        }
        // Safety: `buf` is a valid, writable slice of `buf.len()` bytes and
        // `from`/`from_len` a valid, fully-initialized `sockaddr_ll` plus its length,
        // all live for the duration of the call; `fd` is a valid, open socket.
        let mut from: libc::sockaddr_ll = unsafe { mem::zeroed() };
        let mut from_len = mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t;
        let n = unsafe {
            libc::recvfrom(
                self.fd.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                libc::MSG_TRUNC,
                &mut from as *mut libc::sockaddr_ll as *mut libc::sockaddr,
                &mut from_len,
            )
        };
        if n < 0 {
            return Err(TransportError::Io(std::io::Error::last_os_error()));
        }
        let n = n as usize;
        // Our own transmissions are looped back to every `AF_PACKET` socket on the
        // interface, including the one that sent them: drop them here so the cyclic
        // engine never sees its own provider frames.
        if from.sll_pkttype == libc::PACKET_OUTGOING {
            return Ok(None);
        }
        // With MSG_TRUNC the kernel reports the real length even when it exceeds
        // the buffer: the frame was cut and must not be handed on.
        if n > buf.len() {
            return Err(TransportError::FrameTooLong { len: n });
        }
        if self.proto.accepts(&buf[..n]) {
            Ok(Some(n))
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
    #[ignore = "requires CAP_NET_RAW + a real interface; run: cargo test -- --ignored"]
    fn attach_filter_on_loopback_succeeds() {
        let t = AfPacketTransport::open("lo").expect("open lo");
        t.attach_filter(&super::super::bpf::rt_filter()).unwrap();
    }
}

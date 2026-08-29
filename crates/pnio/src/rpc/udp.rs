//! UDP-backed `RpcTransport`, bound to `PNIO_UDP_PORT` in normal operation.

use std::net::{SocketAddr, UdpSocket};
use std::os::fd::{AsRawFd, RawFd};
use std::time::Duration;

use crate::eth::poll::wait_readable;

use super::transport::RpcTransport;
use super::RpcError;

/// A `UdpSocket`-backed `RpcTransport`. The same socket carries both the
/// request/response exchange and the outgoing `ApplicationReady` call (spec §7).
pub struct UdpRpcTransport {
    socket: UdpSocket,
}

impl UdpRpcTransport {
    /// Binds a blocking UDP socket to `addr` (typically `0.0.0.0:34964`, or
    /// `127.0.0.1:0` / `:0` in tests to get an ephemeral port).
    pub fn bind(addr: SocketAddr) -> Result<Self, RpcError> {
        let socket = UdpSocket::bind(addr)?;
        socket.set_nonblocking(false)?;
        Ok(Self { socket })
    }

    /// The address this socket is actually bound to (useful after binding to
    /// port 0 in tests).
    pub fn local_addr(&self) -> Result<SocketAddr, RpcError> {
        Ok(self.socket.local_addr()?)
    }
}

impl RpcTransport for UdpRpcTransport {
    fn send(&self, buf: &[u8], to: SocketAddr) -> Result<(), RpcError> {
        self.socket.send_to(buf, to)?;
        Ok(())
    }

    fn recv(&self, timeout: Option<Duration>) -> Result<Option<(Vec<u8>, SocketAddr)>, RpcError> {
        if !wait_readable(self.socket.as_raw_fd(), timeout)? {
            return Ok(None);
        }
        let mut buf = vec![0u8; 1500];
        let (n, from) = self.socket.recv_from(&mut buf)?;
        buf.truncate(n);
        Ok(Some((buf, from)))
    }

    fn raw_fd(&self) -> Option<RawFd> {
        Some(self.socket.as_raw_fd())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udp_loopback_roundtrip_and_timeout() {
        let a = UdpRpcTransport::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let b = UdpRpcTransport::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let to = b.local_addr().unwrap();
        a.send(&[0xde, 0xad], to).unwrap();
        let (bytes, from) = b.recv(Some(Duration::from_millis(500))).unwrap().unwrap();
        assert_eq!(bytes, vec![0xde, 0xad]);
        assert_eq!(from, a.local_addr().unwrap());
        assert_eq!(b.recv(Some(Duration::from_millis(20))).unwrap(), None);
        assert!(b.raw_fd().is_some());
    }
}

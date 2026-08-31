//! DCE-RPC datagram I/O abstraction (UDP port 34964, see `PNIO_UDP_PORT`).

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::os::fd::RawFd;
use std::sync::Mutex;
use std::time::Duration;

use super::RpcError;

/// DCE-RPC datagram transport: one `send`/`recv` pair per PDU, no framing beyond
/// what the DCE-RPC codec already provides.
pub trait RpcTransport: Send + Sync {
    /// Sends `buf` as a single datagram to `to`.
    fn send(&self, buf: &[u8], to: SocketAddr) -> Result<(), RpcError>;

    /// Receives the next datagram.
    ///
    /// Returns `Ok(None)` if `timeout` elapses before a datagram arrives (`None`
    /// blocks indefinitely). Distinct from `Err(_)`, which is a real I/O failure.
    fn recv(&self, timeout: Option<Duration>) -> Result<Option<(Vec<u8>, SocketAddr)>, RpcError>;

    /// The raw file descriptor backing this transport, when there is one, so a
    /// caller can multiplex several transports in a single `poll(2)` loop.
    ///
    /// Defaults to `None` for in-memory backends (e.g. `MockRpcTransport`).
    fn raw_fd(&self) -> Option<RawFd> {
        None
    }
}

/// In-memory transport for testing.
#[derive(Default)]
pub struct MockRpcTransport {
    tx: Mutex<Vec<(Vec<u8>, SocketAddr)>>,
    rx: Mutex<VecDeque<(Vec<u8>, SocketAddr)>>,
}

impl MockRpcTransport {
    /// An empty mock transport: nothing sent yet, nothing queued to receive.
    pub fn new() -> Self {
        Self::default()
    }
    /// Enqueues a datagram to be returned by `recv` (FIFO).
    pub fn push_rx(&self, bytes: Vec<u8>, from: SocketAddr) {
        self.rx.lock().unwrap().push_back((bytes, from));
    }
    /// All datagrams sent via `send`, in order.
    pub fn sent(&self) -> Vec<(Vec<u8>, SocketAddr)> {
        self.tx.lock().unwrap().clone()
    }
}

impl RpcTransport for MockRpcTransport {
    fn send(&self, buf: &[u8], to: SocketAddr) -> Result<(), RpcError> {
        self.tx.lock().unwrap().push((buf.to_vec(), to));
        Ok(())
    }
    fn recv(&self, _timeout: Option<Duration>) -> Result<Option<(Vec<u8>, SocketAddr)>, RpcError> {
        Ok(self.rx.lock().unwrap().pop_front())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_records_sent_and_replays_rx() {
        let t = MockRpcTransport::new();
        let a: SocketAddr = "172.16.2.100:54766".parse().unwrap();
        t.send(&[1, 2], a).unwrap();
        assert_eq!(t.sent(), vec![(vec![1, 2], a)]);
        t.push_rx(vec![9], a);
        assert_eq!(t.recv(None).unwrap(), Some((vec![9], a)));
        assert_eq!(t.recv(None).unwrap(), None);
        assert_eq!(t.raw_fd(), None);
    }
}

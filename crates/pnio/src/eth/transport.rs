use std::os::fd::RawFd;
use std::sync::Mutex;
use std::time::Duration;
use thiserror::Error;

/// Largest frame a transport must be able to hand back: 1500-byte MTU + 14-byte
/// header + 4-byte 802.1Q tag + 4-byte FCS.
pub const MAX_FRAME_LEN: usize = 1522;

/// Errors from a raw Ethernet transport's send/receive.
#[derive(Debug, Error)]
pub enum TransportError {
    /// The underlying socket/device failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// `recv_into` was given a buffer shorter than [`MAX_FRAME_LEN`] — a programming
    /// error on the caller's side, reported rather than risking a truncated frame.
    #[error("receive buffer shorter than {MAX_FRAME_LEN} bytes")]
    BufferTooSmall,
    /// A frame longer than the buffer arrived; it was discarded, never truncated.
    #[error("frame of {len} bytes does not fit the receive buffer")]
    FrameTooLong {
        /// The oversized frame's actual length in bytes.
        len: usize,
    },
}

/// Raw Ethernet frame I/O abstraction (L2 header included).
pub trait EthTransport: Send + Sync {
    /// Sends a complete Ethernet frame (L2 header included).
    fn send(&self, frame: &[u8]) -> Result<(), TransportError>;

    /// Receive the next frame into `buf` and return its length.
    ///
    /// `buf` must be at least [`MAX_FRAME_LEN`] bytes (`BufferTooSmall` otherwise). A
    /// frame that does not fit is an error (`FrameTooLong`), never a silent truncation.
    ///
    /// Returns `Ok(None)` in three legitimate, non-error cases:
    /// - the queue is empty (e.g. `MockTransport` with nothing pushed);
    /// - no frame arrived before `timeout` elapsed (`AfPacketTransport` honors
    ///   `timeout` via `poll(2)`; `None` blocks indefinitely);
    /// - the backend filters non-PROFINET traffic and the next frame on the wire
    ///   was not PROFINET (e.g. `AfPacketTransport`).
    ///
    /// A receive loop should treat `Ok(None)` as "nothing for me right now" and
    /// continue, distinct from `Err(_)` which is a real I/O failure.
    fn recv_into(
        &self,
        buf: &mut [u8],
        timeout: Option<Duration>,
    ) -> Result<Option<usize>, TransportError>;

    /// Allocating convenience over [`EthTransport::recv_into`]: same contract, the
    /// frame comes back as an owned `Vec`. Not for the RT path.
    fn recv(&self, timeout: Option<Duration>) -> Result<Option<Vec<u8>>, TransportError> {
        let mut buf = vec![0u8; MAX_FRAME_LEN];
        match self.recv_into(&mut buf, timeout)? {
            Some(n) => {
                buf.truncate(n);
                Ok(Some(buf))
            }
            None => Ok(None),
        }
    }

    /// The raw file descriptor backing this transport, when there is one, so a
    /// caller can multiplex several transports in a single `poll(2)` loop.
    ///
    /// Defaults to `None` for in-memory backends (e.g. `MockTransport`).
    fn raw_fd(&self) -> Option<RawFd> {
        None
    }
}

/// In-memory transport for testing.
#[derive(Default)]
pub struct MockTransport {
    tx: Mutex<Vec<Vec<u8>>>,
    rx: Mutex<std::collections::VecDeque<Vec<u8>>>,
}

impl MockTransport {
    /// An empty mock transport: nothing sent yet, nothing queued to receive.
    pub fn new() -> Self {
        Self::default()
    }
    /// Enqueues a frame to be returned by `recv` (FIFO).
    pub fn push_rx(&self, frame: Vec<u8>) {
        self.rx.lock().unwrap().push_back(frame);
    }
    /// All frames sent via `send`, in order.
    pub fn sent(&self) -> Vec<Vec<u8>> {
        self.tx.lock().unwrap().clone()
    }
}

impl EthTransport for MockTransport {
    fn send(&self, frame: &[u8]) -> Result<(), TransportError> {
        self.tx.lock().unwrap().push(frame.to_vec());
        Ok(())
    }
    fn recv_into(
        &self,
        buf: &mut [u8],
        _timeout: Option<Duration>,
    ) -> Result<Option<usize>, TransportError> {
        if buf.len() < MAX_FRAME_LEN {
            return Err(TransportError::BufferTooSmall);
        }
        let mut rx = self.rx.lock().unwrap();
        let Some(frame) = rx.front() else {
            return Ok(None);
        };
        if frame.len() > buf.len() {
            let len = frame.len();
            rx.pop_front();
            return Err(TransportError::FrameTooLong { len });
        }
        let frame = rx.pop_front().expect("front() was Some");
        buf[..frame.len()].copy_from_slice(&frame);
        Ok(Some(frame.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_records_sent_frames() {
        let t = MockTransport::new();
        t.send(&[1, 2, 3]).unwrap();
        t.send(&[4, 5]).unwrap();
        assert_eq!(t.sent(), vec![vec![1, 2, 3], vec![4, 5]]);
    }

    #[test]
    fn mock_returns_pushed_rx_in_order_then_none() {
        let t = MockTransport::new();
        t.push_rx(vec![9, 9]);
        assert_eq!(t.recv(None).unwrap(), Some(vec![9, 9]));
        assert_eq!(t.recv(None).unwrap(), None);
    }

    #[test]
    fn mock_recv_into_copies_the_frame_and_returns_its_length() {
        let t = MockTransport::new();
        t.push_rx(vec![1, 2, 3]);
        let mut buf = [0u8; MAX_FRAME_LEN];
        assert_eq!(t.recv_into(&mut buf, None).unwrap(), Some(3));
        assert_eq!(&buf[..3], &[1, 2, 3]);
        assert_eq!(t.recv_into(&mut buf, None).unwrap(), None);
    }

    #[test]
    fn mock_recv_into_rejects_a_short_buffer_without_consuming() {
        let t = MockTransport::new();
        t.push_rx(vec![1, 2, 3]);
        let mut small = [0u8; 16];
        assert!(matches!(
            t.recv_into(&mut small, None),
            Err(TransportError::BufferTooSmall)
        ));
        // The frame is still queued.
        let mut buf = [0u8; MAX_FRAME_LEN];
        assert_eq!(t.recv_into(&mut buf, None).unwrap(), Some(3));
    }

    #[test]
    fn mock_recv_into_reports_a_frame_longer_than_the_buffer() {
        let t = MockTransport::new();
        t.push_rx(vec![0u8; MAX_FRAME_LEN + 1]);
        let mut buf = [0u8; MAX_FRAME_LEN];
        assert!(matches!(
            t.recv_into(&mut buf, None),
            Err(TransportError::FrameTooLong { len }) if len == MAX_FRAME_LEN + 1
        ));
    }

    #[test]
    fn default_recv_returns_the_same_bytes_as_recv_into() {
        let t = MockTransport::new();
        t.push_rx(vec![7, 8, 9]);
        assert_eq!(t.recv(None).unwrap(), Some(vec![7, 8, 9]));
    }
}

//! Shared `poll(2)` timeout helper for the `AF_PACKET` and UDP RPC backends.

use std::os::fd::{BorrowedFd, RawFd};
use std::time::Duration;

use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags};

/// Waits for `fd` to become readable.
///
/// `timeout`:
/// - `None` blocks indefinitely (`poll` timeout `-1`);
/// - `Some(d)` waits at most `d`, rounding up to the millisecond.
///
/// Returns `Ok(true)` once `fd` is readable, `Ok(false)` if `timeout` elapsed first.
/// Retries transparently on `EINTR`.
pub(crate) fn wait_readable(fd: RawFd, timeout: Option<Duration>) -> std::io::Result<bool> {
    let timeout_ms: libc::c_int = match timeout {
        None => -1,
        Some(d) => {
            // Round up: any leftover sub-millisecond remainder still costs a whole
            // millisecond of `poll(2)` timeout, so `0 < d < 1ms` must not become 0
            // (which would mean "return immediately" instead of "wait a little").
            let ms = d.as_millis() + u128::from(d.subsec_nanos() % 1_000_000 != 0);
            libc::c_int::try_from(ms).unwrap_or(libc::c_int::MAX)
        }
    };

    loop {
        // Safety: `fd` is owned by the caller for the duration of this call; we only
        // borrow it to build the pollfd entry, we never close or duplicate it here.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let mut fds = [PollFd::new(&borrowed, PollFlags::POLLIN)];
        match poll(&mut fds, timeout_ms) {
            Ok(0) => return Ok(false),
            Ok(_) => return Ok(true),
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(std::io::Error::from(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;
    use std::os::fd::AsRawFd;
    use std::time::Instant;

    #[test]
    fn sub_millisecond_timeout_rounds_up_and_still_times_out() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let fd = sock.as_raw_fd();

        let start = Instant::now();
        let ready = wait_readable(fd, Some(Duration::from_micros(500))).unwrap();
        let elapsed = start.elapsed();

        assert!(!ready);
        assert!(
            elapsed >= Duration::from_millis(1),
            "expected at least 1ms, got {elapsed:?}"
        );
    }

    #[test]
    fn zero_timeout_returns_immediately() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let fd = sock.as_raw_fd();

        let ready = wait_readable(fd, Some(Duration::ZERO)).unwrap();
        assert!(!ready);
    }
}

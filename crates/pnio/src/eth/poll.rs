//! Shared `poll(2)` timeout helper for the `AF_PACKET` and UDP RPC backends.

use std::os::fd::{BorrowedFd, RawFd};
use std::time::Duration;

use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

/// Waits for `fd` to become readable.
///
/// `timeout`:
/// - `None` blocks indefinitely (`poll` timeout `-1`);
/// - `Some(d)` waits at most `d`, rounding up to the millisecond.
///
/// Returns `Ok(true)` once `fd` is readable, `Ok(false)` if `timeout` elapsed first.
/// Retries transparently on `EINTR`.
pub(crate) fn wait_readable(fd: RawFd, timeout: Option<Duration>) -> std::io::Result<bool> {
    let timeout_ms = poll_timeout(timeout);

    loop {
        // Safety: `fd` is owned by the caller for the duration of this call; we only
        // borrow it to build the pollfd entry, we never close or duplicate it here.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];
        match poll(&mut fds, timeout_ms) {
            Ok(0) => return Ok(false),
            Ok(_) => return Ok(true),
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(std::io::Error::from(e)),
        }
    }
}

/// Waits for any of `fds` to become readable.
///
/// Same `timeout` semantics as [`wait_readable`]. Returns `Ok(true)` once at least
/// one fd is readable, `Ok(false)` if `timeout` elapsed first. Retries transparently
/// on `EINTR`.
pub(crate) fn wait_any_readable(fds: &[RawFd], timeout: Option<Duration>) -> std::io::Result<bool> {
    let timeout_ms = poll_timeout(timeout);

    loop {
        // Safety: each `fd` is owned by the caller for the duration of this call; we
        // only borrow it to build the pollfd entry, we never close or duplicate it here.
        let borrowed: Vec<BorrowedFd> = fds
            .iter()
            .map(|&fd| unsafe { BorrowedFd::borrow_raw(fd) })
            .collect();
        let mut pollfds: Vec<PollFd> = borrowed
            .iter()
            .map(|&fd| PollFd::new(fd, PollFlags::POLLIN))
            .collect();
        match poll(&mut pollfds, timeout_ms) {
            Ok(0) => return Ok(false),
            Ok(_) => return Ok(true),
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(std::io::Error::from(e)),
        }
    }
}

/// Maximum number of fds [`poll_readable_into`] can watch in one call, and so the
/// size of the stack array it builds its `pollfd` set in — no allocation, so the
/// RT thread can call it every cycle.
pub(crate) const MAX_POLL_FDS: usize = 4;

/// Waits for any of `fds` to become readable and reports *which* ones are.
///
/// Same `timeout` semantics as [`wait_readable`]. Fills `ready[i]` for every
/// `fds[i]` and returns how many are set. On timeout every `ready[i]` is `false`
/// and the result is `0`. Retries transparently on `EINTR`.
///
/// Allocation-free: the `pollfd` set is built in a fixed-size stack array, hence
/// the `fds.len() <= MAX_POLL_FDS` limit. Error conditions (`POLLERR`, `POLLHUP`,
/// `POLLNVAL`) count as readable so the caller reaches the failing `read`/`recv`
/// and sees the error, rather than spinning on a `poll` that never yields
/// `POLLIN`.
///
/// Panics if `fds` is empty, longer than [`MAX_POLL_FDS`], or longer than `ready`.
pub(crate) fn poll_readable_into(
    fds: &[RawFd],
    ready: &mut [bool],
    timeout: Option<Duration>,
) -> std::io::Result<usize> {
    assert!(!fds.is_empty(), "poll_readable_into: no fds");
    assert!(
        fds.len() <= MAX_POLL_FDS,
        "poll_readable_into: at most {MAX_POLL_FDS} fds"
    );
    assert!(
        ready.len() >= fds.len(),
        "poll_readable_into: ready too short"
    );

    let timeout_ms = poll_timeout(timeout);

    loop {
        for r in ready[..fds.len()].iter_mut() {
            *r = false;
        }
        // Safety: each `fd` is owned by the caller for the duration of this call; we
        // only borrow it to build the pollfd entry, we never close or duplicate it here.
        // Unused slots repeat `fds[0]`; they are never passed to `poll`.
        let borrowed: [BorrowedFd; MAX_POLL_FDS] = std::array::from_fn(|i| unsafe {
            BorrowedFd::borrow_raw(if i < fds.len() { fds[i] } else { fds[0] })
        });
        let mut pollfds: [PollFd; MAX_POLL_FDS] =
            std::array::from_fn(|i| PollFd::new(borrowed[i], PollFlags::POLLIN));
        match poll(&mut pollfds[..fds.len()], timeout_ms) {
            Ok(0) => return Ok(0),
            Ok(_) => {
                let mut n = 0;
                for (i, pfd) in pollfds[..fds.len()].iter().enumerate() {
                    if pfd.revents().is_some_and(|r| !r.is_empty()) {
                        ready[i] = true;
                        n += 1;
                    }
                }
                return Ok(n);
            }
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(std::io::Error::from(e)),
        }
    }
}

/// `poll(2)` timeout for `timeout`: `None` blocks; a duration is rounded **up** to whole
/// milliseconds — any leftover sub-millisecond remainder still costs a whole millisecond,
/// so `0 < d < 1 ms` must not become 0 (which would mean "return immediately" instead of
/// "wait a little"). Durations beyond `PollTimeout`'s range saturate to its maximum.
fn poll_timeout(timeout: Option<Duration>) -> PollTimeout {
    match timeout {
        None => PollTimeout::NONE,
        Some(d) => {
            let ms = d.as_millis() + u128::from(d.subsec_nanos() % 1_000_000 != 0);
            u16::try_from(ms)
                .map(PollTimeout::from)
                .unwrap_or(PollTimeout::MAX)
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

    #[test]
    fn wait_any_readable_finds_the_ready_fd_among_several() {
        let idle = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender
            .send_to(b"hi", receiver.local_addr().unwrap())
            .unwrap();

        let fds = [idle.as_raw_fd(), receiver.as_raw_fd()];
        let ready = wait_any_readable(&fds, Some(Duration::from_millis(200))).unwrap();
        assert!(ready);
    }

    #[test]
    fn poll_readable_into_marks_only_the_ready_fd() {
        let idle = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender
            .send_to(b"hi", receiver.local_addr().unwrap())
            .unwrap();

        let fds = [receiver.as_raw_fd(), idle.as_raw_fd()];
        let mut ready = [false; 2];
        let n = poll_readable_into(&fds, &mut ready, Some(Duration::from_millis(200))).unwrap();
        assert_eq!(n, 1);
        assert_eq!(ready, [true, false]);
    }

    #[test]
    fn poll_readable_into_times_out_with_nothing_ready() {
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        let fds = [a.as_raw_fd(), b.as_raw_fd()];
        let mut ready = [true; 2];
        let n = poll_readable_into(&fds, &mut ready, Some(Duration::ZERO)).unwrap();
        assert_eq!(n, 0);
        assert_eq!(ready, [false, false]);
    }

    #[test]
    fn wait_any_readable_times_out_when_none_ready() {
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        let fds = [a.as_raw_fd(), b.as_raw_fd()];
        let ready = wait_any_readable(&fds, Some(Duration::ZERO)).unwrap();
        assert!(!ready);
    }
}

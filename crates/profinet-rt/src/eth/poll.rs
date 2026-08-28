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
            let ms = d.as_millis();
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

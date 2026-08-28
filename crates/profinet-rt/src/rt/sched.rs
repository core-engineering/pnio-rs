//! Scheduling helpers for real-time threads: `SCHED_FIFO`, CPU affinity, memory
//! locking and stack pre-faulting.
//!
//! Everything goes through raw `libc::syscall`: musl stubs `sched_setscheduler` with
//! `ENOSYS` (Plan 4 lesson), and keeping affinity on the same path avoids the same
//! surprise resurfacing. `mlockall` is a real symbol on both libcs. All helpers act
//! on the *calling* thread (pid 0) — call them from the thread they should affect.

use std::io;
use std::mem;

/// Bytes of the calling thread's stack touched by [`prefault_stack`], so the RT
/// loop never takes a page fault growing its stack after `mlockall`.
pub const STACK_PREFAULT_BYTES: usize = 256 * 1024;

/// Switch the calling thread to `SCHED_FIFO` at `priority` (1..=99).
pub fn set_fifo(priority: u8) -> io::Result<()> {
    // Safety: `sched_param` is plain-old-data for which an all-zero bit pattern is
    // valid; only `sched_priority` is meaningful to `sched_setscheduler` (musl
    // carries extra reserved fields glibc doesn't, both fine left zeroed).
    let mut param: libc::sched_param = unsafe { mem::zeroed() };
    param.sched_priority = priority as libc::c_int;
    // Safety: raw `sched_setscheduler(2)`; `param` is fully initialized and live for
    // the call, passed as a valid pointer; pid 0 means the calling thread.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_sched_setscheduler,
            0 as libc::pid_t,
            libc::SCHED_FIFO,
            &param as *const libc::sched_param,
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Build the affinity bitmap for `cpus`.
fn cpu_set(cpus: &[usize]) -> libc::cpu_set_t {
    // Safety: `cpu_set_t` is a plain-old-data bitmap for which an all-zero bit
    // pattern is a valid (empty) set; `CPU_SET` only writes inside it.
    unsafe {
        let mut set: libc::cpu_set_t = mem::zeroed();
        for &cpu in cpus {
            libc::CPU_SET(cpu, &mut set);
        }
        set
    }
}

/// Restrict the calling thread to `cpus` (non-empty, each `< 8 * size_of::<cpu_set_t>()`
/// — the bitmap's width — so [`libc::CPU_SET`] never writes out of bounds).
pub fn set_affinity(cpus: &[usize]) -> io::Result<()> {
    if cpus.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty CPU list",
        ));
    }
    let max_cpu = 8 * mem::size_of::<libc::cpu_set_t>();
    if let Some(&cpu) = cpus.iter().find(|&&cpu| cpu >= max_cpu) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("cpu {cpu} out of range (max {max_cpu})"),
        ));
    }
    let set = cpu_set(cpus);
    // Safety: raw `sched_setaffinity(2)`; `set` is fully initialized and live for the
    // call, passed as a valid pointer with its exact size; pid 0 = calling thread.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_sched_setaffinity,
            0 as libc::pid_t,
            mem::size_of::<libc::cpu_set_t>() as libc::size_t,
            &set as *const libc::cpu_set_t,
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Lock every current and future page of the process in RAM
/// (`mlockall(MCL_CURRENT | MCL_FUTURE)`). Process-wide, unlike the other helpers.
pub fn lock_memory() -> io::Result<()> {
    // Safety: `mlockall(2)` with constant flags, no pointers.
    let ret = unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Touch [`STACK_PREFAULT_BYTES`] of the calling thread's stack, one byte per page,
/// so the pages exist (and, after [`lock_memory`], stay resident) before the RT loop
/// starts. `black_box` keeps the writes from being optimized away.
pub fn prefault_stack() {
    let mut buf = [0u8; STACK_PREFAULT_BYTES];
    let mut i = 0;
    while i < buf.len() {
        buf[i] = 1;
        i += 4096;
    }
    std::hint::black_box(&buf);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_set_holds_exactly_the_requested_cpus() {
        let set = cpu_set(&[0, 2]);
        // Safety: `CPU_ISSET` only reads the bitmap.
        unsafe {
            assert!(libc::CPU_ISSET(0, &set));
            assert!(!libc::CPU_ISSET(1, &set));
            assert!(libc::CPU_ISSET(2, &set));
            assert!(!libc::CPU_ISSET(3, &set));
        }
    }

    #[test]
    fn set_affinity_rejects_an_empty_list() {
        assert_eq!(
            set_affinity(&[]).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn set_affinity_rejects_a_cpu_beyond_the_bitmap_width() {
        let max_cpu = 8 * mem::size_of::<libc::cpu_set_t>();
        assert_eq!(
            set_affinity(&[max_cpu]).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        // A valid CPU earlier in the slice does not save an out-of-range one later.
        assert_eq!(
            set_affinity(&[0, max_cpu]).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn set_affinity_to_the_current_cpu_succeeds() {
        // The CPU we are on is always allowed, even under a restricted cpuset.
        // Safety: `sched_getcpu(3)` takes no pointers.
        let cpu = unsafe { libc::sched_getcpu() };
        assert!(cpu >= 0);
        set_affinity(&[cpu as usize]).unwrap();
    }

    #[test]
    fn prefault_stack_returns() {
        prefault_stack();
    }

    /// Needs CAP_SYS_NICE (or an rtprio rlimit): run by hand on the edge.
    #[test]
    #[ignore]
    fn set_fifo_applies() {
        set_fifo(10).unwrap();
    }

    /// Needs CAP_IPC_LOCK or a large RLIMIT_MEMLOCK: run by hand on the edge.
    #[test]
    #[ignore]
    fn lock_memory_applies() {
        lock_memory().unwrap();
    }
}

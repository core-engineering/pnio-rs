# Plan 7 — 1 ms determinism Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Hold a 1 ms PROFINET update time against the S7-1500 on the PREEMPT_RT edge, at idle and under load, with a zero-allocation RT path, per-socket BPF filters, locked memory, latency histograms, a PASS/FAIL verdict in `rt_bringup`, and reproducible edge tuning + campaign scripts.

**Architecture:** The crate's RT thread (`rt::runner`) keeps its Plan 4 structure; this plan removes its last allocation (`recv_into` into a fixed buffer), moves the raw scheduling syscalls into a public `rt::sched` module (adds `mlockall`), attaches a classic BPF filter to each `AF_PACKET` socket (`eth::bpf`), and adds three 1 µs-bin histograms to `RtStats` (`rt::hist`). `examples/rt_bringup.rs` gains CSV output and a threshold verdict. `bench/` holds the edge tuning script, systemd unit, load and campaign scripts.

**Tech Stack:** Rust 1.96 (workspace toolchain), `libc` (raw syscalls, `sock_filter`), `nix` (existing), no new dependencies. Bash scripts on Debian 13 (`ethtool`, `chrt`, `rt-tests`, `stress-ng`, `tcpdump`). Bench: edge `lab-server` 192.168.1.21 (`eno2` 172.16.2.10 ↔ CPU 1515-2 PN 172.16.2.100), musl build for the edge binary.

**Spec:** `docs/design/2026-08-28-profinet-rt-rt-1ms-design.md` (read it; sections referenced as spec §N below).

## Global Constraints

- `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all` green after every task (CI runs exactly these three). rustfmt `max_width = 100`.
- **No new dependencies** (spec §15). `libc` and `nix` only.
- **RT loop rules** (spec §4): no allocation, no blocking lock, no logging, and no syscall other than `poll`, `read(timerfd)`, `recvfrom`, `sendto`, `write(eventfd)`, `clock_gettime`.
- Scheduling and affinity go through **raw `libc::syscall`** (`SYS_sched_setscheduler`, `SYS_sched_setaffinity`): musl stubs the wrappers with ENOSYS (Plan 4 lesson). Every `unsafe` block carries a `// Safety:` comment (existing convention).
- Tasks touching syscalls also build for musl: `cargo build --release --target x86_64-unknown-linux-musl --example rt_bringup` (target installed locally; the edge runs glibc 2.41 < host 2.43).
- Project language is English (code, docs, commits). Commit messages: `feat(scope): …`, `fix(scope): …`, `docs: …`, `test(scope): …`.
- Implementers **commit but never push** (GCM crashes in linked worktrees); the controller pushes.
- Cargo needs `. "$HOME/.cargo/env" &&` in front of every command in this environment.
- Thresholds (spec §1): `missed_ticks == 0`, `watchdog_expirations == 0`, tick lateness p99.99 < 100 µs and max < 300 µs, RX interval max < 1.5 ms. Defaults in `rt_bringup`.
- Edge facts (do not change): `eno2` = 172.16.2.10/24 is also the TIA NAT gateway's PLC leg — never re-address it; CPU 3 is isolated (`isolcpus=domain,managed_irq,3 nohz_full=3 rcu_nocbs=3 irqaffinity=0-2 intel_idle.max_cstate=1 processor.max_cstate=1 nosoftlockup`), kernel `6.12.105+deb13-rt-amd64`.

---

## File map

| File | Responsibility | Task |
|---|---|---|
| `crates/profinet-rt/src/eth/transport.rs` | `EthTransport::recv_into` (required), `recv` default, `TransportError::{BufferTooSmall, FrameTooLong}`, `MAX_FRAME_LEN`, mock impl | 1 |
| `crates/profinet-rt/src/eth/afpacket.rs` | native `recv_into`, `attach_filter` | 1, 3 |
| `crates/profinet-rt/src/rt/mod.rs` | `RtError::Transport`, exports `sched`, `hist` | 1, 2, 4 |
| `crates/profinet-rt/src/rt/runner.rs` | fixed RX buffer, `sched::` calls, `lock_memory`, filter at spawn, histogram recording | 1, 2, 3, 4 |
| `crates/profinet-rt/src/rt/sched.rs` | `set_fifo`, `set_affinity`, `lock_memory`, `prefault_stack` | 2 |
| `crates/profinet-rt/src/device/mod.rs` | `RtOptions.lock_memory` → `RtConfig` | 2 |
| `crates/profinet-rt/src/eth/bpf.rs` | `SockFilter`, `frame_id_filter`, `rt_filter`, `acyclic_filter` | 3 |
| `crates/profinet-rt/src/eth/mod.rs` | export `bpf` | 3 |
| `crates/profinet-rt/src/rt/hist.rs` | `Histogram`, `HistSnapshot` | 4 |
| `crates/profinet-rt/src/rt/engine.rs` | histograms + maxima in `RtStats` / `StatsSnapshot` | 4 |
| `crates/profinet-rt/examples/rt_bringup.rs` | flags, CSV, verdict, 1 ms app loop, acyclic filter | 5 |
| `bench/edge-rt-tune.sh`, `bench/profinet-rt-tune.service`, `bench/load.sh`, `bench/campaign.sh`, `bench/README.md` | edge tuning and campaign | 6 |
| `docs/bench-pnet-device.md` §6e, `README.md`, `FOLLOWUPS.md` | report | 7 |

---

### Task 1: `recv_into` — zero-allocation receive path

**Files:**
- Modify: `crates/profinet-rt/src/eth/transport.rs`
- Modify: `crates/profinet-rt/src/eth/afpacket.rs:126-175` (the `EthTransport` impl)
- Modify: `crates/profinet-rt/src/eth/mod.rs` (export `MAX_FRAME_LEN`)
- Modify: `crates/profinet-rt/src/rt/mod.rs` (`RtError::Transport`)
- Modify: `crates/profinet-rt/src/rt/runner.rs` (module doc, `spawn`, `run_loop`, `drain_rx`, test `SharedMock`)

**Interfaces:**
- Produces: `EthTransport::recv_into(&self, buf: &mut [u8], timeout: Option<Duration>) -> Result<Option<usize>, TransportError>` (required); `EthTransport::recv` (default, allocates, unchanged signature); `eth::MAX_FRAME_LEN: usize = 1522`; `TransportError::BufferTooSmall`, `TransportError::FrameTooLong { len: usize }`; `RtError::Transport(TransportError)`.
- Consumers: Task 3 (`attach_filter` lives next to `recv_into`), Task 4 (records inside `drain_rx`).

- [ ] **Step 1: Write the failing tests in `transport.rs`**

Append to the `tests` module of `crates/profinet-rt/src/eth/transport.rs`:

```rust
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
```

- [ ] **Step 2: Run to verify they fail**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt eth::transport 2>&1 | tail -5`
Expected: compile error — `recv_into` and `MAX_FRAME_LEN` do not exist.

- [ ] **Step 3: Implement the trait change and the mock**

Replace the top of `transport.rs` (error enum and trait) with:

```rust
use std::os::fd::RawFd;
use std::sync::Mutex;
use std::time::Duration;
use thiserror::Error;

/// Largest frame a transport must be able to hand back: 1500-byte MTU + 14-byte
/// header + 4-byte 802.1Q tag + 4-byte FCS.
pub const MAX_FRAME_LEN: usize = 1522;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// `recv_into` was given a buffer shorter than [`MAX_FRAME_LEN`] — a programming
    /// error on the caller's side, reported rather than risking a truncated frame.
    #[error("receive buffer shorter than {MAX_FRAME_LEN} bytes")]
    BufferTooSmall,
    /// A frame longer than the buffer arrived; it was discarded, never truncated.
    #[error("frame of {len} bytes does not fit the receive buffer")]
    FrameTooLong { len: usize },
}

/// Raw Ethernet frame I/O abstraction (L2 header included).
pub trait EthTransport: Send + Sync {
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
```

Replace the `MockTransport` impl:

```rust
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
```

In `eth/mod.rs`: `pub use transport::{EthTransport, MockTransport, TransportError, MAX_FRAME_LEN};`.

- [ ] **Step 4: Implement the native `recv_into` in `afpacket.rs`**

Replace the `recv` method of `impl EthTransport for AfPacketTransport` with:

```rust
    /// Returns `Ok(Some(len))` only for PROFINET frames (untagged or VLAN-tagged).
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
        if is_profinet_frame(&buf[..n]) {
            Ok(Some(n))
        } else {
            Ok(None)
        }
    }
```

Add `MAX_FRAME_LEN` to the `use super::transport::{...}` line. Delete the old `recv` (the trait default now covers it). Keep the `open_loopback_succeeds` / `open_unknown_interface_errors` tests as they are.

- [ ] **Step 5: Update `rt/mod.rs` and `runner.rs`**

`rt/mod.rs` — add a variant to `RtError` (after `Io`):

```rust
    #[error(transparent)]
    Transport(#[from] crate::eth::TransportError),
```

`runner.rs`:
1. Module doc: delete the paragraph starting "The one allocation left in the cycle is the `Vec`…" and replace with: `//! Nothing is allocated once the loop runs: the RX buffer ([`MAX_FRAME_LEN`] bytes), the TX frame, the input snapshot, the poll set and the event queue capacity are all set up before it starts.`
2. `use crate::eth::{EthTransport, MacAddr, MAX_FRAME_LEN};` (drop `TransportError` from that import).
3. In `spawn`, replace the `map_err(|e| match e { TransportError::Io(e) => RtError::Io(e) })?` with a plain `?` (the `From` impl handles it).
4. In `run_loop`, after `let mut snapshot = …`, add `let mut rx_buf = [0u8; MAX_FRAME_LEN];` and pass `&mut rx_buf` to `drain_rx` (new first-after-transport parameter).
5. In `drain_rx`, add the parameter `rx_buf: &mut [u8; MAX_FRAME_LEN]` after `transport`, and replace the `match transport.recv(Some(Duration::ZERO))` block with:

```rust
        let got_frame = match transport.recv_into(rx_buf, Some(Duration::ZERO)) {
            Ok(None) => false,
            Ok(Some(n)) => {
                *processed_frame = true;
                let now = Instant::now();
                if let RxVerdict::Accepted { .. } = engine.on_frame(&rx_buf[..n], now) {
                    let validity = Validity {
                        provider_run: engine.provider_run(),
                        primary: engine.primary(),
                        watchdog: WatchdogState::Ok,
                        last_rx_age: Some(Duration::ZERO),
                        cycle: ticks,
                    };
                    if image.rt_publish(engine.rx_csdu(), validity) {
                        *pending = None;
                    } else {
                        stats
                            .output_publish_deferred
                            .fetch_add(1, Ordering::Relaxed);
                        *pending = Some(Pending::Data(validity));
                    }
                }
                true
            }
            // An oversized frame is not ours to consume and not a socket failure:
            // count it and keep draining.
            Err(TransportError::FrameTooLong { .. }) => {
                stats.rx_invalid.fetch_add(1, Ordering::Relaxed);
                *processed_frame = true;
                true
            }
            Err(e) => {
                shared.push_event(RtEvent::SocketError(format!("recv: {e}")));
                return false;
            }
        };
```
(`TransportError` is then still needed in the import: keep `use crate::eth::{EthTransport, MacAddr, TransportError, MAX_FRAME_LEN};`.)
6. In the runner tests, the `SharedMock(Arc<MockTransport>)` wrapper implements `EthTransport` by forwarding `send`/`recv`: change it to forward `recv_into` instead (`self.0.recv_into(buf, timeout)`).

- [ ] **Step 6: Run the whole suite, fmt, clippy**

Run: `. "$HOME/.cargo/env" && cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: all `ok`; the 4 new transport tests pass; runner and device tests unchanged and green.

- [ ] **Step 7: Commit**

```bash
git add -A crates/profinet-rt/src
git commit -m "feat(eth): recv_into on EthTransport — the RT loop receives into a fixed buffer, no allocation per frame"
```

---

### Task 2: `rt::sched` — public scheduling helpers, `mlockall`, `RtOptions.lock_memory`

**Files:**
- Create: `crates/profinet-rt/src/rt/sched.rs`
- Modify: `crates/profinet-rt/src/rt/mod.rs` (export), `crates/profinet-rt/src/rt/runner.rs` (delete private helpers, new field, setup order), `crates/profinet-rt/src/device/mod.rs` (`RtOptions`, `start_runner`, two tests), `crates/profinet-rt/examples/rt_bringup.rs` (struct literal only — flags come in Task 5)

**Interfaces:**
- Produces: `rt::sched::{set_fifo(priority: u8) -> io::Result<()>, set_affinity(cpus: &[usize]) -> io::Result<()>, lock_memory() -> io::Result<()>, prefault_stack()}`; `RtConfig.lock_memory: bool`; `RtOptions.lock_memory: bool`.
- Consumes: nothing new.

- [ ] **Step 1: Write the failing tests**

Create `crates/profinet-rt/src/rt/sched.rs` with only the test module first:

```rust
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
```

Add `#[cfg(target_os = "linux")] pub mod sched;` to `rt/mod.rs` (next to `runner`).

- [ ] **Step 2: Run to verify they fail**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt rt::sched 2>&1 | tail -5`
Expected: compile error — `cpu_set`, `set_affinity`, … not found.

- [ ] **Step 3: Implement `sched.rs`**

Above the test module:

```rust
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

/// Restrict the calling thread to `cpus` (non-empty).
pub fn set_affinity(cpus: &[usize]) -> io::Result<()> {
    if cpus.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty CPU list"));
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
```

- [ ] **Step 4: Wire it into `runner.rs`**

1. Delete the private `set_fifo_priority` and `pin_to_cpu` functions (and the now-unused `use std::mem;` if nothing else uses it — check).
2. Add `use super::sched;`.
3. `RtConfig` gains, after `rt_priority`:
```rust
    /// Lock the process memory (`mlockall`) and pre-fault the RT stack before the loop.
    pub lock_memory: bool,
```
4. In `run_loop`, destructure `lock_memory` too and replace the setup block with (order: affinity → FIFO → lock → prefault):

```rust
    // --- setup: allocation, formatting and warnings all happen before the loop ---
    if let Some(cpu) = cpu_pin {
        if let Err(e) = sched::set_affinity(&[cpu]) {
            shared.push_event(RtEvent::SchedWarning(format!("CPU pin {cpu}: {e}")));
        }
    }
    if let Some(priority) = rt_priority {
        if let Err(e) = sched::set_fifo(priority) {
            shared.push_event(RtEvent::SchedWarning(format!(
                "SCHED_FIFO priority {priority}: {e}"
            )));
        }
    }
    if lock_memory {
        match sched::lock_memory() {
            Ok(()) => sched::prefault_stack(),
            Err(e) => shared.push_event(RtEvent::SchedWarning(format!("mlockall: {e}"))),
        }
    }
```
5. Runner tests: add `lock_memory: false,` to the `RtConfig` literal in `cfg()`.

- [ ] **Step 5: Wire it into `device/mod.rs` and the example**

`RtOptions` gains:
```rust
    /// Lock process memory and pre-fault the RT stack (`mlockall`); needs
    /// `CAP_IPC_LOCK` or a sufficient `RLIMIT_MEMLOCK`, otherwise a `SchedWarning`.
    pub lock_memory: bool,
```
`start_runner`: `lock_memory: rt.lock_memory,` in the `RtConfig` literal. The two device tests that build `RtOptions { iface: "mock".into(), cpu_pin: None, rt_priority: None }` get `lock_memory: false,`. `examples/rt_bringup.rs`: `lock_memory: false,` in its `RtOptions` literal (Task 5 replaces it with the flag).

- [ ] **Step 6: Run everything, including the musl build**

Run: `. "$HOME/.cargo/env" && cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all 2>&1 | grep -E "^test result|FAILED|panicked" && cargo build -q --release --target x86_64-unknown-linux-musl --example rt_bringup && echo MUSL_OK`
Expected: all green, 4 new sched tests pass (2 ignored), `MUSL_OK`.

- [ ] **Step 7: Commit**

```bash
git add -A crates/profinet-rt
git commit -m "feat(rt): public rt::sched (FIFO, affinity, mlockall, stack pre-fault) and RtOptions.lock_memory"
```

---

### Task 3: `eth::bpf` — per-socket classic BPF filters

**Files:**
- Create: `crates/profinet-rt/src/eth/bpf.rs`
- Modify: `crates/profinet-rt/src/eth/mod.rs` (export), `crates/profinet-rt/src/eth/afpacket.rs` (`attach_filter`), `crates/profinet-rt/src/rt/runner.rs` (`spawn` attaches `rt_filter`)

**Interfaces:**
- Produces: `eth::bpf::{SockFilter, frame_id_filter(lo: u16, hi: u16) -> Vec<SockFilter>, rt_filter() -> Vec<SockFilter>, acyclic_filter() -> Vec<SockFilter>}`; `AfPacketTransport::attach_filter(&self, prog: &[SockFilter]) -> Result<(), TransportError>`.
- Consumes: `testutil::{golden, golden_rt}` (goldens: `rtc_cpu_8001`, `rtc_dev_8000` are VLAN-tagged RTC1 frames — FrameID at offset 18; `ident_ok_pnet` is an untagged DCP Identify response, FrameID `0xFEFF` at offset 14).

- [ ] **Step 1: Write the failing tests**

Create `bpf.rs` with the test module (implementation comes in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{golden, golden_rt};

    /// Just enough classic BPF to run our own programs: LD_H_ABS, LDX_IMM, JA, JEQ,
    /// JGE, JGT, LD_H_IND, RET. Returns the accepted length (0 = rejected).
    fn run(prog: &[SockFilter], pkt: &[u8]) -> u32 {
        let ldh = |off: usize| -> Option<u32> {
            pkt.get(off..off + 2)
                .map(|b| u32::from(u16::from_be_bytes([b[0], b[1]])))
        };
        let (mut a, mut x, mut pc) = (0u32, 0u32, 0usize);
        loop {
            let i = prog[pc];
            pc += 1;
            match i.code {
                LD_H_ABS => match ldh(i.k as usize) {
                    Some(v) => a = v,
                    None => return 0,
                },
                LD_H_IND => match ldh((x + i.k) as usize) {
                    Some(v) => a = v,
                    None => return 0,
                },
                LDX_IMM => x = i.k,
                JA => pc += i.k as usize,
                JEQ => pc += if a == i.k { i.jt } else { i.jf } as usize,
                JGE => pc += if a >= i.k { i.jt } else { i.jf } as usize,
                JGT => pc += if a > i.k { i.jt } else { i.jf } as usize,
                RET => return i.k,
                other => panic!("opcode {other:#x} not in the test interpreter"),
            }
        }
    }

    fn untag(tagged: &[u8]) -> Vec<u8> {
        let mut f = tagged[..12].to_vec();
        f.extend_from_slice(&tagged[16..]);
        f
    }

    fn tag(untagged: &[u8]) -> Vec<u8> {
        let mut f = untagged[..12].to_vec();
        f.extend_from_slice(&[0x81, 0x00, 0xc0, 0x00]);
        f.extend_from_slice(&untagged[12..]);
        f
    }

    #[test]
    fn frame_id_filter_has_the_documented_shape() {
        let p = frame_id_filter(0x8000, 0xBFFF);
        assert_eq!(p.len(), 13);
        assert_eq!(p[0], SockFilter { code: LD_H_ABS, jt: 0, jf: 0, k: 12 });
        assert_eq!(p[1], SockFilter { code: JEQ, jt: 0, jf: 2, k: 0x8892 });
        assert_eq!(p[2], SockFilter { code: LDX_IMM, jt: 0, jf: 0, k: 14 });
        assert_eq!(p[3], SockFilter { code: JA, jt: 0, jf: 0, k: 4 });
        assert_eq!(p[4], SockFilter { code: JEQ, jt: 0, jf: 6, k: 0x8100 });
        assert_eq!(p[5], SockFilter { code: LD_H_ABS, jt: 0, jf: 0, k: 16 });
        assert_eq!(p[6], SockFilter { code: JEQ, jt: 0, jf: 4, k: 0x8892 });
        assert_eq!(p[7], SockFilter { code: LDX_IMM, jt: 0, jf: 0, k: 18 });
        assert_eq!(p[8], SockFilter { code: LD_H_IND, jt: 0, jf: 0, k: 0 });
        assert_eq!(p[9], SockFilter { code: JGE, jt: 0, jf: 1, k: 0x8000 });
        assert_eq!(p[10], SockFilter { code: JGT, jt: 0, jf: 1, k: 0xBFFF });
        assert_eq!(p[11], SockFilter { code: RET, jt: 0, jf: 0, k: 0 });
        assert_eq!(p[12], SockFilter { code: RET, jt: 0, jf: 0, k: 0xFFFF });
    }

    #[test]
    fn rt_filter_accepts_rtc1_frames_tagged_or_not_and_rejects_the_rest() {
        let rt = rt_filter();
        let cpu = golden_rt("rtc_cpu_8001"); // tagged, FrameID 0x8001
        assert_eq!(run(&rt, &cpu), 0xFFFF);
        assert_eq!(run(&rt, &untag(&cpu)), 0xFFFF);
        let dev = golden_rt("rtc_dev_8000");
        assert_eq!(run(&rt, &dev), 0xFFFF);
        let dcp = golden("ident_ok_pnet"); // untagged, FrameID 0xFEFF
        assert_eq!(run(&rt, &dcp), 0);
        assert_eq!(run(&rt, &tag(&dcp)), 0);
    }

    #[test]
    fn acyclic_filter_accepts_dcp_and_rejects_rtc1() {
        let ac = acyclic_filter();
        let dcp = golden("ident_ok_pnet");
        assert_eq!(run(&ac, &dcp), 0xFFFF);
        assert_eq!(run(&ac, &tag(&dcp)), 0xFFFF);
        let cpu = golden_rt("rtc_cpu_8001");
        assert_eq!(run(&ac, &cpu), 0);
        assert_eq!(run(&ac, &untag(&cpu)), 0);
    }

    #[test]
    fn both_filters_reject_ipv4_and_short_frames() {
        let mut ipv4 = golden("ident_ok_pnet");
        ipv4[12] = 0x08;
        ipv4[13] = 0x00;
        assert_eq!(run(&rt_filter(), &ipv4), 0);
        assert_eq!(run(&acyclic_filter(), &ipv4), 0);
        assert_eq!(run(&rt_filter(), &ipv4[..13]), 0);
        assert_eq!(run(&acyclic_filter(), &[0x81, 0x00]), 0);
    }
}
```

Add `pub mod bpf;` to `eth/mod.rs` (unconditional — the program builder is pure; only `attach_filter` is Linux-only).

- [ ] **Step 2: Run to verify they fail**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt eth::bpf 2>&1 | tail -5`
Expected: compile error (missing items).

- [ ] **Step 3: Implement `bpf.rs`**

Above the tests:

```rust
//! Classic BPF programs attached to the `AF_PACKET` sockets so each one wakes up only
//! for the frames it handles: the RT socket for RTC1 (`0x8000..=0xBFFF`), the acyclic
//! socket for alarms and DCP (`0xFC00..=0xFFFF`).
//!
//! The program accepts EtherType `0x8892` directly or behind an 802.1Q tag; the
//! kernel usually strips the tag before the filter runs, but a NIC without VLAN RX
//! offload would not, so both shapes are handled.
//!
//! Only the handful of opcodes we need are defined here, from the classic BPF
//! encoding (`BPF_CLASS | BPF_SIZE | BPF_MODE` for loads, `BPF_JMP | op | BPF_K` for
//! jumps): no `libc` constants exist for them.

/// One classic BPF instruction; same layout as the kernel's `struct sock_filter`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SockFilter {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

/// `A = half-word at [k]` (`BPF_LD | BPF_H | BPF_ABS`).
pub const LD_H_ABS: u16 = 0x28;
/// `A = half-word at [X + k]` (`BPF_LD | BPF_H | BPF_IND`).
pub const LD_H_IND: u16 = 0x48;
/// `X = k` (`BPF_LDX | BPF_W | BPF_IMM`).
pub const LDX_IMM: u16 = 0x01;
/// `pc += k` (`BPF_JMP | BPF_JA`).
pub const JA: u16 = 0x05;
/// `pc += (A == k) ? jt : jf` (`BPF_JMP | BPF_JEQ | BPF_K`).
pub const JEQ: u16 = 0x15;
/// `pc += (A >= k) ? jt : jf` (`BPF_JMP | BPF_JGE | BPF_K`).
pub const JGE: u16 = 0x35;
/// `pc += (A > k) ? jt : jf` (`BPF_JMP | BPF_JGT | BPF_K`).
pub const JGT: u16 = 0x25;
/// `return k` (`BPF_RET | BPF_K`): 0 drops, anything else accepts that many bytes.
pub const RET: u16 = 0x06;

const fn insn(code: u16, jt: u8, jf: u8, k: u32) -> SockFilter {
    SockFilter { code, jt, jf, k }
}

/// Accept PROFINET (`0x8892`) frames, untagged or 802.1Q-tagged, whose FrameID is in
/// `lo..=hi`; drop everything else.
///
/// ```text
///  0: ldh [12]                 ethertype
///  1: jeq 0x8892  → 2 else 4
///  2: ldx #14                  FrameID offset, untagged
///  3: ja  → 8
///  4: jeq 0x8100  → 5 else 11
///  5: ldh [16]                 inner ethertype
///  6: jeq 0x8892  → 7 else 11
///  7: ldx #18                  FrameID offset, tagged
///  8: ldh [x+0]                FrameID
///  9: jge lo      → 10 else 11
/// 10: jgt hi      → 11 else 12
/// 11: ret 0
/// 12: ret 0xFFFF
/// ```
pub fn frame_id_filter(lo: u16, hi: u16) -> Vec<SockFilter> {
    vec![
        insn(LD_H_ABS, 0, 0, 12),
        insn(JEQ, 0, 2, 0x8892),
        insn(LDX_IMM, 0, 0, 14),
        insn(JA, 0, 0, 4),
        insn(JEQ, 0, 6, 0x8100),
        insn(LD_H_ABS, 0, 0, 16),
        insn(JEQ, 0, 4, 0x8892),
        insn(LDX_IMM, 0, 0, 18),
        insn(LD_H_IND, 0, 0, 0),
        insn(JGE, 0, 1, u32::from(lo)),
        insn(JGT, 0, 1, u32::from(hi)),
        insn(RET, 0, 0, 0),
        insn(RET, 0, 0, 0xFFFF),
    ]
}

/// Filter for the RT socket: RTC1 frames only.
pub fn rt_filter() -> Vec<SockFilter> {
    frame_id_filter(0x8000, 0xBFFF)
}

/// Filter for the acyclic socket: alarms (`0xFC01`, `0xFE01`) and DCP (`0xFEFC..=0xFEFF`).
pub fn acyclic_filter() -> Vec<SockFilter> {
    frame_id_filter(0xFC00, 0xFFFF)
}
```

`eth/mod.rs`: `pub mod bpf;` and `pub use bpf::SockFilter;` are enough (callers use `eth::bpf::rt_filter()`).

- [ ] **Step 4: `attach_filter` in `afpacket.rs`**

Add to `impl AfPacketTransport`:

```rust
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
```
`use super::bpf::SockFilter;`. Add a test next to `open_loopback_succeeds`:

```rust
    #[test]
    fn attach_filter_on_loopback_succeeds() {
        let Ok(t) = AfPacketTransport::open("lo") else {
            return; // no CAP_NET_RAW in this environment: nothing to test
        };
        t.attach_filter(&super::super::bpf::rt_filter()).unwrap();
    }
```
(Mirror whatever guard `open_loopback_succeeds` already uses for the no-capability case.)

- [ ] **Step 5: Attach `rt_filter` in `RtRunner::spawn`**

```rust
    pub fn spawn(cfg: RtConfig) -> Result<RtHandle, RtError> {
        let transport = AfPacketTransport::open(&cfg.iface)?;
        // The RT socket must never wake up for DCP or alarms: an unfiltered run is
        // not comparable, so a filter failure is fatal here, not a warning.
        transport.attach_filter(&crate::eth::bpf::rt_filter())?;
        Self::spawn_with_transport(cfg, transport)
    }
```

- [ ] **Step 6: Run everything**

Run: `. "$HOME/.cargo/env" && cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all 2>&1 | grep -E "^test result|FAILED|panicked" && cargo build -q --release --target x86_64-unknown-linux-musl --example rt_bringup && echo MUSL_OK`
Expected: green; 4 new bpf tests + 1 afpacket test pass.

- [ ] **Step 7: Commit**

```bash
git add -A crates/profinet-rt
git commit -m "feat(eth): classic BPF filters per AF_PACKET socket — RT socket sees RTC1 only, acyclic socket alarms/DCP only"
```

---

### Task 4: `rt::hist` — latency histograms in `RtStats`

**Files:**
- Create: `crates/profinet-rt/src/rt/hist.rs`
- Modify: `crates/profinet-rt/src/rt/engine.rs:60-115` (`RtStats`, `StatsSnapshot`, `snapshot`), `crates/profinet-rt/src/rt/mod.rs` (export), `crates/profinet-rt/src/rt/runner.rs` (recording)

**Interfaces:**
- Produces: `rt::hist::{HIST_BINS: usize = 2048, Histogram, HistSnapshot { bins: Vec<u64>, count: u64, max_ns: u64 }}`; `Histogram::{new() (const), record(ns: u64), count(), max_ns(), percentile(p: f64) -> Option<u64> /* µs */, snapshot(), reset()}`; `RtStats::{tick_lateness, cycle_work, rx_interval}: Histogram`; `StatsSnapshot::{max_cycle_work_ns, max_rx_interval_ns}`.
- Consumes: `engine.last_rx() -> Option<Instant>` (exists).

- [ ] **Step 1: Write the failing tests**

Create `hist.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_histogram_has_no_percentile() {
        let h = Histogram::new();
        assert_eq!(h.count(), 0);
        assert_eq!(h.max_ns(), 0);
        assert_eq!(h.percentile(50.0), None);
    }

    #[test]
    fn dirac_reports_its_bin_at_every_percentile() {
        let h = Histogram::new();
        for _ in 0..100 {
            h.record(42_300); // 42.3 µs → bin 42
        }
        assert_eq!(h.count(), 100);
        assert_eq!(h.max_ns(), 42_300);
        assert_eq!(h.percentile(0.0), Some(42));
        assert_eq!(h.percentile(50.0), Some(42));
        assert_eq!(h.percentile(99.99), Some(42));
        assert_eq!(h.percentile(100.0), Some(42));
    }

    #[test]
    fn uniform_distribution_percentiles() {
        let h = Histogram::new();
        for us in 0..1000u64 {
            h.record(us * 1000);
        }
        assert_eq!(h.percentile(50.0), Some(499));
        assert_eq!(h.percentile(99.0), Some(989));
        assert_eq!(h.percentile(99.99), Some(999));
        assert_eq!(h.percentile(100.0), Some(999));
    }

    #[test]
    fn overflow_goes_to_the_last_bin_and_max_keeps_the_real_value() {
        let h = Histogram::new();
        h.record(5_000_000); // 5 ms
        assert_eq!(h.percentile(100.0), Some((HIST_BINS - 1) as u64));
        assert_eq!(h.max_ns(), 5_000_000);
        assert_eq!(h.snapshot().bins[HIST_BINS - 1], 1);
    }

    #[test]
    fn reset_clears_everything() {
        let h = Histogram::new();
        h.record(10_000);
        h.reset();
        assert_eq!(h.count(), 0);
        assert_eq!(h.max_ns(), 0);
        assert_eq!(h.percentile(50.0), None);
        assert!(h.snapshot().bins.iter().all(|&b| b == 0));
    }
}
```

Add to `engine.rs` tests (find the existing `RtStats`-related test or add a new one):

```rust
    #[test]
    fn stats_snapshot_carries_the_histogram_maxima() {
        let s = RtStats::default();
        s.tick_lateness.record(1_500);
        s.cycle_work.record(20_000);
        s.rx_interval.record(1_010_000);
        let snap = s.snapshot();
        assert_eq!(snap.max_tick_lateness_ns, 1_500);
        assert_eq!(snap.max_cycle_work_ns, 20_000);
        assert_eq!(snap.max_rx_interval_ns, 1_010_000);
    }
```

Add to the runner test `runner_ticks_sends_and_consumes_with_a_mock_transport`, after the existing stats asserts:

```rust
        assert_eq!(stats.cycle_work.count(), stats.snapshot().tx);
        assert_eq!(stats.tick_lateness.count(), stats.snapshot().tx);
        assert_eq!(stats.rx_interval.count(), 0); // one frame: no interval yet
```

`rt/mod.rs`: `pub mod hist;` and `pub use hist::{HistSnapshot, Histogram, HIST_BINS};`.

- [ ] **Step 2: Run to verify they fail**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt hist 2>&1 | tail -5`
Expected: compile errors.

- [ ] **Step 3: Implement `hist.rs`**

```rust
//! Fixed-bin latency histogram, written by the RT thread (one relaxed `fetch_add`
//! and one `fetch_max` per sample) and read from any other thread.

use std::sync::atomic::{AtomicU64, Ordering};

/// 1 µs bins from 0 to 2046 µs; the last bin collects everything ≥ 2047 µs.
pub const HIST_BINS: usize = 2048;

/// Plain-value copy of a [`Histogram`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistSnapshot {
    /// Sample count per 1 µs bin; `bins[HIST_BINS - 1]` is the overflow bin.
    pub bins: Vec<u64>,
    pub count: u64,
    pub max_ns: u64,
}

pub struct Histogram {
    bins: [AtomicU64; HIST_BINS],
    count: AtomicU64,
    max_ns: AtomicU64,
}

impl Histogram {
    pub const fn new() -> Self {
        const ZERO: AtomicU64 = AtomicU64::new(0);
        Self {
            bins: [ZERO; HIST_BINS],
            count: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
        }
    }

    /// Record one sample in nanoseconds. RT-safe: no lock, no allocation.
    pub fn record(&self, ns: u64) {
        let bin = usize::try_from(ns / 1000).map_or(HIST_BINS - 1, |b| b.min(HIST_BINS - 1));
        self.bins[bin].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.max_ns.fetch_max(ns, Ordering::Relaxed);
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    pub fn max_ns(&self) -> u64 {
        self.max_ns.load(Ordering::Relaxed)
    }

    /// The bin (in µs) below which `p` percent of the samples fall (`p` in `0..=100`);
    /// `None` when empty. The overflow bin reads as `HIST_BINS - 1` — use
    /// [`Histogram::max_ns`] for the real maximum.
    pub fn percentile(&self, p: f64) -> Option<u64> {
        let count = self.count();
        if count == 0 {
            return None;
        }
        let p = p.clamp(0.0, 100.0);
        // Rank of the wanted sample, 1-based; p = 0 → the first sample.
        let target = ((p / 100.0) * count as f64).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (i, bin) in self.bins.iter().enumerate() {
            seen += bin.load(Ordering::Relaxed);
            if seen >= target {
                return Some(i as u64);
            }
        }
        Some((HIST_BINS - 1) as u64) // counts raced past `count`: report the tail
    }

    pub fn snapshot(&self) -> HistSnapshot {
        HistSnapshot {
            bins: self.bins.iter().map(|b| b.load(Ordering::Relaxed)).collect(),
            count: self.count(),
            max_ns: self.max_ns(),
        }
    }

    pub fn reset(&self) {
        for b in &self.bins {
            b.store(0, Ordering::Relaxed);
        }
        self.count.store(0, Ordering::Relaxed);
        self.max_ns.store(0, Ordering::Relaxed);
    }
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Histogram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Histogram")
            .field("count", &self.count())
            .field("max_ns", &self.max_ns())
            .field("p50_us", &self.percentile(50.0))
            .field("p99_us", &self.percentile(99.0))
            .field("p9999_us", &self.percentile(99.99))
            .finish()
    }
}
```

- [ ] **Step 4: Extend `RtStats` / `StatsSnapshot` in `engine.rs`**

`RtStats` keeps `#[derive(Debug, Default)]` (both impls exist on `Histogram`) and gains:
```rust
    /// Timer wake-up minus scheduled expiry, per tick.
    pub tick_lateness: Histogram,
    /// Tick wake-up to `send` returned, per tick: our own cost.
    pub cycle_work: Histogram,
    /// Interval between two consecutive accepted controller frames.
    pub rx_interval: Histogram,
```
`StatsSnapshot` gains `pub max_cycle_work_ns: u64, pub max_rx_interval_ns: u64`; `snapshot()` fills them from `self.cycle_work.max_ns()` / `self.rx_interval.max_ns()`. `use super::hist::Histogram;`. Any place that builds a `StatsSnapshot` literal (grep `StatsSnapshot {`) gets the two fields.

- [ ] **Step 5: Record in `runner.rs`**

1. Tick lateness — replace the `fetch_max` on `max_tick_lateness_ns` with both:
```rust
                        stats
                            .max_tick_lateness_ns
                            .fetch_max(lateness, Ordering::Relaxed);
                        stats.tick_lateness.record(lateness);
```
2. Cycle work — right after the successful `transport.send(frame)`:
```rust
                    stats
                        .cycle_work
                        .record(now.elapsed().as_nanos() as u64);
```
(`now` is the tick wake-up `Instant` already in scope.)
3. RX interval — in `drain_rx`, inside `Ok(Some(n))`, before `engine.on_frame`: `let prev_rx = engine.last_rx();` and inside the `Accepted` branch, first line:
```rust
                    if let Some(prev) = prev_rx {
                        stats
                            .rx_interval
                            .record(now.saturating_duration_since(prev).as_nanos() as u64);
                    }
```
Check `engine.last_rx()` returns the *previous* accepted frame's instant before `on_frame` updates it (it is used that way for `last_rx_age` today).

- [ ] **Step 6: Run everything**

Run: `. "$HOME/.cargo/env" && cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: green; 5 hist tests, the engine test and the extended runner test pass.

- [ ] **Step 7: Commit**

```bash
git add -A crates/profinet-rt
git commit -m "feat(rt): 1 µs-bin latency histograms (tick lateness, cycle work, RX interval) in RtStats"
```

---

### Task 5: `rt_bringup` — flags, CSV, verdict, 1 ms application loop

**Files:**
- Modify: `crates/profinet-rt/examples/rt_bringup.rs`

**Interfaces:**
- Consumes: `rt::sched::set_affinity`, `RtOptions.lock_memory`, `AfPacketTransport::attach_filter`, `eth::bpf::acyclic_filter`, `RtStats::{tick_lateness, cycle_work, rx_interval}`, `Histogram::{percentile, max_ns, snapshot}`, `HIST_BINS`.
- Produces: CLI contract used by `bench/campaign.sh` (Task 6): flags `--lock-memory`, `--app-cpus`, `--duration`, `--csv`, `--max-lateness-us`, `--p9999-lateness-us`, `--max-rx-interval-us`; exit code 0 on `VERDICT: PASS`, 1 otherwise; summary on stderr.

- [ ] **Step 1: Add the flags**

Extend `Args`:

```rust
    /// Lock process memory (mlockall) and pre-fault the RT stack
    #[arg(long)]
    lock_memory: bool,
    /// CPUs for the main/acyclic/application threads, e.g. "0-2" or "0,1,2"
    #[arg(long)]
    app_cpus: Option<String>,
    /// Stop after this many seconds (0 = run until SIGINT/SIGTERM)
    #[arg(long, default_value_t = 0)]
    duration: u64,
    /// Write per-interval stats to this CSV; histograms go to <PATH>.hist.csv at exit
    #[arg(long)]
    csv: Option<std::path::PathBuf>,
    /// Verdict threshold: max tick lateness, µs
    #[arg(long, default_value_t = 300)]
    max_lateness_us: u64,
    /// Verdict threshold: p99.99 tick lateness, µs
    #[arg(long, default_value_t = 100)]
    p9999_lateness_us: u64,
    /// Verdict threshold: max interval between controller frames, µs
    #[arg(long, default_value_t = 1500)]
    max_rx_interval_us: u64,
```

Add the CPU-list parser (unit-testable, keep it at the bottom of the file):

```rust
/// Parse "0-2", "0,1,2" or "3" into a CPU list.
fn parse_cpu_list(s: &str) -> Result<Vec<usize>, String> {
    let mut cpus = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let a: usize = a.parse().map_err(|_| format!("bad cpu '{a}'"))?;
                let b: usize = b.parse().map_err(|_| format!("bad cpu '{b}'"))?;
                if a > b {
                    return Err(format!("bad range '{part}'"));
                }
                cpus.extend(a..=b);
            }
            None => cpus.push(part.parse().map_err(|_| format!("bad cpu '{part}'"))?),
        }
    }
    if cpus.is_empty() {
        return Err("empty cpu list".into());
    }
    Ok(cpus)
}

#[cfg(test)]
mod tests {
    use super::parse_cpu_list;

    #[test]
    fn parses_ranges_and_lists() {
        assert_eq!(parse_cpu_list("0-2").unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_cpu_list("0,1,2").unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_cpu_list("3").unwrap(), vec![3]);
        assert_eq!(parse_cpu_list("0-1,3").unwrap(), vec![0, 1, 3]);
        assert!(parse_cpu_list("").is_err());
        assert!(parse_cpu_list("2-1").is_err());
        assert!(parse_cpu_list("x").is_err());
    }
}
```

- [ ] **Step 2: Wire the setup**

In `main`, right after `Args::parse()`:
```rust
    if let Some(list) = a.app_cpus.as_deref() {
        let cpus = parse_cpu_list(list).unwrap_or_else(|e| {
            eprintln!("--app-cpus: {e}");
            std::process::exit(2);
        });
        // Threads spawned later (application loop) inherit this; the RT thread sets
        // its own affinity from --cpu.
        if let Err(e) = profinet_rt::rt::sched::set_affinity(&cpus) {
            log::warn!("app affinity {cpus:?}: {e}");
        }
    }
```
`RtOptions` literal: `lock_memory: a.lock_memory,`. After `AfPacketTransport::open`:
```rust
    eth.attach_filter(&profinet_rt::eth::bpf::acyclic_filter())
        .expect("attach acyclic BPF filter");
```

- [ ] **Step 3: CSV, duration and the 1 ms application loop**

Replace the application thread body. Keep `run_app_cycle` unchanged. New body (the closure captures `a.stats_every`, `a.duration`, `a.csv`):

```rust
    let image = dev.image();
    let stats = dev.rt_stats();
    let app_stop = stop.clone();
    let csv_path = a.csv.clone();
    let duration = a.duration;
    let app = std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let mut last_log = started;
        let mut last_err_log = started - Duration::from_secs(1);
        let stats_every = Duration::from_secs(a.stats_every);
        let mut csv = csv_path.as_ref().map(|p| {
            let mut f = std::fs::File::create(p).expect("create csv");
            use std::io::Write;
            writeln!(f, "t_s,tx,rx_accepted,rx_dropped,missed_ticks,watchdog_expirations,reused,deferred,lat_max_us,lat_p9999_us,work_max_us,rxint_max_us").expect("csv header");
            f
        });
        while !app_stop.load(Ordering::Relaxed) {
            for r in run_app_cycle(&image) {
                match r {
                    Ok(()) | Err(ImageError::UnknownSubmodule { .. }) => {} // no AR yet: retry
                    Err(e) => {
                        if last_err_log.elapsed() >= Duration::from_secs(1) {
                            log::warn!("application cycle error: {e}");
                            last_err_log = std::time::Instant::now();
                        }
                    }
                }
            }
            if last_log.elapsed() >= stats_every {
                let s = stats.snapshot();
                log::info!("rt stats: {s:?}, freshness: {:?}", image.validity().freshness());
                if let Some(f) = csv.as_mut() {
                    use std::io::Write;
                    let _ = writeln!(
                        f,
                        "{},{},{},{},{},{},{},{},{},{},{},{}",
                        started.elapsed().as_secs(),
                        s.tx,
                        s.rx_accepted,
                        s.rx_dropped,
                        s.missed_ticks,
                        s.watchdog_expirations,
                        s.input_snapshot_reused,
                        s.output_publish_deferred,
                        s.max_tick_lateness_ns / 1000,
                        stats.tick_lateness.percentile(99.99).unwrap_or(0),
                        s.max_cycle_work_ns / 1000,
                        s.max_rx_interval_ns / 1000,
                    );
                }
                last_log = std::time::Instant::now();
            }
            if duration > 0 && started.elapsed() >= Duration::from_secs(duration) {
                log::info!("--duration reached, stopping");
                app_stop.store(true, Ordering::Relaxed);
            }
            // 1 ms: at a 1 ms update time a slower application loop would make
            // `input_snapshot_reused` meaningless.
            std::thread::sleep(Duration::from_millis(1));
        }
    });
```
(The per-interval `lat_max_us` / `lat_p9999_us` are cumulative, not windowed — simpler and what the verdict uses; say so in the CSV header comment in `bench/README.md`.)

- [ ] **Step 4: Verdict at exit**

Add a `Thresholds` struct and `verdict` function, and call it after the device loop ends (both the error path and the normal path), writing the histogram CSV first:

```rust
struct Thresholds {
    max_lateness_us: u64,
    p9999_lateness_us: u64,
    max_rx_interval_us: u64,
}

/// Print the summary and return true on PASS.
fn verdict(stats: &profinet_rt::rt::RtStats, t: &Thresholds, memory_locked: bool, secs: u64) -> bool {
    use profinet_rt::rt::Histogram;
    let s = stats.snapshot();
    let line = |name: &str, h: &Histogram| {
        eprintln!(
            "{name:14} n={:<8} p50={:>5}us p99={:>5}us p99.99={:>5}us max={:>8.1}us",
            h.count(),
            h.percentile(50.0).unwrap_or(0),
            h.percentile(99.0).unwrap_or(0),
            h.percentile(99.99).unwrap_or(0),
            h.max_ns() as f64 / 1000.0
        );
    };
    eprintln!("--- rt_bringup summary ({secs} s) ---");
    line("tick_lateness", &stats.tick_lateness);
    line("cycle_work", &stats.cycle_work);
    line("rx_interval", &stats.rx_interval);
    eprintln!(
        "tx={} rx_accepted={} rx_dropped={} missed_ticks={} watchdog_expirations={} reused={} deferred={} memory_locked={}",
        s.tx, s.rx_accepted, s.rx_dropped, s.missed_ticks, s.watchdog_expirations,
        s.input_snapshot_reused, s.output_publish_deferred, if memory_locked { "yes" } else { "no" }
    );
    let mut fails = Vec::new();
    if s.missed_ticks != 0 {
        fails.push(format!("missed_ticks={}", s.missed_ticks));
    }
    if s.watchdog_expirations != 0 {
        fails.push(format!("watchdog_expirations={}", s.watchdog_expirations));
    }
    let lat_max = s.max_tick_lateness_ns / 1000;
    if lat_max >= t.max_lateness_us {
        fails.push(format!("lateness max {lat_max}us >= {}us", t.max_lateness_us));
    }
    let p = stats.tick_lateness.percentile(99.99).unwrap_or(0);
    if p >= t.p9999_lateness_us {
        fails.push(format!("lateness p99.99 {p}us >= {}us", t.p9999_lateness_us));
    }
    let rx = s.max_rx_interval_ns / 1000;
    if rx >= t.max_rx_interval_us {
        fails.push(format!("rx_interval max {rx}us >= {}us", t.max_rx_interval_us));
    }
    if s.tx == 0 {
        fails.push("no cyclic exchange happened".into());
    }
    let short = if secs < 60 { " (short run)" } else { "" };
    if fails.is_empty() {
        eprintln!("VERDICT: PASS{short}");
        true
    } else {
        eprintln!("VERDICT: FAIL{short} ({})", fails.join(", "));
        false
    }
}

fn write_hist_csv(path: &std::path::Path, stats: &profinet_rt::rt::RtStats) {
    use std::io::Write;
    let (a, b, c) = (
        stats.tick_lateness.snapshot(),
        stats.cycle_work.snapshot(),
        stats.rx_interval.snapshot(),
    );
    let mut f = std::fs::File::create(path).expect("create hist csv");
    writeln!(f, "bin_us,tick_lateness,cycle_work,rx_interval").unwrap();
    for i in 0..profinet_rt::rt::HIST_BINS {
        writeln!(f, "{i},{},{},{}", a.bins[i], b.bins[i], c.bins[i]).unwrap();
    }
}
```

`memory_locked`: `true` when `--lock-memory` was given and no `SchedWarning` mentioning `mlockall` was logged. Simplest: `Device` logs the warning; the example cannot see it. Instead, call `profinet_rt::rt::sched::lock_memory()` **in `main` itself** when `--lock-memory` is set (process-wide, so doing it once in main is equivalent), record the result in a `memory_locked: bool`, and still pass `lock_memory: a.lock_memory` to `RtOptions` so the RT stack gets pre-faulted (the second `mlockall` is idempotent). Log a warning on failure.

End of `main`:
```rust
    let started = std::time::Instant::now();   // set before dev.run
    …
    let run_result = dev.run(&stop);
    stop.store(true, Ordering::Relaxed);
    let _ = app.join();
    if let Some(p) = a.csv.as_ref() {
        let mut hist = p.clone().into_os_string();
        hist.push(".hist.csv");
        write_hist_csv(std::path::Path::new(&hist), &stats_main);
    }
    let thresholds = Thresholds {
        max_lateness_us: a.max_lateness_us,
        p9999_lateness_us: a.p9999_lateness_us,
        max_rx_interval_us: a.max_rx_interval_us,
    };
    let pass = verdict(&stats_main, &thresholds, memory_locked, started.elapsed().as_secs());
    if let Err(e) = run_result {
        log::error!("device loop ended: {e}");
        std::process::exit(1);
    }
    std::process::exit(if pass { 0 } else { 1 });
```
(`stats_main = dev.rt_stats()` taken before the app thread moves its own clone.)

- [ ] **Step 5: Build, test, musl**

Cargo does not run tests inside examples unless told to. Add to `crates/profinet-rt/Cargo.toml`:

```toml
[[example]]
name = "rt_bringup"
test = true
```

Run: `. "$HOME/.cargo/env" && cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all 2>&1 | grep -E "^test result|FAILED|panicked" && cargo build -q --release --target x86_64-unknown-linux-musl --example rt_bringup && echo MUSL_OK`
Expected: green, a `Running unittests examples/rt_bringup.rs` line with `parses_ranges_and_lists` passing, `MUSL_OK`. Also: `cargo run -q --example rt_bringup -- --help | grep -c -- "--"` ≥ 13.

- [ ] **Step 6: Commit**

```bash
git add crates/profinet-rt/examples/rt_bringup.rs crates/profinet-rt/Cargo.toml
git commit -m "feat(example): rt_bringup — lock-memory/app-cpus/duration/csv flags, histogram CSV, threshold verdict, 1 ms app loop"
```

---

### Task 6: `bench/` — edge tuning, load and campaign scripts

**Files:**
- Create: `bench/edge-rt-tune.sh`, `bench/profinet-rt-tune.service`, `bench/load.sh`, `bench/campaign.sh`, `bench/README.md`

**Interfaces:**
- Consumes: `rt_bringup` CLI (Task 5), `~/bench/rt_bringup` binary path on the edge, edge facts from Global Constraints.
- Produces: `~/bench/logs/plan7-<ts>/{env.txt,cyclictest-idle.txt,cyclictest-load.txt,rt-idle.log,rt-idle.csv,rt-idle.csv.hist.csv,rt-load.log,rt-load.csv,rt-load.csv.hist.csv,rt-load.pcapng,summary.txt}`.

- [ ] **Step 1: `bench/edge-rt-tune.sh`**

```bash
#!/usr/bin/env bash
# Edge tuning for the 1 ms PROFINET RT campaign (spec §5.2). Idempotent; run as root.
# Prints ok/warn/FAIL per step and the resulting state at the end.
set -euo pipefail

PLC_IF="${PLC_IF:-eno2}"
RT_CPU="${RT_CPU:-3}"
HK_CPUS="${HK_CPUS:-0-2}"
IRQ_PRIO="${IRQ_PRIO:-90}"
RX_USECS="${RX_USECS:-0}"
TX_USECS="${TX_USECS:-0}"
EEE="${EEE:-off}"

ok()   { echo "ok    $*"; }
warn() { echo "warn  $*" >&2; }
fail() { echo "FAIL  $*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || fail "run as root"

# 1. preconditions
[ "$(cat /sys/kernel/realtime 2>/dev/null)" = "1" ] || fail "not a PREEMPT_RT kernel"
isolated="$(cat /sys/devices/system/cpu/isolated)"
case ",$isolated," in
  *",$RT_CPU,"*) ok "cpu $RT_CPU isolated" ;;
  *) [ "$isolated" = "$RT_CPU" ] && ok "cpu $RT_CPU isolated" || fail "cpu $RT_CPU not in isolated='$isolated' (GRUB cmdline?)" ;;
esac

# 2. governor
for g in /sys/devices/system/cpu/cpu[0-9]*/cpufreq/scaling_governor; do
  echo performance > "$g" 2>/dev/null && ok "$g = performance" || warn "$g not writable"
done

# 3. single queue
if ethtool -L "$PLC_IF" combined 1 >/dev/null 2>&1; then ok "$PLC_IF combined 1"; else warn "$PLC_IF: ethtool -L unsupported"; fi

# 4. IRQ affinity
while read -r irq name; do
  irq="${irq%:}"
  if [[ "$name" == *TxRx* ]]; then
    echo "$RT_CPU" > "/proc/irq/$irq/smp_affinity_list" && ok "irq $irq ($name) -> cpu $RT_CPU"
  else
    echo "$HK_CPUS" > "/proc/irq/$irq/smp_affinity_list" && ok "irq $irq ($name) -> cpus $HK_CPUS"
  fi
done < <(awk -v ifc="$PLC_IF" '$NF ~ "^"ifc {print $1, $NF}' /proc/interrupts)

# 5. IRQ thread priority (threaded IRQs on PREEMPT_RT)
for pid in $(pgrep -f "irq/[0-9]+-${PLC_IF}-TxRx" || true); do
  chrt -f -p "$IRQ_PRIO" "$pid" && ok "irq thread pid $pid -> SCHED_FIFO $IRQ_PRIO"
done

# 6. NIC latency knobs (igb support is a hypothesis: warn, never fail)
ethtool --set-eee "$PLC_IF" eee "$EEE" >/dev/null 2>&1 && ok "$PLC_IF eee $EEE" || warn "$PLC_IF: eee not settable"
ethtool -C "$PLC_IF" rx-usecs "$RX_USECS" tx-usecs "$TX_USECS" >/dev/null 2>&1 && ok "$PLC_IF coalescing rx=$RX_USECS tx=$TX_USECS" || warn "$PLC_IF: coalescing not settable"
ethtool -K "$PLC_IF" gro off lro off >/dev/null 2>&1 && ok "$PLC_IF gro/lro off" || warn "$PLC_IF: gro/lro not settable"

# 7. sysctl
sysctl -q -w kernel.sched_rt_runtime_us=-1 && ok "sched_rt_runtime_us=-1"
sysctl -q -w kernel.timer_migration=0 && ok "timer_migration=0"
sysctl -q -w vm.stat_interval=120 && ok "vm.stat_interval=120"

# 8. state
echo "----- state -----"
echo "kernel:    $(uname -r)  realtime=$(cat /sys/kernel/realtime)"
echo "cmdline:   $(cat /proc/cmdline)"
echo "isolated:  $isolated   nohz_full: $(cat /sys/devices/system/cpu/nohz_full)"
echo "governor:  $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor)"
echo "cpuidle:   $(for s in /sys/devices/system/cpu/cpu$RT_CPU/cpuidle/state*; do printf '%s(disable=%s) ' "$(cat "$s/name")" "$(cat "$s/disable")"; done)"
awk -v ifc="$PLC_IF" '$NF ~ "^"ifc {gsub(":","",$1); print "irq " $1 " " $NF}' /proc/interrupts | while read -r _ irq name; do
  echo "irq:       $irq $name affinity=$(cat /proc/irq/$irq/smp_affinity_list)"
done
for pid in $(pgrep -f "irq/[0-9]+-${PLC_IF}-TxRx" || true); do echo "irqthread: pid $pid $(chrt -p "$pid" | tr '\n' ' ')"; done
ethtool -l "$PLC_IF" 2>/dev/null | sed -n '/Current/,$p' | tr '\n' ' '; echo
ethtool -c "$PLC_IF" 2>/dev/null | grep -E '^(rx-usecs|tx-usecs):' | tr '\n' ' '; echo
ethtool --show-eee "$PLC_IF" 2>/dev/null | grep -i 'EEE status' || true
```

- [ ] **Step 2: `bench/profinet-rt-tune.service`**

```ini
[Unit]
Description=profinet-rt edge tuning (isolated RT core, NIC IRQ affinity)
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/home/maintenance/bench/edge-rt-tune.sh

[Install]
WantedBy=multi-user.target
```

- [ ] **Step 3: `bench/load.sh`**

```bash
#!/usr/bin/env bash
# CPU + memory load on the housekeeping cores only (never on the isolated RT core).
set -euo pipefail
SECS="${1:-600}"
HK_CPUS="${HK_CPUS:-0-2}"
exec taskset -c "$HK_CPUS" stress-ng --cpu 3 --vm 1 --vm-bytes 512M --timeout "${SECS}s" --metrics-brief
```

- [ ] **Step 4: `bench/campaign.sh`**

```bash
#!/usr/bin/env bash
# Plan 7 campaign: cyclictest (idle, load) then rt_bringup (idle, load + tcpdump).
# Run from ~/bench on the edge, TIA already at a 1 ms update time.
set -euo pipefail

DURATION="${1:-600}"
BENCH="${BENCH:-$HOME/bench}"
BIN="${BIN:-$BENCH/rt_bringup}"
PLC_IF="${PLC_IF:-eno2}"
DEV_IP="${DEV_IP:-172.16.2.10}"
RT_CPU="${RT_CPU:-3}"
HK_CPUS="${HK_CPUS:-0-2}"
RT_PRIO="${RT_PRIO:-80}"
STAMP="${STAMP:-$(date +%Y%m%d-%H%M%S)}"
OUT="$BENCH/logs/plan7-$STAMP"

[ "$(cat /sys/kernel/realtime 2>/dev/null)" = "1" ] || { echo "not PREEMPT_RT" >&2; exit 2; }
grep -qw "$RT_CPU" /sys/devices/system/cpu/isolated || { echo "cpu $RT_CPU not isolated" >&2; exit 2; }
[ -x "$BIN" ] || { echo "$BIN missing" >&2; exit 2; }
mkdir -p "$OUT"
echo "campaign dir: $OUT"

{ uname -r; cat /proc/cmdline; cat /sys/devices/system/cpu/isolated; systemctl status profinet-rt-tune --no-pager 2>&1 | tail -n +1; } > "$OUT/env.txt" 2>&1 || true

step() { echo "== $(date +%T) $*"; }

step "1/4 cyclictest idle ($DURATION s)"
cyclictest -m -p"$RT_PRIO" -a"$RT_CPU" -i1000 -h400 -D"$DURATION" -q > "$OUT/cyclictest-idle.txt"

step "2/4 cyclictest under load"
"$BENCH/load.sh" "$((DURATION + 10))" > "$OUT/load-cyclictest.txt" 2>&1 &
LOAD=$!
sleep 5
cyclictest -m -p"$RT_PRIO" -a"$RT_CPU" -i1000 -h400 -D"$DURATION" -q > "$OUT/cyclictest-load.txt"
wait "$LOAD" || true

RT_ARGS=(--iface "$PLC_IF" --ip "$DEV_IP" --rt-priority "$RT_PRIO" --cpu "$RT_CPU" --app-cpus "$HK_CPUS" --lock-memory --duration "$DURATION" --stats-every 5)

step "3/4 rt_bringup idle"
set +e
"$BIN" "${RT_ARGS[@]}" --csv "$OUT/rt-idle.csv" > "$OUT/rt-idle.log" 2>&1
IDLE_RC=$?
set -e
sleep 10   # let the CPU notice the device is gone and settle

step "4/4 rt_bringup under load + tcpdump"
"$BENCH/load.sh" "$((DURATION + 20))" > "$OUT/load-rt.txt" 2>&1 &
LOAD=$!
taskset -c "$HK_CPUS" tcpdump -i "$PLC_IF" -B 65536 -w "$OUT/rt-load.pcapng" > "$OUT/tcpdump.txt" 2>&1 &
DUMP=$!
sleep 5
set +e
"$BIN" "${RT_ARGS[@]}" --csv "$OUT/rt-load.csv" > "$OUT/rt-load.log" 2>&1
LOAD_RC=$?
set -e
kill -TERM "$DUMP" 2>/dev/null || true
wait "$LOAD" "$DUMP" 2>/dev/null || true

{
  echo "campaign $STAMP, duration $DURATION s"
  echo "cyclictest idle : $(grep -E '^# Max Latencies' "$OUT/cyclictest-idle.txt" || tail -1 "$OUT/cyclictest-idle.txt")"
  echo "cyclictest load : $(grep -E '^# Max Latencies' "$OUT/cyclictest-load.txt" || tail -1 "$OUT/cyclictest-load.txt")"
  echo "--- rt_bringup idle (rc=$IDLE_RC)"; sed -n '/rt_bringup summary/,$p' "$OUT/rt-idle.log"
  echo "--- rt_bringup load (rc=$LOAD_RC)"; sed -n '/rt_bringup summary/,$p' "$OUT/rt-load.log"
} | tee "$OUT/summary.txt"

[ "$IDLE_RC" -eq 0 ] && [ "$LOAD_RC" -eq 0 ]
```

- [ ] **Step 5: `bench/README.md`**

Write it with these sections, each complete: **Prerequisites** (PREEMPT_RT kernel package, GRUB cmdline from spec §5.1 verbatim, disabled services, `rt-tests stress-ng tcpdump ethtool`); **Install** (`scp bench/*.sh bench/*.service maintenance@192.168.1.21:~/bench/`, `chmod +x`, `sudo cp ~/bench/profinet-rt-tune.service /etc/systemd/system/ && sudo systemctl enable --now profinet-rt-tune`, capabilities: `sudo setcap cap_net_raw,cap_net_admin,cap_sys_nice,cap_ipc_lock+eip ~/bench/rt_bringup`, `sudo setcap cap_net_raw,cap_net_admin+eip /usr/bin/tcpdump`, `sudo setcap cap_sys_nice,cap_ipc_lock+eip /usr/bin/cyclictest`; note that `setcap` must be repeated after every binary copy); **Build the edge binary** (`cargo build --release --target x86_64-unknown-linux-musl --example rt_bringup`, `scp target/x86_64-unknown-linux-musl/release/examples/rt_bringup maintenance@192.168.1.21:~/bench/`); **Run** (`campaign.sh [DURATION]`, the directory layout, what `summary.txt` holds, that CSV lateness columns are cumulative); **Thresholds** (the four from spec §1 and the flags to override them); **Post-processing the capture** (the two `tshark.exe` commands: `"/mnt/c/Program Files/Wireshark/tshark.exe" -r rt-load.pcapng -Y "pn_rt.frame_id == 0x8001" -T fields -e frame.time_delta_displayed` piped to `sort -n | awk` for p99.99/max, same for `0x8000`); **TIA** (update time 1 ms, watchdog factor 3, download between the 32 ms control run and the campaign).

- [ ] **Step 6: Lint the scripts and commit**

Run: `bash -n bench/edge-rt-tune.sh bench/load.sh bench/campaign.sh && chmod +x bench/*.sh && (command -v shellcheck >/dev/null && shellcheck bench/*.sh || echo "shellcheck not installed — skipped")`
Expected: no syntax error.

```bash
git add bench
git commit -m "feat(bench): edge RT tuning script + systemd unit, load and campaign scripts, bench README"
```

---

### Task 7: HIL campaign, report, docs (controller + user, not a subagent)

**Files:**
- Modify: `docs/bench-pnet-device.md` (new §6e before §7; update §7), `README.md` (status rows for `rt` and HIL, architecture note), `FOLLOWUPS.md` (Plan 7 section; close or defer the seqlock item per spec §9), `docs/design/2026-08-28-profinet-rt-rt-1ms-design.md` (status line → "implemented, HIL <date>")

- [ ] **Step 1: Build and deploy**

`. "$HOME/.cargo/env" && cargo build --release --target x86_64-unknown-linux-musl --example rt_bringup && scp target/x86_64-unknown-linux-musl/release/examples/rt_bringup bench/*.sh bench/*.service maintenance@192.168.1.21:~/bench/`. Then ask the user for (sudo): the three `setcap` lines from `bench/README.md`, `cp` + `enable --now` of the unit, and `systemctl status profinet-rt-tune` output (the `----- state -----` block goes into the report).

- [ ] **Step 2: Baseline cyclictest** (30 s smoke, then the campaign does the 10 min ones)

`ssh maintenance@192.168.1.21 'cyclictest -m -p80 -a3 -i1000 -h400 -D30 -q | tail -3'` — expect max well under 100 µs. If not, tune before going further (C-states, IRQ placement — check the state block).

- [ ] **Step 3: 32 ms control run** (TIA still at 32 ms)

`ssh -f maintenance@192.168.1.21 'cd ~/bench && setsid nohup ./rt_bringup --iface eno2 --ip 172.16.2.10 --rt-priority 80 --cpu 3 --app-cpus 0-2 --lock-memory --duration 120 --csv logs/control-32ms.csv > logs/control-32ms.log 2>&1 < /dev/null'`. Expect `VERDICT: PASS`, device green, watch table mirroring — proves Tasks 1-5 did not regress Plan 4's behaviour.

- [ ] **Step 4: TIA → 1 ms** (user): device properties → PROFINET interface → real-time settings → update time 1 ms, watchdog factor 3; download; CPU RUN. Confirm the device turns green with `rt_bringup` running a short `--duration 60` run.

- [ ] **Step 5: Campaign**

`ssh -f maintenance@192.168.1.21 'cd ~/bench && setsid nohup ./campaign.sh 600 > logs/campaign.log 2>&1 < /dev/null'`; poll `logs/campaign.log` every few minutes (~45 min total). Copy the campaign directory locally (`scp -r`), run the tshark post-processing from `bench/README.md` on `rt-load.pcapng`.

- [ ] **Step 6: Report**

`docs/bench-pnet-device.md` §6e "HIL — 1 ms on PREEMPT_RT (<date>)": edge state block, cyclictest idle/load (min/avg/max), `rt_bringup` idle/load tables (counters + p50/p99/p99.99/max for the three histograms), pcap inter-arrival p99.99/max both directions, verdict per threshold, TIA diagnostic buffer and watch table (user screenshots described), lessons (which igb knobs were unsupported, anything surprising). Update §7 Next steps. `README.md`: `rt` row → "✅ 1 ms held on PREEMPT_RT (edge Atom E3940, HIL <date>): lateness p99.99 = X µs, max = Y µs under load"; HIL row likewise; architecture note on the seqlock updated with the §9 decision. `FOLLOWUPS.md`: new "## From Plan 7 (1 ms)" section: PACKET_MMAP, busy-poll, PACKET_AUXDATA, unsupported igb knobs, and the seqlock item closed with numbers or promoted to Plan 7bis. Spec status line updated.

- [ ] **Step 7: Commit**

```bash
git add docs README.md FOLLOWUPS.md
git commit -m "docs: Plan 7 HIL — 1 ms campaign results (PREEMPT_RT edge), README status, FOLLOWUPS"
```

---

## Self-review

- **Spec coverage**: §5 → Task 6 (+ GRUB done by user); §6.1 → Task 1; §6.2 → Task 3; §7 → Task 2; §8 → Task 4; §9 → Task 7 (decision recorded); §10 → Task 5; §11 → Task 6 + 7; §12 → Task 7; §13 errors: `mlockall` warning (Task 2 + 5 `memory_locked`), filter fatal (Task 3), `FrameTooLong` counted (Task 1), script warn/FAIL split (Task 6), campaign preconditions (Task 6), watchdog abort = FAIL (Task 5 verdict); §14 tests: each task's Step 1; §15 no deps; §16 roles in Task 7.
- **Type consistency**: `recv_into(&self, &mut [u8], Option<Duration>) -> Result<Option<usize>, TransportError>` used identically in Tasks 1, 3 (unchanged), 4; `RtConfig.lock_memory` / `RtOptions.lock_memory` (Task 2) consumed by Task 5; `Histogram::{percentile, max_ns, snapshot, count}` and `HIST_BINS` (Task 4) consumed by Task 5; `SockFilter` / `rt_filter` / `acyclic_filter` (Task 3) consumed by Tasks 3 and 5; CLI flags (Task 5) consumed by Task 6 (`campaign.sh` `RT_ARGS`).
- **Placeholders**: none; every code step carries the code; the README in Task 6 lists each section's exact content.

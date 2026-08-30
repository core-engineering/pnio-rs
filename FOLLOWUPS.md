# Tracked Follow-ups (from Plan 1 Branch Review)

Non-blocking findings for Plan 1, to be integrated into the briefs of the relevant plans.

## For Plan 4 (`rt` cyclic / RT thread)
- ✅ **RESOLVED (merge 459963d)** — **Kernel filtering & busy-spin**: `AfPacketTransport::open`
  now binds with `sll_protocol = htons(0x8892)` and joins the PROFINET multicast group
  (`01:0e:cf:00:00:00`), so the kernel only wakes `recv` on PROFINET frames — no idle spin on
  unrelated broadcast traffic. Used by the Plan 3 acyclic loop (`device`, merge 896dfd5) since.
- ✅ **RESOLVED (merge 459963d, fix 174cb4d)** — **`recv` timeout**: implemented via `poll`
  (timeouts rounded up to the millisecond); exercised continuously by the Plan 3 device loop's
  `step`/`run` (`crates/pnio/src/device/mod.rs`).
- **MSG_TRUNC**: `recv` does not handle MSG_TRUNC (non-issue for standard RT frames ≤1522).

## For Plan 2 (`dcp`) — before frame-exact comparisons
- ✅ **RESOLVED (merge ba63901)** — **Typed `CaptureError`**: `Io(#[from] std::io::Error)` +
  `Pcap(#[from] pcap_file::PcapError)` + `UnknownFormat([u8;4])`. **`PcapFrames` reads both
  pcap and pcapng** (magic auto-detection) and the iterator returns
  `Result<Vec<u8>, CaptureError>` (no more swallowing). ✅ **`TransportError::Io` now typed**
  too (merge f4de284): `Io(#[from] std::io::Error)` + `From<nix::errno::Errno>` in the
  AF_PACKET backend — cross-module consistency done.

## For Plan 6 (`config` / GSDML / typed API)
- ✅ **RESOLVED (Plan 6, 2026-08-29)** — **BOOL bit ordering (LSB-first) verified on the wire**
  with a real S7-1500 (1515-2 PN FW V2.9.4) ↔ p-net device: `%Q0.0 := TRUE` alone → output
  byte `0x01` in the RTC1 frame (`captures/q-bits-2026-08-27-165102.pcapng`); device input
  byte `0x80` (Button1) → `%I0.7 = TRUE` in TIA (`captures/io-bits-2026-08-27-164448.pcapng`).
  `data::get_bit`/`set_bit` (`1 << (i % 8)`) is correct. The `q-bits` capture is now a test
  vector (`%Q0.0` → `0x01`, `0x80` → bit 7) and `config`'s own tests check the
  declaration→(byte, bit) mapping for our GSDML (`layout_mixes_bools_and_byte_types_in_declaration_order`,
  `layout_packs_bools_eight_per_byte`, `crates/pnio/src/config.rs`).
- ✅ **RESOLVED (Plan 6, 2026-08-29)** — **`data::Value` wired up**: `Value::encode(&self, dst:
  &mut [u8], bit: usize)` / `Value::decode(ty: FieldType, src: &[u8], bit: usize)` /
  `Value::field_type()` are the typed dispatch `api` and `gsdml` build on (`crates/pnio/src/data.rs`).
- ✅ **RESOLVED (Plan 6, 2026-08-29)** — **`Field`/`FieldType` naming reconciled**: the code's
  name, `FieldType`, is the one kept everywhere (`config` re-exports `data::FieldType` as-is;
  `config::FieldRef { byte, bit, ty: FieldType }` is the per-field byte/bit table entry). The
  spec §5.4 sketch's `Field` naming was the one to give way; this doc set (`docs/gsdml.md`,
  `README.md`) uses `FieldType` throughout.

## Doc
- ✅ **RESOLVED (merge f4de284)** — **`recv` contract**: the `EthTransport::recv` trait doc now
  enumerates the legitimate `Ok(None)` cases (empty queue; no frame before timeout — with a note
  that `AfPacketTransport` does not yet honor `timeout`; non-PROFINET frame for the backend).

## For subsequent DCP plans (from Plan dcp branch review)

### ✅ RESOLVED (merge ba63901) — DCP hardening
- **Identify over-response fixed**: `IdentifyFilter` now classifies NameOfStation /
  AllSelector (0xff,0xff) / other filters; `handle_dcp_frame` only responds on a
  confirmable match (matching name, or explicit AllSelector) and **never** if an
  unrecognized filter is present.
- **Minor items closed**: `DcpError::BadFrameId` removed; `pub use` re-exports at the
  `dcp::` level (including `DCP_MULTICAST_MAC`); `debug_assert!` overflow guards in
  `block.rs`; coverage added (`to_u16`, `from_u8` errors, `TooShort` branch,
  empty-identify, AllSelector).

### ✅ RESOLVED (HIL 2026-08-28) — `DeviceRole` ≠ 0 on a real device
- `DeviceRole` encoded as u16 (role+reserved) was byte-exact vs the golden (role=0) but
  unverified for a non-zero role. HIL bring-up (`docs/bench-pnet-device.md` §6c) ran with
  `device_role: 0x0100` (p-net's own DAP frame #37 carries `01 01`); the CPU accepted our
  Ident Ok / Connect exchange without complaint — role encoding confirmed on the wire.

### ✅ RESOLVED (merge 896dfd5) — RX error policy (review recommendation)
- `handle_dcp_frame`'s `Err` on a malformed/short frame is now logged and dropped by the
  caller, not propagated: the Plan 3 acyclic device loop (`crates/pnio/src/device/mod.rs`)
  catches per-frame errors from the DCP/RPC dispatch, logs them, and continues the loop rather
  than aborting the process on a single bad frame.

## From Plan 3 (`rpc` + `cm`)

Open points recorded from the AR-establishment HIL run (`docs/bench-pnet-device.md` §6c,
`hil-facts.md`), to be picked up by the plans noted:

- ✅ **RESOLVED (Plan 5, 2026-08-30)** — **Minimal `Read`/`ReadImplicit` support**: `cm::records`
  now serves `0xAFF0` (I&M0) on every submodule the model knows and `0xAFF1..=0xAFF3` (I&M1-3) on
  the DAP, byte-exact against the capture goldens (`docs/alarm-golden-frames.md`). Index `0xfbff`
  ("RPC connection monitoring trigger") and every other record index still answer PNIORW "invalid
  index" (`PnioStatus::read_index_unsupported`) — only the I&M range moved; see the diagnosis
  record reads item below.
- ✅ **RESOLVED (Plan 5, 2026-08-30)** — **Alarm channel (ERR-RTA / ACK-RTA, frame IDs
  `0xfe01`/`0xfc01`)**: `alarm::rta` (codec) + `alarm::channel` (sender/receiver state machine)
  reproduce the captured handshake byte-for-byte (notification → ACK-RTA → Alarm-Ack → ACK-RTA,
  retries/timeouts from the negotiated `AlarmCR`, ERR-RTA both ways); `device` routes
  `0xFC01`/`0xFE01` frames to it and sends ERR-RTA on every device-side abort and on `stop()`. An
  aborted AR is now announced on the wire, not just inferred from the next reconnect.
- **`PnioStatus` constants** to re-verify against the purchased IEC 61158-6-10 once
  available (current values inferred from captures and p-net's public headers).
- **RPC fragmentation** (multi-fragment DCE-RPC requests/responses) — out of scope for Plan 3,
  rejected explicitly; not exercised by the bench (CPU's PDUs all fit one fragment). Revisit
  if a larger config triggers it.
- **`ModuleDiffBlock`** — not produced by our Connect response; a config mismatch is currently
  rejected with an explicit status rather than reported via `ModuleDiffBlock`. Revisit when
  partial/mismatched module plugging needs to be supported.
- **`PACKET_AUXDATA` / VLAN tag visibility**: `eno2` has `rx-vlan-offload: on`, so the kernel
  untags before delivery to `AfPacketTransport`; reading the original VLAN tag (priority) needs
  `PACKET_AUXDATA`, not yet implemented. Plan 4 (RTC1 priority tagging).
- **RPC response cache never flushed on abort** — benign today because sequence numbers keep
  advancing across an AR takeover (fixed in `8ab2711`), but the cache is not explicitly
  cleared when an AR aborts. Revisit if it causes a stale-response replay in a future scenario.
- **Same-ARUUID takeover not qualified by initiator**: the reconnect fix (`aca42d9`) accepts a
  Connect with the same ARUUID and a bumped session key from *any* initiator UUID, not just the
  one that opened the original AR. Matches observed CPU behavior (single controller on the
  bench) but is not a spec-derived restriction — revisit if a multi-controller scenario is in
  scope.
- **`ExpectedSubmoduleBlockReq` type-0 descriptor**: the golden captures only exercise
  fully-specified submodule descriptors; a type-0 ("no submodule") descriptor's handling is
  documented but not exercised on real hardware.
- **`MultipleWrite` outer `record_data_length = 0`**: our Write response mirrors p-net's
  behavior of zeroing this outer NDR field (see `docs/cm-golden-frames.md` "Key facts") rather
  than recomputing it from the summed per-block payload lengths. Kept for byte-exactness; flag
  if a future controller rejects a zeroed length.
- ✅ **RESOLVED (final review)** — `Write`/`PrmEnd`/`Release` are now matched against the
  established AR (final review); records capped at 64 / 64 KiB.

## From Plan 4 (`rt`)

Open points recorded from the cyclic-exchange HIL run (`docs/bench-pnet-device.md` §6d,
`hil-rt-facts.md`), to be picked up by the plans noted:

- ✅ **RESOLVED (HIL 2026-08-28, `docs/bench-pnet-device.md` §6e)** — **1 ms / determinism**:
  1 ms held against a real S7-1500 on `PREEMPT_RT`, idle and under load, zero missed ticks and
  zero watchdog expirations over 2.9 million cycles with the final binary (`2ce31e2`); the only
  spec §1 criterion not met under the load pinned to CPUs 0-2 (tick lateness p99.99) is met once
  the load is kept off CPU 3's L2 sibling. See "From Plan 7 (1 ms)" below for the follow-up.
- **Per-socket BPF filters**: both `AF_PACKET` sockets currently see every `0x8892` frame
  (not just the ones addressed to each). `AfPacketTransport::recv` also allocates 1522 B per
  frame (needs a `recv_into` on the trait), and there is a double `poll` per drained frame.
- **Lock-free seqlock for the I/O image**: only worth it if `input_snapshot_reused` /
  `output_publish_deferred` become significant at 1 ms — at 32 ms, `output_publish_deferred`
  reached 19 in 6 minutes, which is harmless application-level double-buffer contention, not a
  bottleneck.
- **`PACKET_AUXDATA`** (RX VLAN priority) — not needed to operate; revisit only if a consumer
  needs the received VLAN priority.
- ✅ **RESOLVED (Plan 5, 2026-08-30)** — **ERR-RTA on device stop / `ProblemIndicator` /
  diagnosis reporting**: `IoDevice::stop()` and every device-side abort now send ERR-RTA on the
  alarm channel; `diag::DiagStore` drives a shared `AtomicBool` the RT thread reads for the
  `ProblemIndicator` bit in the cyclic data status; `raise_diagnosis`/`clear_diagnosis` on
  `IoDevice` reach the wire as `AlarmNotification`s and are replayed on the next `Data`.
  `AbortReason::RtSocket` (`crates/pnio/src/cm/ar.rs`) is now its own variant, used by
  `device::mod` for socket-level failures instead of reusing `RtWatchdog`.
- **`RtStats` cumulative across ARs**; `RtHandle::join` detaches the thread on a timeout
  (leaves a stale-runner window); `Validity.cycle` counts runner ticks, not the PROFINET cycle
  counter — revisit if a consumer needs the wire cycle counter specifically.
- **Test flake surfaces**: the runner test's lower bound (≥ 8 sends / 60 ms) and the device
  watchdog test's sleep margin are both timing-sensitive; tighten if they start flaking in CI.
- ✅ **RESOLVED (`de8479b`)** — **IOCS deadlock**: IOCS is the consumer's own status, always
  GOOD for every plugged submodule, independent of the received IOPS (matches p-net's
  behaviour and IEC semantics); mirroring the CPU's IOPS into our IOCS left the CPU's
  diagnostics buffer showing "User data failure of hardware component" for the whole run.
  `rx_iops_good` is kept but only feeds the application's `Validity`.
- ✅ **RESOLVED (`7320ed7`)** — **musl `sched_setscheduler`/`sched_setaffinity` stubbed with
  `ENOSYS`**: `--rt-priority` silently fell back on the edge even though the kernel supports
  `SCHED_FIFO`; fixed by calling the raw syscalls (`SYS_sched_setscheduler`,
  `SYS_sched_setaffinity`) directly instead of the libc wrappers.
- ✅ **RESOLVED (Plan 4)** — `PACKET_OUTGOING` frames dropped in `AfPacketTransport::recv`
  (our own transmitted frames were being looped back and misread as received traffic).

## From Plan 7 (1 ms)

Open points recorded from the 1 ms HIL campaign (`docs/bench-pnet-device.md` §6e), to be
picked up as noted:

- ✅ **RESOLVED (HIL 2026-08-28, `docs/bench-pnet-device.md` §6f)** — **L2-pair isolation (Plan
  7bis)**: `stress-ng` pinned to CPUs 0-2 (spec §1's own load) shares CPU 3's L2 cache with CPU 2
  (Atom E3940: L2 1 MiB unified, shared by CPUs 2-3, no L3) and pushed tick lateness p99.99 to
  147-203 µs under the single-core profile, above the 100 µs budget (max still < 300 µs; zero
  missed ticks either way). Isolating the whole L2 pair (`isolcpus=domain,managed_irq,2,3`,
  `HK_CPUS=0-1`), now `bench/`'s default profile, confirmed the fix: p99.99 20 µs idle / 13 µs
  under the spec's own load (now pinned to CPUs 0-1), max 22.7 µs / 22.4 µs, 0 missed ticks / 0
  watchdog expirations over 1.2 M cycles (600078 + 597969) on the same binary (`2ce31e2`) and TIA
  project as §6e. All four spec §1 criteria now met at idle and under load. Seqlock
  reused+deferred under the L2-pair profile: 0.13 % idle, 0.15 % under load — still over the
  spec §9 0.1 % line, by a slightly wider margin than the single-core profile's 0.10 %/0.11-0.12 %;
  the §9 deviation (keep `Mutex` + `try_lock`) stands, `rx_dropped=0` again in every run.
- **`PACKET_MMAP`** (TPACKET_V3 rings): out of scope for Plan 7 (kernel + isolation + minimal
  code held the budget once the load layout above is applied); revisit only if a future
  campaign under the original CPU-0-2 load pinning still needs the p99.99 budget.
- **`SO_BUSY_POLL` / a spinning RT thread**: same trigger as `PACKET_MMAP` above.
- **Triple syscall per received frame** (`SOCK_NONBLOCK` + skip `poll` on a zero timeout) —
  noted in the Plan 7 final review, out of scope for the campaign itself; revisit if
  `cycle_work` needs to shrink further.
- **Default `recv` allocates before polling** (the `EthTransport::recv` convenience method,
  not the RT-path `recv_into`) — same trigger as above.
- **`RtHandle::join`'s 500 ms timeout** leaves the histograms live (not yet consistent with a
  stopped thread) if it expires — a caveat on verdict exactness for a run stopped abnormally,
  not exercised by any campaign run (`--duration` always stopped cleanly).
- **Seqlock trigger (spec §9)**: `Mutex` + `try_lock` stays — a **deliberate deviation** from
  spec §9's own rule (< 0.1 % → `Mutex`; otherwise seqlock), since reused+deferred is 0.11-0.12 %
  under the spec's own load, 0.07-0.10 % otherwise (see `docs/bench-pnet-device.md` §6e). Kept
  anyway because every run showed zero dropped frames and the overshoot is only 0.01-0.02
  points; revisit only if a consumer needs every single cycle's outputs rather than the latest
  one — that is the real trigger, not the raw percentage.
- **`cyclictest` needs root** to open `/dev/cpu_dma_latency` (disables C-state entry during the
  measurement); without it, it warns and continues — not exercised as a failure on the bench,
  noted for anyone re-running the campaign without `sudo`.
- **`PACKET_AUXDATA`** (RX VLAN priority) — still not implemented (carried over from Plan
  3/4); not needed to operate, revisit only if a consumer needs the received VLAN priority.

## From Plan 6 (`config` / GSDML / `api`)

Open points recorded while implementing the typed configuration, generated GSDML and device
facade (`docs/gsdml.md`, `docs/bench-pnet-device.md` §6g):

- **XSD vendoring**: the PI GSDML v2.4 XSD is not in the repo (its licence has not been
  checked); the validation recipe in `docs/gsdml.md` relies on a local TIA Portal install
  instead. Vendor it (once cleared) and wire a CI step that runs the golden GSDML through it on
  every change to `gsdml::render`.
- **V2.31+ profile**: `LLDP_NoD_Supported`, `PTP_BoundarySupported`/`DCP_BoundarySupported`,
  `ResetToFactoryModes`, `CertificationInfo` all require `PNIO_Version >= "V2.31"` and none are
  declared today because the device implements none of them (see `docs/gsdml.md#validation`).
  Bump the declared `PNIO_Version` and add these attributes together, as one version-bump
  change, once LLDP/PTP-DCP-boundary/ResetToFactory support lands.
- **`CertificationInfo` / other optional DAP attributes**: beyond the V2.31+ mandates above,
  the v2.4 XSD defines further optional `DeviceAccessPointItem` attributes (certification
  claims, additional capability flags) this crate does not render any of; revisit once there is
  something honest to declare through them.
- **`with_inputs` scratch allocation per call**: `IoDevice::with_inputs` clones the slot's
  working buffer into a scratch copy on every call (`crates/pnio/src/api.rs`) so a failed/
  panicking closure can't leave a partial write behind. A persistent per-slot scratch buffer
  (reused across calls, cleared before each) would drop that allocation from the hot path;
  not done because no campaign has shown it matters yet.
- **Duplicated acyclic loop in `api`**: `run_publishing_params` (`crates/pnio/src/api.rs`)
  reimplements `Device::run`'s 200 ms-poll loop just to observe `ar_params()` on every state
  change. A `Device::run_with` hook (a callback invoked after each `step`) would let `IoDevice`
  reuse `Device::run` directly instead of duplicating its loop.
- **`ApplicationLengthIncludesIOxS`/`MaxApplication*Length` not declared**: `gsdml::render`
  emits `IOConfigData`'s `MaxInputLength`/`MaxOutputLength`/`MaxDataLength` (the CR C-SDU
  lengths, see `docs/gsdml.md#validation`) but not these related, optional v2.4 XSD attributes;
  revisit if a controller or checker ever expects them.
- **Mixed-submodule branch of the total-C-SDU guard untested**: `config::check_total_csdu`'s
  accounting for a submodule that has data in *both* directions (the `has_in && has_out` case
  in `cr_lengths`) is exercised by `mixed_submodule_has_both_directions` for the plain field
  table, but no test drives it specifically through the `TooLongTotal` guard itself. Add one.
- **`ProfinetDevice` facade (approach 2)**: the spec's alternative facade that absorbs
  `Device`/the threads entirely, rather than `IoDevice`'s thin wrapper — out of scope for Plan 6
  (§2). Revisit if `IoDevice::start_with`'s transport/runner-factory hooks prove insufficient
  for an embedding use case.
- **Application config file**: `DeviceConfig` is Rust-builder-only by design (spec §3: "same
  object renders the GSDML → no drift"); a TOML/YAML declaration format, parsed into the same
  builder, stays a possible follow-up if a deployment needs to configure the device without
  recompiling it.
- **Official Vendor ID**: `0xFFFF`/`0x0001` stay the only identity this crate ships (see
  `docs/gsdml.md#identity-caveat`); obtaining a PI-assigned Vendor ID is the user's step, not
  this crate's.
- **`read_mac` assumes standard 2-hex-digit octets**: `api::read_mac`
  (`crates/pnio/src/api.rs`) splits `/sys/class/net/<iface>/address` on `:` and parses each part
  with `u8::from_str_radix`, which happens to accept a single hex digit per octet (e.g. `"8"`
  parses the same as `"08"`) without it being a deliberate, tested contract — Linux always
  reports zero-padded two-digit octets in practice, so this has not mattered, but the function's
  behavior on a non-standard `address` file is not specified or tested.
- **`stop()` swallows a thread panic into `Ok(())`**: `IoDevice::stop`'s `h.join()` failure path
  (`crates/pnio/src/api.rs`) already logs (`log::error!`) and returns `Ok(())` on a panicking
  acyclic thread rather than propagating the panic — a deliberate simplification so a caller
  never has to handle a `Box<dyn Any>` panic payload, but it means a caller can't distinguish "a
  clean stop" from "the RT thread panicked mid-run" through the `Result` alone. Revisit if a
  caller needs that distinction.
- ✅ **RESOLVED (HIL 2026-08-29, X1)** — **500 µs bonus run**: X2 is RT-only with a fixed 1 ms send
  clock, so the device was moved to the CPU's X1 port (same IP); 5 minutes at 500 µs, `PASS`,
  0 missed ticks over 596 899 cycles, p99.99 lateness 27 µs (`docs/bench-pnet-device.md` §6g).
  250 µs would need the busy-poll / `PACKET_MMAP` items above and a sub-ms application loop.
- **Diagnostic buffer not recorded for the §6g STOP→RUN run**: §6e/§6f's STOP→RUN tests confirmed
  and recorded a clean TIA diagnostic buffer; the Plan 6 HIL run (`plan6-stoprun2`,
  `docs/bench-pnet-device.md` §6g) confirmed the AR/freshness behavior but the diagnostic buffer
  itself was not checked/recorded. Re-check it on the next STOP→RUN run with our own GSDML.

## From Plan 5 (`alarm` / `diag` / `im`)

Recorded at close-out (spec §2 "Out") while implementing the alarm channel, channel diagnosis
and I&M0-3 (`docs/superpowers/specs/2026-08-30-pnio-alarm-diag-im-design.md`):

- **Process alarms** (`AlarmType 0x0002`, `MayIssueProcessAlarm`, OB40): the `alarm::rta` codec
  parses/builds the generic `AlarmNotification` block regardless of `AlarmType`, but there is no
  `IoDevice` API to raise one and no GSDML `MayIssueProcessAlarm` claim.
- **Manufacturer-specific diagnosis codes** (`0x0100..`) and `ChannelDiagList` texts;
  `ExtChannelDiagnosis` (USI `0x8002`, `alarm::rta::ExtChannelDiagnosis`) is parse-only today —
  the codec decodes it but `diag::DiagStore` only ever builds the plain `ChannelDiagnosis` (USI
  `0x8000`); qualified channel diagnosis (`0x8003`) is not implemented either.
- **Plug/pull/return-of-submodule alarms and `ModuleDiffBlock`** are not produced — carried over
  from the Plan 3 follow-up on `ModuleDiffBlock`, still open.
- **Diagnosis record reads** (`0x800A..0x800C`, `0xF80C`, `0xE00x`) still answer PNIORW "invalid
  index": TIA's *Channel diagnostics* page issued none in the Plan 5 capture (the CPU keeps the
  state it learned from alarms), so only `0xAFF0..=0xAFF3` moved off "invalid index".
- **I&M4/5** (`IM5_Supported`), **`I&M0FilterData`** (`0xF840`) and **`RealIdentificationData`**
  (`0xF841`) are not implemented; `im::Im0`/`ImStore` cover I&M0-3 only.
- **IEC 61158-6-10 cross-check**: the `PnioStatus`/RTA constants this plan pinned (ERR-RTA
  `ErrorCode`/`ErrorDecode`/`ErrorCode1`/`ErrorCode2` values, RTA-PDU `PDUType`/`AddFlags`
  encoding) are inferred from the 2026-08-30 p-net capture and Wireshark's dissector, same as
  Plan 3's `PnioStatus` follow-up — re-verify against the purchased standard once available.
- **Loading our own DCP responder under a storm** needs a second host on the segment (carried
  over from the §6h robustness campaign, "Our own DCP responder was not exercised"); Plan 5 adds
  no new coverage here.
- **`alarm_stats()` is per AR**: `IoDevice::alarm_stats()`'s counters (`sent`/`acked`/`retries`/
  `unexpected_rx`/`send_failures`/`ack_timeouts`/`rx_err_rta`) mirror the *current* alarm channel and restart at
  0 on every reconnect (a fresh AR opens a fresh channel); only `alarm_rx_no_channel()` is
  cumulative for the process's lifetime. Accumulate across reconnects if a metrics consumer needs
  a running total.
- **`gen_gsdml`'s `info_text` vs the in-crate golden `meta()`**: fixed in this same close-out
  (see the golden-catalogue identity fix below) — noted here because it was found while writing
  this plan's docs, not because anything remains open.
- **Per-record Write status is not reported.** `cm::write::build_write_res`
  (`crates/pnio/src/cm/write.rs`) hard-codes each record's `status` field to `0`. A malformed I&M
  write is validated (`records::write_im_record` returns `write_invalid_parameter()`) and logged
  at `warn`, but the controller is told OK. Carry the per-record status into the response once
  the standard's exact per-record semantics are confirmed (see the IEC cross-check above).
- **`read_record` ignores `api` and `record_data_length`.** `crates/pnio/src/cm/records.rs`
  dispatches on `(slot, subslot, index)` only: a Read on a foreign API, or one whose
  `record_data_length` is smaller than the 60-byte I&M0 record, is answered as if it asked for
  API 0 with room to spare. The capture only ever shows API 0 and `0x8000` bytes.
- **`PnioStatus::read_wrong_ar()` / `write_invalid_parameter()` codes are conventions.**
  `0xDE 81 3D 03` and `0xDF 81 B0 02` (`crates/pnio/src/cm/status.rs`) were chosen to match what
  the dissector names, not verified against IEC 61158-6-10; no capture contains either (p-net was
  never asked a foreign-AR Read nor a bad Write). Fold into the IEC cross-check.
- **No test for an unwritable `im_store` on persist.** Spec §8 says a write failure logs at
  `error` and the Write is still answered OK, and `ImStore::persist` implements it, but nothing
  exercises the failing branch (read-only directory, full disk). Likewise there is **no test for
  the sequence wraps**: `AlarmSpecifier.sequence` at `0x7FF` and the RTA `SendSeqNum` at `0x7FFF`
  are implemented (`crates/pnio/src/alarm/channel.rs`) and never reached by any test.
- **`AlarmChannel::err_rta` takes `&mut self` needlessly**: it only reads `cfg`/`last_sent_seq`/
  `last_rx_seq` and builds a frame. `&self` would let a caller build the abort frame while
  holding a shared borrow; the signature is the only reason it cannot.
- **`Diagnosis.direction` is a placeholder on the API path.** `IoDevice::raise_diagnosis` takes a
  `Diagnosis` whose `direction` the caller fills in, then `DiagStore` overwrites it from the
  device model's submodule (`crates/pnio/src/diag.rs`) — the field is derived, never an input.
  Consider dropping it from the constructor/API type so callers cannot believe they set it.
- **`ImStore`'s temp file is PID-only and leaks on a failed rename.** `.im-{pid}.tmp`
  (`crates/pnio/src/im.rs`) collides if two devices in one process share a store path, and a
  failed `rename` leaves the temp file behind (no `remove_file` on the error path).
- **`Cm` holds a second `DeviceModel` clone.** `Cm::new` keeps `model.clone()` alongside the one
  it hands `Ar::new` (`crates/pnio/src/cm/mod.rs`), so every AR carries two copies of the slot
  table. An `Rc`/`Arc`, or reading the model back through `self.ar`, would remove the duplicate.
- **`device/mod.rs` is ~1350 lines** and now carries the socket loop, the alarm plumbing, the
  diagnosis queue drain and the ERR-RTA abort paths. Split the alarm/diagnosis half out before it
  grows again.
- **`SwRevision` has no `Display`.** `{prefix}{functional}.{bug_fix}.{internal}` is formatted by
  hand in `gsdml::module_identity_xml` and in the docs/tests; one `impl Display` would make the
  format single-sourced.
- **Spec §2 "Out" items missing from the list above**, recorded here for completeness:
  **AR/API-level diagnosis** (only channel diagnosis is produced), **`MaintenanceStatus` alarm
  items** (the maintenance-required/-demanded qualifiers are neither encoded nor claimed), and
  **controller-initiated alarms other than Alarm-Ack** (a CPU sends none to an IO-Device, so any
  other DATA from it is counted `UnexpectedRx` and dropped).
- **Design note — the `AwaitAlarmAck` timeout no longer aborts the AR** (ruled at final review,
  spec §5.2/§8): once the transport ACK confirms delivery, a missing content-level Alarm-Ack for
  `10 x rta_timeout` drops the alarm, counts `AlarmStats::ack_timeouts` and leaves the AR up,
  where the first implementation resent the DATA and eventually aborted. Rationale: a CPU in STOP,
  or one whose application is simply slow, must not cost us the AR. **Verify at HIL with the CPU
  in STOP** (§6i): confirm the device stays connected, the counter moves, and the CPU picks the
  diagnosis up again on the next Alarm-Ack it does send (or via the replay on reconnect).
- **ERR-RTA for aborts before `Data`** (`AbortReason::ActivityTimeout`/`AppReadyFailed`) is
  impossible by construction, not a gap: `device::open_alarm_channel` only creates the alarm
  channel on a `Data` notify (`crates/pnio/src/device/mod.rs`), so `err_rta_code2` can compute a
  code for these reasons but `abort_with_err_rta_in`'s `self.alarm.as_mut()` guard is `None` at
  that point and no frame is sent — there is no channel yet to send it on.

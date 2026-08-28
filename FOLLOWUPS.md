# Tracked Follow-ups (from Plan 1 Branch Review)

Non-blocking findings for Plan 1, to be integrated into the briefs of the relevant plans.

## For Plan 4 (`rt` cyclic / RT thread)
- ✅ **RESOLVED (merge 459963d)** — **Kernel filtering & busy-spin**: `AfPacketTransport::open`
  now binds with `sll_protocol = htons(0x8892)` and joins the PROFINET multicast group
  (`01:0e:cf:00:00:00`), so the kernel only wakes `recv` on PROFINET frames — no idle spin on
  unrelated broadcast traffic. Used by the Plan 3 acyclic loop (`device`, merge 896dfd5) since.
- ✅ **RESOLVED (merge 459963d, fix 174cb4d)** — **`recv` timeout**: implemented via `poll`
  (timeouts rounded up to the millisecond); exercised continuously by the Plan 3 device loop's
  `step`/`run` (`crates/profinet-rt/src/device/mod.rs`).
- **MSG_TRUNC**: `recv` does not handle MSG_TRUNC (non-issue for standard RT frames ≤1522).

## For Plan 2 (`dcp`) — before frame-exact comparisons
- ✅ **RESOLVED (merge ba63901)** — **Typed `CaptureError`**: `Io(#[from] std::io::Error)` +
  `Pcap(#[from] pcap_file::PcapError)` + `UnknownFormat([u8;4])`. **`PcapFrames` reads both
  pcap and pcapng** (magic auto-detection) and the iterator returns
  `Result<Vec<u8>, CaptureError>` (no more swallowing). ✅ **`TransportError::Io` now typed**
  too (merge f4de284): `Io(#[from] std::io::Error)` + `From<nix::errno::Errno>` in the
  AF_PACKET backend — cross-module consistency done.

## For Plan 6 (`config` / GSDML / typed API)
- ✅ **RESOLVED (bench 2026-08-27)** — **BOOL bit ordering (LSB-first) verified on the wire**
  with a real S7-1500 (1515-2 PN FW V2.9.4) ↔ p-net device: `%Q0.0 := TRUE` alone → output
  byte `0x01` in the RTC1 frame (`captures/q-bits-2026-08-27-165102.pcapng`); device input
  byte `0x80` (Button1) → `%I0.7 = TRUE` in TIA (`captures/io-bits-2026-08-27-164448.pcapng`).
  `data::get_bit`/`set_bit` (`1 << (i % 8)`) is correct. Still to do in Plan 6: add a test
  vector from the capture, and check the declaration→(byte, bit) mapping for our own GSDML.
- **`data::Value` pending use**: the `Value` enum is a forward declaration (no
  constructor/consumer yet). Plan 6 must either wire it up (typed dispatch
  `encode(Value)->bytes` / `decode(FieldType,&[u8])->Value`) or remove it (YAGNI).
- **`Field`/`FieldType` naming consistency**: the API sketch in the spec (§5.4) uses
  `Field::Real`, the code uses `FieldType::Real`. To be reconciled in Plan 6.

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
  caller, not propagated: the Plan 3 acyclic device loop (`crates/profinet-rt/src/device/mod.rs`)
  catches per-frame errors from the DCP/RPC dispatch, logs them, and continues the loop rather
  than aborting the process on a single bad frame.

## From Plan 3 (`rpc` + `cm`)

Open points recorded from the AR-establishment HIL run (`docs/bench-pnet-device.md` §6c,
`hil-facts.md`), to be picked up by the plans noted:

- **Minimal `Read`/`ReadImplicit` support** (index `0xfbff` "RPC connection monitoring
  trigger", plus I&M/diagnosis reads) — currently refused with a generic PNIORW "invalid
  index"; the CPU accepts that as keep-alive today, but real diagnostics need real reads.
  Plan 5.
- **Alarm channel (ERR-RTA / ACK-RTA, frame IDs `0xfe01`/`0xfc01`)** not implemented — an
  aborted AR is currently only noticed indirectly via the controller's reconnect sequence
  (now handled), not via the alarm channel itself. Plan 5.
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

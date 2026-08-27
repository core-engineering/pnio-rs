# Tracked Follow-ups (from Plan 1 Branch Review)

Non-blocking findings for Plan 1, to be integrated into the briefs of the relevant plans.

## For Plan 4 (`rt` cyclic / RT thread)
- **Kernel filtering & busy-spin**: `AfPacketTransport::recv` opens with `ETH_P_ALL` and
  returns `Ok(None)` for any non-PROFINET frame → a naive polling loop may spin idle on
  broadcast traffic. Install a BPF filter (`SO_ATTACH_FILTER`) or bind with
  `sll_protocol = htons(0x8892)` so the kernel only wakes `recv` on PROFINET frames.
  Coupled to the `sll_protocol` point (a single knob, in `open`).
- **`recv` timeout**: the `_timeout` parameter is not implemented (via `SO_RCVTIMEO`
  or `poll`). To be implemented when the RT loop requires it.
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

### Still open
- **`DeviceRole` encoded as u16** (role+reserved) — byte-exact vs golden (role=0);
  re-verify when role≠0 on a real device.

### RX error policy (review recommendation)
- `handle_dcp_frame` returns `Err` on a malformed/short frame; a real RX loop should
  **log+drop** rather than propagate. To be documented on the caller side (Plan 3/4).

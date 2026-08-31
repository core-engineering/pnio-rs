# PROFINET-RT Plan 3 — `rpc` + `cm` AR Establishment Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Take the IO-Device from "answers DCP" to **AR state DATA** against a real S7-1500: answer DCE-RPC Connect / Write / Control(PrmEnd), then call Control(ApplicationReady) on the controller and get it acknowledged — byte-exact against the 2026-08-27 bench capture, then live on the edge.

**Architecture:** Two new pure codec modules, `rpc` (DCE-RPC v4 connectionless header, NDR, UUIDs) and `cm` (PNIO blocks, `DeviceModel`, pure AR state machine emitting `Action`s), a minimal DCP Set IP in `dcp`, and one blocking acyclic loop in `device` (`poll` over the AF_PACKET and UDP sockets) that executes the actions. No socket in `rpc`/`cm`; everything is testable with the golden frames and mock transports. The HIL example clones the p-net sample identity so the TIA project stays untouched.

**Tech Stack:** Rust stable (1.96), `thiserror`, `nix`/`libc` (AF_PACKET, `poll`), std `UdpSocket`; new deps: `log` (crate), `env_logger` + `clap` (example only).

**Spec:** `docs/design/2026-08-27-profinet-rt-cm-ar-design.md` — read it first; every task below argues from it. Bench facts: `docs/bench-pnet-device.md` §6b.

## Global Constraints

- Pure Rust; new dependencies limited to `log` (lib), `env_logger` and `clap` (example / dev only).
- PNIO blocks are **always big-endian**; the DCE-RPC header and NDR counters follow the request's DREP on parse; we **always emit big-endian** (`drep = [0, 0, 0]`).
- `rustfmt` `max_width = 100`; `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all` must pass (that is the CI).
- All cargo commands: `. "$HOME/.cargo/env" && cargo ...` (rustup is not on PATH by default).
- Existing 46 tests + doctest are sacred; golden byte-exact tests are never weakened to pass.
- Errors are typed (`thiserror`), never silent: unparsable input → `Err`, refused request → RPC response with a non-zero `PnioStatus`.
- Golden frames come from `captures/ar-connect-2026-08-27-164334.pcapng` (unversioned); their bytes are pinned in `crates/profinet-rt/testdata/cm/*.hex` (Task 1) and documented in `docs/cm-golden-frames.md`.
- Branch: `feat/cm-ar`, base `main` (HEAD `216130e`). Commit after every task; push after every commit (WSL/NTFS corruption mitigation).
- Project language: English (code, comments, docs, commit messages).

---

## File Structure

Create:
- `crates/profinet-rt/testdata/cm/*.hex` — golden frames as hex text (Task 1)
- `crates/profinet-rt/src/testutil.rs` — `#[cfg(test)]` hex loader (Task 1)
- `crates/profinet-rt/tests/common/mod.rs` — same loader for integration tests (Task 1)
- `docs/cm-golden-frames.md` — provenance + field breakdown (Task 1)
- `crates/profinet-rt/src/rpc/{mod.rs,uuid.rs,header.rs,ndr.rs,transport.rs,udp.rs}` (Tasks 2, 3, 11)
- `crates/profinet-rt/src/cm/{mod.rs,status.rs,block.rs,model.rs,connect.rs,write.rs,control.rs,ar.rs}` (Tasks 4-9)
- `crates/profinet-rt/src/dcp/set.rs` (Task 10)
- `crates/profinet-rt/src/device/mod.rs` (Task 12)
- `crates/profinet-rt/examples/ar_bringup.rs`, `crates/profinet-rt/tests/ar_replay.rs` (Task 13)

Modify:
- `crates/profinet-rt/Cargo.toml` — `log`; dev/example deps (Task 1)
- `crates/profinet-rt/src/lib.rs` — `pub mod rpc; pub mod cm; pub mod device;` + `#[cfg(test)] mod testutil;`
- `crates/profinet-rt/src/dcp/mod.rs` — dispatch `FrameId::GetSet` (Task 10)
- `crates/profinet-rt/src/eth/afpacket.rs` — `sll_protocol`, multicast membership, `poll` timeout (Task 11)
- `crates/profinet-rt/src/eth/transport.rs` — `recv` doc no longer says timeout is ignored (Task 11)
- `FOLLOWUPS.md`, `docs/bench-pnet-device.md` §6c, `README.md` status table (Task 13)

Golden frame inventory (full Ethernet frames; RPC ones are untagged IPv4/UDP so the RPC PDU starts at byte 42; the DCP Set request is VLAN-tagged so its FrameID starts at byte 18):

| File | Capture frame | Content | Bytes |
|---|---|---|---|
| `ident_ok_pnet.hex` | #37 | p-net Ident Ok (structure only: IP block reports 0.0.0.0, Dev-Role `01 01`) | 144 |
| `dcp_set_req.hex` | #47 | CPU DCP Set IP suite (VLAN-tagged, prio 0) | 64 |
| `dcp_set_res.hex` | #48 | p-net Set Ok | 34 |
| `connect_req.hex` | #50 | CPU Connect request (DREP LE) | 699 |
| `connect_res.hex` | #53 | p-net Connect response (DREP BE) | 232 |
| `write_req.hex` | #54 | CPU Write MultipleWrite | 486 |
| `write_res.hex` | #56 | p-net Write response | 462 |
| `prmend_req.hex` | #57 | CPU Control PrmEnd | 174 |
| `prmend_res.hex` | #58 | p-net Control Done | 174 |
| `appready_req.hex` | #59 | p-net → CPU Control ApplicationReady (DREP BE) | 174 |
| `appready_res.hex` | #60 | CPU → p-net Done (DREP LE, flags1 0x0a) | 174 |

---

### Task 1: Pin the golden frames + test loaders + `log` dependency

**Files:**
- Create: `crates/profinet-rt/testdata/cm/*.hex` (11 files), `crates/profinet-rt/src/testutil.rs`, `crates/profinet-rt/tests/common/mod.rs`, `docs/cm-golden-frames.md`
- Modify: `crates/profinet-rt/Cargo.toml`, `crates/profinet-rt/src/lib.rs`

**Interfaces:**
- Produces: `crate::testutil::golden(name: &str) -> Vec<u8>` (unit tests) and `common::golden(name) -> Vec<u8>` (integration tests); both read `testdata/cm/<name>.hex`, ignore `#` comments and whitespace.
- Produces: `crate::testutil::{RPC_OFF, VLAN_PAYLOAD_OFF}` = `42`, `18`.

- [ ] **Step 1: Create the branch**

```bash
git checkout -b feat/cm-ar main
```

- [ ] **Step 2: Write the hex files**

Create `crates/profinet-rt/testdata/cm/` and write each file with the bytes below (16 per line, `#` comment lines allowed). Source: `captures/ar-connect-2026-08-27-164334.pcapng`, decoded with tshark 4.6.6.

`ident_ok_pnet.hex`:
```
# frame 37: p-net DCP Ident Ok (Xid 0x0300012c), 144 bytes
ec 1c 5d 61 e7 3f 8c f3 19 cd 19 f8 88 92 fe ff
05 01 03 00 01 2c 00 00 00 76 01 02 00 0e 00 00
00 00 00 00 00 00 00 00 00 00 00 00 02 01 00 1a
00 00 50 2d 4e 65 74 20 53 61 6d 70 6c 65 20 41
70 70 6c 69 63 61 74 69 6f 6e 02 02 00 0d 00 00
72 74 2d 6c 61 62 73 2d 64 65 76 00 02 03 00 06
00 00 04 93 00 02 02 04 00 04 00 00 01 01 02 05
00 1e 00 00 01 02 01 01 02 01 02 02 02 03 02 04
02 05 02 06 05 01 05 02 05 03 05 05 05 06 ff ff
```

`dcp_set_req.hex`:
```
# frame 47: CPU DCP Set IP (VLAN tagged, Xid 0x0300012d), 64 bytes
8c f3 19 cd 19 f8 ec 1c 5d 61 e7 3f 81 00 00 00
88 92 fe fd 04 00 03 00 01 2d 00 00 00 12 01 02
00 0e 00 00 ac 10 02 0a ff ff ff 00 ac 10 02 0a
00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
```

`dcp_set_res.hex`:
```
# frame 48: p-net DCP Set Ok, 34 bytes
ec 1c 5d 61 e7 3f 8c f3 19 cd 19 f8 88 92 fe fd
04 01 03 00 01 2d 00 00 00 08 05 04 00 03 01 02
00 00
```

`connect_req.hex`:
```
# frame 50: CPU Connect request, DREP LE, 699 bytes (RPC PDU at offset 42)
8c f3 19 cd 19 f8 ec 1c 5d 61 e7 3f 08 00 45 00
02 ad ea 68 00 00 40 11 31 49 ac 10 02 64 ac 10
02 0a d5 ee 88 94 02 99 3a a4 04 00 20 00 10 00
00 00 00 00 a0 de 97 6c d1 11 82 71 00 01 00 02
04 93 01 00 a0 de 97 6c d1 11 82 71 00 a0 24 42
df 7d ae a1 ac d2 32 00 10 10 b0 58 ec 1c 5d 61
e7 3f 00 00 00 00 01 00 00 00 00 00 00 00 00 00
ff ff ff ff 41 02 00 00 00 00 2d 02 00 00 2d 02
00 00 2d 02 00 00 00 00 00 00 2d 02 00 00 01 01
00 5b 01 00 00 01 e5 e1 ae cc b1 33 4b 4d b1 87
cc 68 b0 21 1e d2 00 02 ec 1c 5d 61 e7 3f de a0
00 00 6c 97 11 d1 82 71 10 64 01 0e 00 2a 40 00
00 11 00 c8 88 92 00 25 70 6c 63 78 62 62 65 6e
63 68 2e 70 72 6f 66 69 6e 65 74 78 61 69 6e 74
65 72 66 61 63 65 78 62 32 35 66 62 64 01 02 00
68 01 00 00 01 00 01 88 92 00 00 00 02 00 28 80
00 00 20 00 20 00 01 00 00 ff ff ff ff 00 03 00
03 c0 00 00 00 00 00 00 00 00 01 00 00 00 00 00
06 00 00 00 01 00 00 00 00 80 00 00 01 00 00 80
01 00 02 00 01 00 01 00 03 00 03 00 01 00 06 00
04 00 01 00 09 00 03 00 02 00 01 00 05 00 03 00
01 00 08 00 04 00 01 00 12 01 02 00 68 01 00 00
02 00 02 88 92 00 00 00 02 00 28 80 01 00 20 00
20 00 01 00 00 ff ff ff ff 00 03 00 03 c0 00 00
00 00 00 00 00 00 01 00 00 00 00 00 03 00 02 00
01 00 04 00 03 00 01 00 07 00 04 00 01 00 0a 00
06 00 00 00 01 00 00 00 00 80 00 00 01 00 00 80
01 00 02 00 01 00 01 00 03 00 03 00 01 00 06 00
04 00 01 00 09 01 04 00 3c 01 00 00 01 00 00 00
00 00 00 00 00 00 01 00 00 00 03 00 01 00 00 00
01 00 00 00 01 00 00 01 01 80 00 00 00 80 00 00
00 00 01 00 00 01 01 80 01 00 00 80 01 00 00 00
01 00 00 01 01 01 04 00 20 01 00 00 01 00 00 00
00 00 01 00 00 00 30 00 00 00 01 00 01 00 00 01
30 00 01 00 01 00 01 01 01 01 04 00 20 01 00 00
01 00 00 00 00 00 02 00 00 00 31 00 00 00 01 00
01 00 00 01 31 00 02 00 02 00 01 01 01 01 04 00
26 01 00 00 01 00 00 00 00 00 03 00 00 00 32 00
00 00 01 00 01 00 00 01 32 00 03 00 01 00 01 01
01 00 02 00 01 01 01 01 04 00 26 01 00 00 01 00
00 00 00 00 04 00 00 00 40 00 00 00 01 00 01 00
00 01 40 00 03 00 01 00 08 01 01 00 02 00 08 01
01 01 03 00 16 01 00 00 01 88 92 00 00 00 00 00
01 00 03 00 00 01 00 c0 00 a0 00
```

`connect_res.hex`:
```
# frame 53: p-net Connect response, DREP BE, 232 bytes (RPC PDU at offset 42)
ec 1c 5d 61 e7 3f 8c f3 19 cd 19 f8 08 00 45 00
00 da 90 22 40 00 40 11 4d 62 ac 10 02 0a ac 10
02 64 88 94 d5 ee 00 c6 5d 66 04 02 28 00 00 00
00 00 de a0 00 00 6c 97 11 d1 82 71 00 01 00 02
04 93 de a0 00 01 6c 97 11 d1 82 71 00 a0 24 42
df 7d d2 ac a1 ae 00 32 10 10 b0 58 ec 1c 5d 61
e7 3f 00 00 00 00 00 00 00 01 00 00 00 00 00 00
ff ff ff ff 00 6e 00 00 00 00 00 00 00 00 00 00
00 5a 00 00 02 2d 00 00 00 00 00 00 00 5a 81 01
00 1e 01 00 00 01 e5 e1 ae cc b1 33 4b 4d b1 87
cc 68 b0 21 1e d2 00 02 8c f3 19 cd 19 f8 88 92
81 02 00 08 01 00 00 01 00 01 80 00 81 02 00 08
01 00 00 02 00 02 80 01 81 03 00 08 01 00 00 01
00 00 00 c8 81 06 00 10 01 00 00 0b 72 74 2d 6c
61 62 73 2d 64 65 76 00
```

`write_req.hex`:
```
# frame 54: CPU Write MultipleWrite, DREP LE, 486 bytes
8c f3 19 cd 19 f8 ec 1c 5d 61 e7 3f 08 00 45 00
01 d8 c4 48 00 00 40 11 58 3e ac 10 02 64 ac 10
02 0a d5 ee 88 94 01 c4 58 9b 04 00 20 00 10 00
00 00 00 00 a0 de 97 6c d1 11 82 71 00 01 00 02
04 93 01 00 a0 de 97 6c d1 11 82 71 00 a0 24 42
df 7d ae a1 ac d2 32 00 10 10 b0 58 ec 1c 5d 61
e7 3f 00 00 00 00 01 00 00 00 01 00 00 00 03 00
ff ff ff ff 6c 01 00 00 00 00 58 01 00 00 58 01
00 00 58 01 00 00 00 00 00 00 58 01 00 00 00 08
00 3c 01 00 00 00 e5 e1 ae cc b1 33 4b 4d b1 87
cc 68 b0 21 1e d2 ff ff ff ff ff ff ff ff 00 00
e0 40 00 00 01 18 00 00 00 00 00 00 00 00 00 00
00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 08
00 3c 01 00 00 01 e5 e1 ae cc b1 33 4b 4d b1 87
cc 68 b0 21 1e d2 00 00 00 00 00 00 80 00 00 00
80 71 00 00 00 0c 00 00 00 00 00 00 00 00 00 00
00 00 00 00 00 00 00 00 00 00 00 00 00 00 02 50
00 08 01 00 00 00 00 00 00 01 00 08 00 3c 01 00
00 02 e5 e1 ae cc b1 33 4b 4d b1 87 cc 68 b0 21
1e d2 00 00 00 00 00 03 00 01 00 00 00 7b 00 00
00 04 00 00 00 00 00 00 00 00 00 00 00 00 00 00
00 00 00 00 00 00 00 00 00 00 00 00 00 01 00 08
00 3c 01 00 00 03 e5 e1 ae cc b1 33 4b 4d b1 87
cc 68 b0 21 1e d2 00 00 00 00 00 03 00 01 00 00
00 7c 00 00 00 04 00 00 00 00 00 00 00 00 00 00
00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
00 02 00 08 00 3c 01 00 00 04 e5 e1 ae cc b1 33
4b 4d b1 87 cc 68 b0 21 1e d2 00 00 00 00 00 04
00 01 00 00 00 7d 00 00 00 04 00 00 00 00 00 00
00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
00 00 00 00 00 02
```

`write_res.hex`:
```
# frame 56: p-net Write response, DREP BE, 462 bytes
ec 1c 5d 61 e7 3f 8c f3 19 cd 19 f8 08 00 45 00
01 c0 90 23 40 00 40 11 4c 7b ac 10 02 0a ac 10
02 64 88 94 d5 ee 01 ac 5e 4c 04 02 28 00 00 00
00 00 de a0 00 00 6c 97 11 d1 82 71 00 01 00 02
04 93 de a0 00 01 6c 97 11 d1 82 71 00 a0 24 42
df 7d d2 ac a1 ae 00 32 10 10 b0 58 ec 1c 5d 61
e7 3f 00 00 00 00 00 00 00 01 00 00 00 01 00 03
ff ff ff ff 01 54 00 00 00 00 00 00 00 00 00 00
01 40 00 00 01 58 00 00 00 00 00 00 01 40 80 08
00 3c 01 00 00 00 e5 e1 ae cc b1 33 4b 4d b1 87
cc 68 b0 21 1e d2 ff ff ff ff ff ff ff ff 00 00
e0 40 00 00 00 00 00 00 00 00 00 00 00 00 00 00
00 00 00 00 00 00 00 00 00 00 00 00 00 00 80 08
00 3c 01 00 00 01 e5 e1 ae cc b1 33 4b 4d b1 87
cc 68 b0 21 1e d2 00 00 00 00 00 00 80 00 00 00
80 71 00 00 00 00 00 00 00 00 00 00 00 00 00 00
00 00 00 00 00 00 00 00 00 00 00 00 00 00 80 08
00 3c 01 00 00 02 e5 e1 ae cc b1 33 4b 4d b1 87
cc 68 b0 21 1e d2 00 00 00 00 00 03 00 01 00 00
00 7b 00 00 00 00 00 00 00 00 00 00 00 00 00 00
00 00 00 00 00 00 00 00 00 00 00 00 00 00 80 08
00 3c 01 00 00 03 e5 e1 ae cc b1 33 4b 4d b1 87
cc 68 b0 21 1e d2 00 00 00 00 00 03 00 01 00 00
00 7c 00 00 00 00 00 00 00 00 00 00 00 00 00 00
00 00 00 00 00 00 00 00 00 00 00 00 00 00 80 08
00 3c 01 00 00 04 e5 e1 ae cc b1 33 4b 4d b1 87
cc 68 b0 21 1e d2 00 00 00 00 00 04 00 01 00 00
00 7d 00 00 00 00 00 00 00 00 00 00 00 00 00 00
00 00 00 00 00 00 00 00 00 00 00 00 00 00
```

`prmend_req.hex`:
```
# frame 57: CPU Control PrmEnd, DREP LE, 174 bytes
8c f3 19 cd 19 f8 ec 1c 5d 61 e7 3f 08 00 45 00
00 a0 7e 5e 00 00 40 11 9f 60 ac 10 02 64 ac 10
02 0a d5 ee 88 94 00 8c d4 51 04 00 20 00 10 00
00 00 00 00 a0 de 97 6c d1 11 82 71 00 01 00 02
04 93 01 00 a0 de 97 6c d1 11 82 71 00 a0 24 42
df 7d ae a1 ac d2 32 00 10 10 b0 58 ec 1c 5d 61
e7 3f 00 00 00 00 01 00 00 00 02 00 00 00 04 00
ff ff ff ff 34 00 00 00 00 00 20 00 00 00 20 00
00 00 20 00 00 00 00 00 00 00 20 00 00 00 01 10
00 1c 01 00 00 00 e5 e1 ae cc b1 33 4b 4d b1 87
cc 68 b0 21 1e d2 00 02 00 00 00 01 00 00
```

`prmend_res.hex`:
```
# frame 58: p-net Control PrmEnd Done, DREP BE, 174 bytes
ec 1c 5d 61 e7 3f 8c f3 19 cd 19 f8 08 00 45 00
00 a0 90 24 40 00 40 11 4d 9a ac 10 02 0a ac 10
02 64 88 94 d5 ee 00 8c 5d 2c 04 02 28 00 00 00
00 00 de a0 00 00 6c 97 11 d1 82 71 00 01 00 02
04 93 de a0 00 01 6c 97 11 d1 82 71 00 a0 24 42
df 7d d2 ac a1 ae 00 32 10 10 b0 58 ec 1c 5d 61
e7 3f 00 00 00 00 00 00 00 01 00 00 00 02 00 04
ff ff ff ff 00 34 00 00 00 00 00 00 00 00 00 00
00 20 00 00 00 20 00 00 00 00 00 00 00 20 81 10
00 1c 01 00 00 00 e5 e1 ae cc b1 33 4b 4d b1 87
cc 68 b0 21 1e d2 00 02 00 00 00 08 00 00
```

`appready_req.hex`:
```
# frame 59: p-net -> CPU Control ApplicationReady, DREP BE, 174 bytes (src port 49153)
ec 1c 5d 61 e7 3f 8c f3 19 cd 19 f8 08 00 45 00
00 a0 90 25 40 00 40 11 4d 99 ac 10 02 0a ac 10
02 64 c0 01 88 94 00 8c 5d 2c 04 00 20 00 00 00
00 00 de a0 00 00 6c 97 11 d1 82 71 10 64 01 0e
00 2a de a0 00 02 6c 97 11 d1 82 71 00 a0 24 42
df 7d 14 af 19 8a 12 34 10 56 80 79 8c f3 19 cd
19 f8 00 00 00 00 00 00 00 01 00 00 00 00 00 04
ff ff ff ff 00 34 00 00 00 00 00 00 05 3c 00 00
00 20 00 00 05 3c 00 00 00 00 00 00 00 20 01 12
00 1c 01 00 00 00 e5 e1 ae cc b1 33 4b 4d b1 87
cc 68 b0 21 1e d2 00 02 00 00 00 02 00 00
```

`appready_res.hex`:
```
# frame 60: CPU -> p-net ApplicationReady Done, DREP LE, flags1 0x0a, 174 bytes
8c f3 19 cd 19 f8 ec 1c 5d 61 e7 3f 08 00 45 00
00 a0 7a 32 00 00 40 11 a3 8c ac 10 02 64 ac 10
02 0a dc 68 c0 01 00 8c 48 1b 04 02 0a 00 10 00
00 00 00 00 a0 de 97 6c d1 11 82 71 10 64 01 0e
00 2a 02 00 a0 de 97 6c d1 11 82 71 00 a0 24 42
df 7d 8a 19 af 14 34 12 56 10 80 79 8c f3 19 cd
19 f8 56 51 00 00 01 00 00 00 00 00 00 00 04 00
ff ff ff ff 34 00 00 00 00 00 00 00 00 00 20 00
00 00 3c 05 00 00 00 00 00 00 20 00 00 00 81 12
00 1c 01 00 00 00 e5 e1 ae cc b1 33 4b 4d b1 87
cc 68 b0 21 1e d2 00 02 00 00 00 08 00 00
```

- [ ] **Step 3: Write the loaders (with their own tests)**

`crates/profinet-rt/src/testutil.rs`:
```rust
//! Test-only helpers: golden frame loading from `testdata/cm/*.hex`.

/// Offset of the DCE-RPC PDU inside an untagged IPv4/UDP Ethernet frame (14 + 20 + 8).
pub const RPC_OFF: usize = 42;
/// Offset of the PROFINET FrameID inside a VLAN-tagged Ethernet frame (14 + 4).
pub const VLAN_PAYLOAD_OFF: usize = 18;

/// Parse a hex dump: `#` starts a comment to end of line, whitespace separates bytes.
pub fn parse_hex(text: &str) -> Vec<u8> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .flat_map(|l| l.split_whitespace().map(|b| u8::from_str_radix(b, 16).expect("hex byte")))
        .collect::<Vec<u8>>()
}

/// Load `testdata/cm/<name>.hex` relative to the crate root.
pub fn golden(name: &str) -> Vec<u8> {
    let path = format!("{}/testdata/cm/{name}.hex", env!("CARGO_MANIFEST_DIR"));
    parse_hex(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_skips_comments_and_whitespace() {
        assert_eq!(parse_hex("# c\n01 02\n  0a # tail\n"), vec![1, 2, 10]);
    }

    #[test]
    fn golden_files_have_expected_lengths() {
        for (name, len) in [
            ("ident_ok_pnet", 144), ("dcp_set_req", 64), ("dcp_set_res", 34),
            ("connect_req", 699), ("connect_res", 232), ("write_req", 486), ("write_res", 462),
            ("prmend_req", 174), ("prmend_res", 174), ("appready_req", 174), ("appready_res", 174),
        ] {
            assert_eq!(golden(name).len(), len, "{name}");
        }
    }
}
```

`crates/profinet-rt/tests/common/mod.rs` — same two functions (`parse_hex`, `golden`) and the two constants, without the test module (integration tests cannot see `crate::testutil`). Mark `#![allow(dead_code)]` at the top: not every integration test uses everything.

In `crates/profinet-rt/src/lib.rs` add after `pub mod eth;`:
```rust
#[cfg(test)]
pub(crate) mod testutil;
```

- [ ] **Step 4: Dependencies**

In `crates/profinet-rt/Cargo.toml`:
```toml
[dependencies]
libc = "0.2"
log = "0.4"
nix = { version = "0.27", default-features = false, features = ["net", "socket", "poll"] }
pcap-file = "2"
thiserror = "1"

[dev-dependencies]
clap = { version = "4", features = ["derive"] }
env_logger = "0.11"
```
(`nix` gains the `poll` feature for Task 11; `clap`/`env_logger` serve `examples/` and tests only.)

- [ ] **Step 5: Run the tests**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt testutil -- --nocapture`
Expected: 2 passed (lengths match; if a length differs, a hex file has a typo — fix the file, never the expected length).

- [ ] **Step 6: Write `docs/cm-golden-frames.md`**

Content: provenance paragraph (bench 2026-08-27, CPU 1515-2 PN FW V2.9.4, p-net v0.2.0, tshark 4.6.6, capture file name, VLAN/offset notes), the inventory table above, and the field breakdown of each RPC PDU (copy the layout tables from Tasks 2-7: RPC header offsets, NDR, block-by-block for Connect req/res, Write, Control, DCP Set). Key facts to state: CPU requests carry `drep = 10 00 00` (LE), p-net answers `00 00 00` (BE) with `flags1 = 0x28`; the response's NDR `max_count` **echoes the request's `args_max`** (557 / 344 / 32); p-net answers `MaxAlarmDataLength = 200` to a request of 256; the MultipleWrite response's outer `record_data_length` is 0 (mirrored for byte-exactness); DCP Set response block is `05 04 00 03 | 01 02 00 | 00`.

- [ ] **Step 7: Commit + push**

```bash
git add crates/profinet-rt/testdata crates/profinet-rt/src/testutil.rs crates/profinet-rt/tests/common crates/profinet-rt/src/lib.rs crates/profinet-rt/Cargo.toml Cargo.lock docs/cm-golden-frames.md
git commit -m "test(cm): pin AR/DCP-Set golden frames from bench 2026-08-27 + hex loaders"
git push -u origin feat/cm-ar
```

---

### Task 2: `rpc::uuid` + `rpc::header` — DCE-RPC v4 connectionless header

**Files:**
- Create: `crates/profinet-rt/src/rpc/mod.rs`, `crates/profinet-rt/src/rpc/uuid.rs`, `crates/profinet-rt/src/rpc/header.rs`
- Modify: `crates/profinet-rt/src/lib.rs` (`pub mod rpc;`)

**Interfaces:**
- Produces `rpc::Uuid([u8; 16])` (RFC 4122 field order in memory: `time_low` BE at 0..4, `time_mid` 4..6, `time_hi` 6..8, `clock_seq` 8..10, `node` 10..16); `Uuid::parse_str("dea00001-6c97-11d1-8271-00a02442df7d")`, `Uuid::read(buf, drep)`, `Uuid::write(&self, out, drep)`, `Display` as canonical lowercase; constants `PNIO_DEVICE_INTERFACE`, `PNIO_CONTROLLER_INTERFACE`, `Uuid::pnio_object(instance: u16, device_id: u16, vendor_id: u16)`.
- Produces `rpc::Drep { little_endian: bool }` with `Drep::BIG`, `Drep::from_byte(u8)`, `to_bytes() -> [u8; 3]`, `u16(&self, &[u8]) -> u16`, `u32`, `put_u16(&self, &mut Vec<u8>, u16)`, `put_u32`.
- Produces `rpc::RpcHeader { ptype: PacketType, flags1: u8, flags2: u8, drep: Drep, serial_hi: u8, object: Uuid, interface: Uuid, activity: Uuid, server_boot: u32, if_version: u32, seq_num: u32, opnum: u16, ihint: u16, ahint: u16, frag_len: u16, frag_num: u16, auth_proto: u8, serial_lo: u8 }`, `RpcHeader::LEN = 80`, `RpcHeader::parse(&[u8]) -> Result<RpcHeader, RpcError>`, `write(&self, &mut Vec<u8>)` (uses `self.drep`), `PacketType { Request=0, Ping=1, Response=2, Fault=3, Working=4, Nocall=5, Reject=6, Ack=7, ClCancel=8, Fack=9, CancelAck=10 }` with `from_u8`/`to_u8`; flag constants `FLAG1_LAST_FRAG = 0x02`, `FLAG1_FRAG = 0x04`, `FLAG1_NO_FACK = 0x08`, `FLAG1_IDEMPOTENT = 0x20`; `Opnum { Connect=0, Release=1, Read=2, Write=3, Control=4, ReadImplicit=5 }`.
- Produces `rpc::RpcError { TooShort{need,have}, BadVersion(u8), UnsupportedPtype(u8), Fragmented{frag_num,flags1}, BadInterface(Uuid), NdrMismatch(&'static str), Io(std::io::Error) }`.

Byte layout (all multi-byte integers in DREP; UUID `time_low/mid/hi` in DREP, the rest as bytes):

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | version = 4 |
| 1 | 1 | ptype |
| 2 | 1 | flags1 |
| 3 | 1 | flags2 |
| 4 | 3 | drep (byte 0 bit 4 = little-endian) |
| 7 | 1 | serial_hi |
| 8 | 16 | object uuid |
| 24 | 16 | interface uuid |
| 40 | 16 | activity uuid |
| 56 | 4 | server_boot |
| 60 | 4 | if_version |
| 64 | 4 | seq_num |
| 68 | 2 | opnum |
| 70 | 2 | ihint |
| 72 | 2 | ahint |
| 74 | 2 | frag_len |
| 76 | 2 | frag_num |
| 78 | 1 | auth_proto |
| 79 | 1 | serial_lo |

- [ ] **Step 1: Write the failing tests** (`rpc/uuid.rs` and `rpc/header.rs` test modules)

```rust
// uuid.rs
#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::Drep;

    #[test]
    fn parse_and_display_roundtrip() {
        let u = Uuid::parse_str("dea00001-6c97-11d1-8271-00a02442df7d").unwrap();
        assert_eq!(u.to_string(), "dea00001-6c97-11d1-8271-00a02442df7d");
        assert_eq!(u, PNIO_DEVICE_INTERFACE);
    }

    #[test]
    fn little_endian_wire_form_swaps_first_three_fields() {
        // object UUID as it appears in the CPU Connect request (DREP LE), connect_req.hex[50..66]
        let le = [0x00, 0x00, 0xa0, 0xde, 0x97, 0x6c, 0xd1, 0x11, 0x82, 0x71, 0x00, 0x01, 0x00, 0x02, 0x04, 0x93];
        let u = Uuid::read(&le, Drep::LITTLE).unwrap();
        assert_eq!(u.to_string(), "dea00000-6c97-11d1-8271-000100020493");
        assert_eq!(u, Uuid::pnio_object(0x0001, 0x0002, 0x0493));
        let mut out = Vec::new();
        u.write(&mut out, Drep::LITTLE);
        assert_eq!(out, le);
        let mut be = Vec::new();
        u.write(&mut be, Drep::BIG);
        assert_eq!(&be[..4], &[0xde, 0xa0, 0x00, 0x00]);
    }

    #[test]
    fn rejects_bad_text() {
        assert!(Uuid::parse_str("not-a-uuid").is_none());
    }
}
```

```rust
// header.rs
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{golden, RPC_OFF};

    #[test]
    fn parse_connect_request_header_little_endian() {
        let f = golden("connect_req");
        let h = RpcHeader::parse(&f[RPC_OFF..]).unwrap();
        assert_eq!(h.ptype, PacketType::Request);
        assert_eq!(h.flags1, FLAG1_IDEMPOTENT);
        assert!(h.drep.little_endian);
        assert_eq!(h.object.to_string(), "dea00000-6c97-11d1-8271-000100020493");
        assert_eq!(h.interface, PNIO_DEVICE_INTERFACE);
        assert_eq!(h.activity.to_string(), "d2aca1ae-0032-1010-b058-ec1c5d61e73f");
        assert_eq!(h.if_version, 1);
        assert_eq!(h.seq_num, 0);
        assert_eq!(h.opnum, Opnum::Connect.to_u16());
        assert_eq!((h.ihint, h.ahint), (0xffff, 0xffff));
        assert_eq!(h.frag_len, 577);
        assert_eq!(h.frag_num, 0);
    }

    #[test]
    fn parse_appready_request_header_big_endian() {
        let f = golden("appready_req");
        let h = RpcHeader::parse(&f[RPC_OFF..]).unwrap();
        assert!(!h.drep.little_endian);
        assert_eq!(h.interface, PNIO_CONTROLLER_INTERFACE);
        assert_eq!(h.object.to_string(), "dea00000-6c97-11d1-8271-1064010e002a");
        assert_eq!(h.opnum, 4);
        assert_eq!(h.frag_len, 52);
    }

    #[test]
    fn header_roundtrip_byte_exact_both_dreps() {
        for name in ["connect_req", "connect_res", "appready_req", "appready_res"] {
            let f = golden(name);
            let h = RpcHeader::parse(&f[RPC_OFF..]).unwrap();
            let mut out = Vec::new();
            h.write(&mut out);
            assert_eq!(out, &f[RPC_OFF..RPC_OFF + RpcHeader::LEN], "{name}");
        }
    }

    #[test]
    fn rejects_short_bad_version_and_fragments() {
        assert!(matches!(RpcHeader::parse(&[4u8; 10]), Err(RpcError::TooShort { need: 80, have: 10 })));
        let mut f = golden("connect_req")[RPC_OFF..].to_vec();
        f[0] = 5;
        assert!(matches!(RpcHeader::parse(&f), Err(RpcError::BadVersion(5))));
        let mut f = golden("connect_req")[RPC_OFF..].to_vec();
        f[2] |= FLAG1_FRAG;
        assert!(matches!(RpcHeader::parse(&f), Err(RpcError::Fragmented { .. })));
        let mut f = golden("connect_req")[RPC_OFF..].to_vec();
        f[76] = 1; // frag_num (LE low byte)
        assert!(matches!(RpcHeader::parse(&f), Err(RpcError::Fragmented { .. })));
    }

    #[test]
    fn packet_type_roundtrip_and_unknown() {
        assert_eq!(PacketType::from_u8(2), Ok(PacketType::Response));
        assert_eq!(PacketType::from_u8(42), Err(RpcError::UnsupportedPtype(42)));
        assert_eq!(PacketType::Fault.to_u8(), 3);
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt rpc::`
Expected: compile error (module `rpc` missing).

- [ ] **Step 3: Implement**

`rpc/mod.rs`:
```rust
//! DCE-RPC v4 connectionless (CL) codec used by PROFINET IO (UDP port 34964).

pub mod header;
pub mod uuid;

pub use header::{
    Opnum, PacketType, RpcHeader, FLAG1_FRAG, FLAG1_IDEMPOTENT, FLAG1_LAST_FRAG, FLAG1_NO_FACK,
};
pub use uuid::{Uuid, PNIO_CONTROLLER_INTERFACE, PNIO_DEVICE_INTERFACE};

use thiserror::Error;

/// UDP port of the PNIO context manager (device side listens here; controllers too).
pub const PNIO_UDP_PORT: u16 = 34964;

#[derive(Debug, Error)]
pub enum RpcError {
    #[error("rpc buffer too short: need {need}, have {have}")]
    TooShort { need: usize, have: usize },
    #[error("unsupported DCE-RPC version {0} (expected 4)")]
    BadVersion(u8),
    #[error("unsupported DCE-RPC packet type {0}")]
    UnsupportedPtype(u8),
    #[error("fragmented DCE-RPC PDU not supported (frag_num {frag_num}, flags1 {flags1:#04x})")]
    Fragmented { frag_num: u16, flags1: u8 },
    #[error("unexpected interface UUID {0}")]
    BadInterface(Uuid),
    #[error("NDR mismatch: {0}")]
    NdrMismatch(&'static str),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// NDR data representation: only the byte order matters for PNIO (char = ASCII, float = IEEE).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Drep {
    pub little_endian: bool,
}

impl Drep {
    pub const BIG: Drep = Drep { little_endian: false };
    pub const LITTLE: Drep = Drep { little_endian: true };

    pub fn from_byte(b: u8) -> Drep {
        Drep { little_endian: b & 0x10 != 0 }
    }
    pub fn to_bytes(self) -> [u8; 3] {
        [if self.little_endian { 0x10 } else { 0x00 }, 0, 0]
    }
    pub fn u16(self, b: &[u8]) -> u16 {
        let a = [b[0], b[1]];
        if self.little_endian { u16::from_le_bytes(a) } else { u16::from_be_bytes(a) }
    }
    pub fn u32(self, b: &[u8]) -> u32 {
        let a = [b[0], b[1], b[2], b[3]];
        if self.little_endian { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) }
    }
    pub fn put_u16(self, out: &mut Vec<u8>, v: u16) {
        out.extend_from_slice(&if self.little_endian { v.to_le_bytes() } else { v.to_be_bytes() });
    }
    pub fn put_u32(self, out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&if self.little_endian { v.to_le_bytes() } else { v.to_be_bytes() });
    }
}
```

`rpc/uuid.rs`:
```rust
//! 128-bit UUID with DREP-aware wire encoding (first three fields byte-order dependent).

use super::Drep;
use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Uuid(pub [u8; 16]);

/// PNIO device interface (the controller calls us on it).
pub const PNIO_DEVICE_INTERFACE: Uuid = Uuid([
    0xde, 0xa0, 0x00, 0x01, 0x6c, 0x97, 0x11, 0xd1, 0x82, 0x71, 0x00, 0xa0, 0x24, 0x42, 0xdf, 0x7d,
]);
/// PNIO controller interface (we call the controller on it for ApplicationReady).
pub const PNIO_CONTROLLER_INTERFACE: Uuid = Uuid([
    0xde, 0xa0, 0x00, 0x02, 0x6c, 0x97, 0x11, 0xd1, 0x82, 0x71, 0x00, 0xa0, 0x24, 0x42, 0xdf, 0x7d,
]);

impl Uuid {
    pub const NIL: Uuid = Uuid([0; 16]);

    /// PNIO object UUID: `dea00000-6c97-11d1-8271-{instance}{device_id}{vendor_id}`.
    pub fn pnio_object(instance: u16, device_id: u16, vendor_id: u16) -> Uuid {
        let mut b = [0xde, 0xa0, 0x00, 0x00, 0x6c, 0x97, 0x11, 0xd1, 0x82, 0x71, 0, 0, 0, 0, 0, 0];
        b[10..12].copy_from_slice(&instance.to_be_bytes());
        b[12..14].copy_from_slice(&device_id.to_be_bytes());
        b[14..16].copy_from_slice(&vendor_id.to_be_bytes());
        Uuid(b)
    }

    pub fn parse_str(s: &str) -> Option<Uuid> {
        let hex: String = s.split('-').collect();
        if hex.len() != 32 || s.split('-').map(str::len).ne([8, 4, 4, 4, 12]) {
            return None;
        }
        let mut b = [0u8; 16];
        for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
            b[i] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
        }
        Some(Uuid(b))
    }

    /// Read 16 bytes in wire form: `time_low`, `time_mid`, `time_hi` follow `drep`.
    pub fn read(buf: &[u8], drep: Drep) -> Option<Uuid> {
        if buf.len() < 16 {
            return None;
        }
        let mut b = [0u8; 16];
        b[0..4].copy_from_slice(&drep.u32(&buf[0..4]).to_be_bytes());
        b[4..6].copy_from_slice(&drep.u16(&buf[4..6]).to_be_bytes());
        b[6..8].copy_from_slice(&drep.u16(&buf[6..8]).to_be_bytes());
        b[8..16].copy_from_slice(&buf[8..16]);
        Some(Uuid(b))
    }

    pub fn write(&self, out: &mut Vec<u8>, drep: Drep) {
        drep.put_u32(out, u32::from_be_bytes([self.0[0], self.0[1], self.0[2], self.0[3]]));
        drep.put_u16(out, u16::from_be_bytes([self.0[4], self.0[5]]));
        drep.put_u16(out, u16::from_be_bytes([self.0[6], self.0[7]]));
        out.extend_from_slice(&self.0[8..16]);
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = &self.0;
        write!(
            f,
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
        )
    }
}

impl fmt::Debug for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Uuid({self})")
    }
}
```

`rpc/header.rs`: the struct/enums from **Interfaces**, `parse` (check `len >= 80` → `TooShort{need: 80, have}`, `buf[0] == 4` → `BadVersion`, `PacketType::from_u8(buf[1])`, `drep = Drep::from_byte(buf[4])`, read the fields at the offsets in the table with `drep.u16/u32` and `Uuid::read(.., drep).unwrap()` (length already checked), then `if frag_num != 0 || flags1 & FLAG1_FRAG != 0 { return Err(Fragmented{..}) }`), and `write` (push in the same order with `self.drep`). `Opnum::from_u16(u16) -> Option<Opnum>` and `to_u16`.

- [ ] **Step 4: Run the tests, clippy, fmt**

Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt rpc:: && cargo clippy --all-targets -- -D warnings && cargo fmt --all --check`
Expected: 8 tests pass, no warnings.

- [ ] **Step 5: Commit + push**

```bash
git add crates/profinet-rt/src/rpc crates/profinet-rt/src/lib.rs
git commit -m "feat(rpc): DCE-RPC v4 CL header + DREP-aware UUID codec (golden byte-exact)"
git push
```

---

### Task 3: `rpc::ndr` — request / response NDR bodies

**Files:**
- Create: `crates/profinet-rt/src/rpc/ndr.rs`
- Modify: `crates/profinet-rt/src/rpc/mod.rs` (`pub mod ndr; pub use ndr::{NdrRequest, NdrResponse};`)

**Interfaces:**
- Produces `NdrRequest { args_max: u32, args_len: u32, max_count: u32, offset: u32, actual_count: u32 }`, `NdrRequest::LEN = 20`, `NdrRequest::parse(buf, drep) -> Result<(NdrRequest, &[u8] /*blocks*/), RpcError>` — checks `args_len == actual_count`, `offset == 0`, and `actual_count <= buf.len() - 20` else `NdrMismatch("...")`; `NdrRequest::for_blocks(args_max: u32, blocks_len: u32) -> NdrRequest` (all counts = `blocks_len`, offset 0); `write(&self, out, drep)`.
- Produces `NdrResponse { status: u32, args_len: u32, max_count: u32, offset: u32, actual_count: u32 }`, `LEN = 20`, `parse(buf, drep) -> Result<(NdrResponse, &[u8]), RpcError>`, `NdrResponse::ok(request_args_max: u32, blocks_len: u32)` (status 0, `max_count = request_args_max` — **p-net echoes the request's `args_max`, and the goldens require it**), `NdrResponse::error(status, request_args_max)` (all lengths 0), `write(&self, out, drep)`.

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{Drep, RpcHeader};
    use crate::testutil::{golden, RPC_OFF};

    const BODY: usize = RPC_OFF + RpcHeader::LEN;

    #[test]
    fn parse_connect_request_body_le() {
        let f = golden("connect_req");
        let (n, blocks) = NdrRequest::parse(&f[BODY..], Drep::LITTLE).unwrap();
        assert_eq!((n.args_max, n.args_len, n.max_count, n.offset, n.actual_count), (557, 557, 557, 0, 557));
        assert_eq!(blocks.len(), 557);
        assert_eq!(&blocks[..4], &[0x01, 0x01, 0x00, 0x5b]); // ARBlockReq header
    }

    #[test]
    fn parse_connect_response_body_be() {
        let f = golden("connect_res");
        let (n, blocks) = NdrResponse::parse(&f[BODY..], Drep::BIG).unwrap();
        assert_eq!(n.status, 0);
        assert_eq!((n.args_len, n.max_count, n.actual_count), (90, 557, 90));
        assert_eq!(blocks.len(), 90);
    }

    #[test]
    fn response_ok_matches_golden_bytes() {
        let f = golden("connect_res");
        let mut out = Vec::new();
        NdrResponse::ok(557, 90).write(&mut out, Drep::BIG);
        assert_eq!(out, &f[BODY..BODY + 20]);
    }

    #[test]
    fn request_for_blocks_matches_appready_golden() {
        let f = golden("appready_req");
        let mut out = Vec::new();
        NdrRequest::for_blocks(1340, 32).write(&mut out, Drep::BIG);
        assert_eq!(out, &f[BODY..BODY + 20]);
    }

    #[test]
    fn mismatch_is_rejected() {
        let mut f = golden("connect_req")[BODY..].to_vec();
        f[16] = 0xff; // actual_count low byte (LE) -> 0x22ff > buffer
        assert!(matches!(NdrRequest::parse(&f, Drep::LITTLE), Err(RpcError::NdrMismatch(_))));
        assert!(matches!(NdrRequest::parse(&f[..10], Drep::LITTLE), Err(RpcError::TooShort { .. })));
    }
}
```

- [ ] **Step 2: Run, expect compile failure.** `. "$HOME/.cargo/env" && cargo test -p profinet-rt rpc::ndr`

- [ ] **Step 3: Implement** `ndr.rs` per the interface (parse reads 5 × `drep.u32`, validates, returns `&buf[20..20 + actual_count]`; `write` pushes with `drep.put_u32`).

- [ ] **Step 4: Run tests + clippy + fmt** — expected 5 pass.

- [ ] **Step 5: Commit + push** — `git commit -m "feat(rpc): NDR request/response array headers (DREP-aware, golden byte-exact)"`

---
### Task 4: `cm::status` + `cm::block` — PnioStatus, BlockHeader, Connect request block parsing

**Files:**
- Create: `crates/profinet-rt/src/cm/mod.rs`, `crates/profinet-rt/src/cm/status.rs`, `crates/profinet-rt/src/cm/block.rs`
- Modify: `crates/profinet-rt/src/lib.rs` (`pub mod cm;`)

**Interfaces:**
- Produces `cm::CmError { Block(BlockError), Reject(PnioStatus), WrongState { event: &'static str, state: &'static str }, UnknownAr(Uuid) }` and `cm::BlockError { TooShort { need, have }, UnexpectedType { expected: u16, got: u16 }, BadVersion(u8, u8), BadLength { declared, available }, Malformed(&'static str) }` (both `thiserror`, `PartialEq`).
- Produces `PnioStatus(pub u32)` with `pub const OK`, `fn new(code: u8, decode: u8, code1: u8, code2: u8)`, accessors `code()/decode()/code1()/code2()`, `is_ok()`, `to_u32()`, and named constructors (comment each with its origin; values follow IEC 61158-6-10 conventions as used by open stacks — re-verify against the purchased standard, tracked in FOLLOWUPS):
  - `connect_reject(block: ConnectBlock, field: u8)` → `(0xDB, 0x81, block as u8, field)` with `ConnectBlock { ArBlock = 1, IocrBlock = 2, ExpectedSubmodule = 3, AlarmCr = 4 }`
  - `connect_ar_already_exists()` → `(0xDB, 0x81, 0x3d, 0x0e)` (CMDEV state conflict)
  - `write_index_unsupported()` → `(0xDF, 0x80, 0xB0, 0x00)` (PNIORW: access, invalid index)
  - `control_wrong_state()` → `(0xDD, 0x81, 0x3d, 0x03)`
  - `service_unsupported()` → `(0x81, 0x81, 0x05, 0x00)`
- Produces `block::BlockHeader { block_type: u16, block_length: u16, version_high: u8, version_low: u8 }`, `BlockHeader::LEN = 6`, `parse(buf) -> Result<(BlockHeader, &[u8] /*body of block_length-2 bytes after version*/), BlockError>`, `write(out, block_type, body_len)` (writes type, `body_len + 2`, `1, 0`), `read_all(buf) -> Result<Vec<(BlockHeader, &[u8])>, BlockError>`.
- Produces block type constants `pub mod ty { AR_BLOCK_REQ = 0x0101, IOCR_BLOCK_REQ = 0x0102, ALARM_CR_BLOCK_REQ = 0x0103, EXPECTED_SUBMODULE_BLOCK_REQ = 0x0104, IOD_WRITE_REQ_HEADER = 0x0008, IOD_CONTROL_REQ_PRM_END = 0x0110, IOX_BLOCK_REQ_APP_READY = 0x0112, RELEASE_BLOCK_REQ = 0x0114, AR_BLOCK_RES = 0x8101, IOCR_BLOCK_RES = 0x8102, ALARM_CR_BLOCK_RES = 0x8103, AR_SERVER_BLOCK_RES = 0x8106, IOD_WRITE_RES_HEADER = 0x8008, IOD_CONTROL_RES_PRM_END = 0x8110, IOX_BLOCK_RES_APP_READY = 0x8112, RELEASE_BLOCK_RES = 0x8114 }`.
- Produces parsed request structs (all fields `pub`, `Debug, Clone, PartialEq`):
  - `ArBlockReq { ar_type: u16, ar_uuid: Uuid, session_key: u16, initiator_mac: MacAddr, initiator_object_uuid: Uuid, ar_properties: u32, activity_timeout_factor: u16, initiator_udp_rt_port: u16, station_name: String }`
  - `IocrBlockReq { iocr_type: u16, reference: u16, lt: u16, properties: u32, data_length: u16, frame_id: u16, send_clock_factor: u16, reduction_ratio: u16, phase: u16, sequence: u16, frame_send_offset: u32, watchdog_factor: u16, data_hold_factor: u16, tag_header: u16, multicast_mac: MacAddr, apis: Vec<IocrApi> }`, `IocrApi { api: u32, io_data: Vec<IocrObject>, iocs: Vec<IocrObject> }`, `IocrObject { slot: u16, subslot: u16, frame_offset: u16 }`
  - `ExpectedSubmoduleBlockReq { apis: Vec<ExpectedApi> }`, `ExpectedApi { api: u32, slot: u16, module_ident: u32, module_properties: u16, submodules: Vec<ExpectedSubmodule> }`, `ExpectedSubmodule { subslot: u16, submodule_ident: u32, properties: u16, input: Option<DataDescription>, output: Option<DataDescription> }`, `DataDescription { data_length: u16, length_iocs: u8, length_iops: u8 }`
  - `AlarmCrBlockReq { alarm_cr_type: u16, lt: u16, properties: u32, rta_timeout_factor: u16, rta_retries: u16, local_alarm_reference: u16, max_alarm_data_length: u16, tag_header_high: u16, tag_header_low: u16 }`
  - each with `fn parse(body: &[u8]) -> Result<Self, BlockError>` (body = bytes after the 6-byte header), plus a `Cursor` helper in `block.rs`: `struct Cursor<'a> { buf: &'a [u8], pos: usize }` with `u8()/u16()/u32()/bytes(n)/uuid()/mac()` returning `Result<_, BlockError>` (`TooShort` on overrun) and `remaining()`.

Body layouts (big-endian, after the 6-byte header):

| Block | Fields in order |
|---|---|
| ARBlockReq | ar_type u16, ar_uuid 16, session_key u16, initiator_mac 6, initiator_object_uuid 16 (BE), ar_properties u32, activity_timeout_factor u16, initiator_udp_rt_port u16, station_name_length u16, name bytes |
| IOCRBlockReq | iocr_type, reference, lt (u16 each), properties u32, data_length, frame_id, send_clock_factor, reduction_ratio, phase, sequence (u16 each), frame_send_offset u32, watchdog_factor, data_hold_factor, tag_header (u16 each), multicast_mac 6, number_of_apis u16, then per API: api u32, number_of_io_data u16, [slot u16, subslot u16, frame_offset u16]…, number_of_iocs u16, [slot, subslot, frame_offset]… |
| ExpectedSubmoduleBlockReq | number_of_apis u16; per API: api u32, slot u16, module_ident u32, module_properties u16, number_of_submodules u16; per submodule: subslot u16, submodule_ident u32, properties u16, then descriptors according to `properties & 0x3` (0 none, 1 input, 2 output, 3 input **then** output): data_description u16 (1 input / 2 output), data_length u16, length_iocs u8, length_iops u8 |
| AlarmCRBlockReq | alarm_cr_type, lt (u16), properties u32, rta_timeout_factor, rta_retries, local_alarm_reference, max_alarm_data_length, tag_header_high, tag_header_low (u16 each) |

- [ ] **Step 1: Failing tests** (`block.rs` test module; the Connect blocks start at `RPC_OFF + 80 + 20 = 142` in `connect_req`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::golden;

    const BLOCKS: usize = 142;

    fn connect_blocks() -> Vec<u8> {
        golden("connect_req")[BLOCKS..].to_vec()
    }

    #[test]
    fn read_all_connect_blocks_in_order() {
        let b = connect_blocks();
        let all = BlockHeader::read_all(&b).unwrap();
        let types: Vec<u16> = all.iter().map(|(h, _)| h.block_type).collect();
        assert_eq!(
            types,
            vec![0x0101, 0x0102, 0x0102, 0x0104, 0x0104, 0x0104, 0x0104, 0x0104, 0x0103]
        );
        assert_eq!(all[0].0.block_length, 91);
        assert_eq!(all[0].1.len(), 89);
    }

    #[test]
    fn parse_ar_block_req() {
        let b = connect_blocks();
        let (h, body) = BlockHeader::parse(&b).unwrap();
        assert_eq!(h.block_type, ty::AR_BLOCK_REQ);
        let ar = ArBlockReq::parse(body).unwrap();
        assert_eq!(ar.ar_type, 1);
        assert_eq!(ar.ar_uuid.to_string(), "e5e1aecc-b133-4b4d-b187-cc68b0211ed2");
        assert_eq!(ar.session_key, 2);
        assert_eq!(ar.initiator_mac.0, [0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f]);
        assert_eq!(ar.initiator_object_uuid.to_string(), "dea00000-6c97-11d1-8271-1064010e002a");
        assert_eq!(ar.ar_properties, 0x4000_0011);
        assert_eq!(ar.activity_timeout_factor, 200);
        assert_eq!(ar.initiator_udp_rt_port, 0x8892);
        assert_eq!(ar.station_name, "plcxbbench.profinetxainterfacexb25fbd");
    }

    #[test]
    fn parse_iocr_blocks() {
        let b = connect_blocks();
        let all = BlockHeader::read_all(&b).unwrap();
        let input = IocrBlockReq::parse(all[1].1).unwrap();
        assert_eq!((input.iocr_type, input.reference, input.frame_id), (1, 1, 0x8000));
        assert_eq!((input.data_length, input.send_clock_factor, input.reduction_ratio), (40, 32, 32));
        assert_eq!((input.watchdog_factor, input.data_hold_factor, input.tag_header), (3, 3, 0xc000));
        assert_eq!(input.frame_send_offset, 0xffff_ffff);
        assert_eq!(input.apis.len(), 1);
        assert_eq!(input.apis[0].io_data.len(), 6);
        assert_eq!(input.apis[0].iocs.len(), 3);
        assert_eq!(input.apis[0].io_data[5], IocrObject { slot: 4, subslot: 1, frame_offset: 9 });
        assert_eq!(input.apis[0].iocs[2], IocrObject { slot: 4, subslot: 1, frame_offset: 18 });
        let output = IocrBlockReq::parse(all[2].1).unwrap();
        assert_eq!((output.iocr_type, output.reference, output.frame_id), (2, 2, 0x8001));
        assert_eq!((output.apis[0].io_data.len(), output.apis[0].iocs.len()), (3, 6));
    }

    #[test]
    fn parse_expected_submodules() {
        let b = connect_blocks();
        let all = BlockHeader::read_all(&b).unwrap();
        let dap = ExpectedSubmoduleBlockReq::parse(all[3].1).unwrap();
        assert_eq!(dap.apis[0].slot, 0);
        assert_eq!(dap.apis[0].module_ident, 0x1);
        assert_eq!(dap.apis[0].submodules.len(), 3);
        assert_eq!(dap.apis[0].submodules[1].subslot, 0x8000);
        assert_eq!(dap.apis[0].submodules[1].submodule_ident, 0x8000);
        assert_eq!(dap.apis[0].submodules[0].input, Some(DataDescription { data_length: 0, length_iocs: 1, length_iops: 1 }));
        assert_eq!(dap.apis[0].submodules[0].output, None);
        let echo = ExpectedSubmoduleBlockReq::parse(all[7].1).unwrap();
        let sm = &echo.apis[0].submodules[0];
        assert_eq!((echo.apis[0].slot, echo.apis[0].module_ident, sm.submodule_ident, sm.properties), (4, 0x40, 0x140, 3));
        assert_eq!(sm.input.unwrap().data_length, 8);
        assert_eq!(sm.output.unwrap().data_length, 8);
        let out_only = ExpectedSubmoduleBlockReq::parse(all[5].1).unwrap();
        let sm = &out_only.apis[0].submodules[0];
        assert_eq!((sm.input, sm.output.unwrap().data_length), (None, 1));
    }

    #[test]
    fn parse_alarm_cr() {
        let b = connect_blocks();
        let all = BlockHeader::read_all(&b).unwrap();
        let a = AlarmCrBlockReq::parse(all[8].1).unwrap();
        assert_eq!((a.alarm_cr_type, a.lt, a.properties), (1, 0x8892, 0));
        assert_eq!((a.rta_timeout_factor, a.rta_retries, a.local_alarm_reference), (1, 3, 0));
        assert_eq!((a.max_alarm_data_length, a.tag_header_high, a.tag_header_low), (256, 0xc000, 0xa000));
    }

    #[test]
    fn header_errors() {
        assert!(matches!(BlockHeader::parse(&[1, 1, 0, 5]), Err(BlockError::TooShort { .. })));
        assert!(matches!(BlockHeader::parse(&[1, 1, 0, 5, 2, 0, 0, 0, 0]), Err(BlockError::BadVersion(2, 0))));
        assert!(matches!(BlockHeader::parse(&[1, 1, 0, 9, 1, 0, 0, 0]), Err(BlockError::BadLength { declared: 9, available: 2 })));
        let mut out = Vec::new();
        BlockHeader::write(&mut out, ty::AR_BLOCK_RES, 28);
        assert_eq!(out, vec![0x81, 0x01, 0x00, 0x1e, 0x01, 0x00]);
    }

    #[test]
    fn truncated_ar_block_is_too_short() {
        let b = connect_blocks();
        assert!(matches!(ArBlockReq::parse(&b[6..40]), Err(BlockError::TooShort { .. })));
    }
}
```

`status.rs` tests:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_and_unpack() {
        let s = PnioStatus::new(0xdb, 0x81, 0x03, 0x07);
        assert_eq!(s.to_u32(), 0xdb81_0307);
        assert_eq!((s.code(), s.decode(), s.code1(), s.code2()), (0xdb, 0x81, 0x03, 0x07));
        assert!(!s.is_ok());
        assert!(PnioStatus::OK.is_ok());
        assert_eq!(PnioStatus::connect_reject(ConnectBlock::ExpectedSubmodule, 7), s);
    }
}
```

- [ ] **Step 2: Run, expect compile failure.**

- [ ] **Step 3: Implement** `cm/mod.rs` (errors, `pub mod block; pub mod status;`, re-exports), `status.rs`, `block.rs` (the `Cursor`, `BlockHeader`, the four `parse` functions following the layout table; `ExpectedSubmodule` descriptors: `match properties & 0x3 { 0 => (None, None), 1 => (Some(d()), None), 2 => (None, Some(d())), _ => { let i = d(); let o = d(); (Some(i), Some(o)) } }` where `d()` reads `data_description u16` (must be 1 for input / 2 for output else `Malformed("data description tag")`), `data_length`, `length_iocs`, `length_iops`).

- [ ] **Step 4: Run tests + clippy + fmt** — expected 8 pass.

- [ ] **Step 5: Commit + push** — `git commit -m "feat(cm): PnioStatus + PNIO block header + Connect request block parsers (golden)"`

---

### Task 5: `cm::model` + `cm::connect` — DeviceModel, Connect validation, byte-exact Connect response

**Files:**
- Create: `crates/profinet-rt/src/cm/model.rs`, `crates/profinet-rt/src/cm/connect.rs`
- Modify: `crates/profinet-rt/src/cm/mod.rs`

**Interfaces:**
- Produces `DeviceModel { vendor_id: u16, device_id: u16, instance: u16, station_name: String, mac: MacAddr, max_alarm_data_length: u16, slots: Vec<SlotModel> }`, `SlotModel { slot: u16, module_ident: u32, submodules: Vec<SubmoduleModel> }`, `SubmoduleModel { subslot: u16, submodule_ident: u32, input_len: u16, output_len: u16 }`; `DeviceModel::find(&self, slot, subslot) -> Option<&SubmoduleModel>`; `DeviceModel::pnet_sample(mac: MacAddr) -> DeviceModel` (the HIL clone: vendor `0x0493`, device `0x0002`, instance `1`, name `rt-labs-dev`, `max_alarm_data_length: 200`, DAP slot 0 module `0x1` with subslots `(1, 0x1, 0, 0)`, `(0x8000, 0x8000, 0, 0)`, `(0x8001, 0x8001, 0, 0)`; slot 1 module `0x30` subslot `(1, 0x130, 1, 0)`; slot 2 `0x31` `(1, 0x131, 0, 1)`; slot 3 `0x32` `(1, 0x132, 1, 1)`; slot 4 `0x40` `(1, 0x140, 8, 8)`); `DeviceModel::object_uuid(&self) -> Uuid`.
- Produces `ConnectReq { ar: ArBlockReq, iocrs: Vec<IocrBlockReq>, expected: Vec<ExpectedSubmoduleBlockReq>, alarm_cr: AlarmCrBlockReq }` with `ConnectReq::parse(blocks: &[u8]) -> Result<ConnectReq, CmError>` (exactly one ARBlockReq and one AlarmCRBlockReq else `Reject(connect_reject(ArBlock/AlarmCr, 0))`; unknown block types → `Reject(connect_reject(ArBlock, 0xff))`; block decode errors → `CmError::Block`).
- Produces `validate(req: &ConnectReq, model: &DeviceModel) -> Result<ArParams, PnioStatus>` where `ArParams { ar_uuid: Uuid, session_key: u16, initiator_mac: MacAddr, initiator_object_uuid: Uuid, activity_timeout_factor: u16, input_cr: IocrParams, output_cr: IocrParams, alarm_ref_remote: u16, max_alarm_data_length: u16 }`, `IocrParams { reference: u16, frame_id: u16, data_length: u16, send_clock_factor: u16, reduction_ratio: u16, watchdog_factor: u16, data_hold_factor: u16, io_data: Vec<IocrObject>, iocs: Vec<IocrObject> }`. Rules (spec §6): `ar_type == 1` else `connect_reject(ArBlock, 1)`; `ar_uuid != NIL` else `(ArBlock, 2)`; exactly 2 IOCR, one type 1 one type 2 else `(IocrBlock, 1)`; `frame_id` in `0x8000..=0xBBFF` else `(IocrBlock, 6)`; every expected submodule found in the model with equal `input_len`/`output_len` (an absent descriptor counts as length 0) else `(ExpectedSubmodule, 7)`; `alarm_cr_type == 1` else `(AlarmCr, 1)`.
- Produces `build_connect_res(params: &ArParams, model: &DeviceModel) -> Vec<u8>` — the PNIO blocks only (no RPC header/NDR): `ARBlockRes` (ar_type 1, ar_uuid, session_key, `model.mac`, port `0x8892`), `IOCRBlockRes` ×2 in request order (type, reference, frame_id), `AlarmCRBlockRes` (type 1, local_alarm_reference **0**, `model.max_alarm_data_length`), `ARServerBlockRes` (name length, name, zero padding to a 4-byte multiple of the block).

- [ ] **Step 1: Failing tests** (`connect.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cm::model::DeviceModel;
    use crate::eth::MacAddr;
    use crate::testutil::golden;

    const REQ_BLOCKS: usize = 142;
    const RES_BLOCKS: usize = 142;
    const DEVICE_MAC: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);

    fn req() -> ConnectReq {
        ConnectReq::parse(&golden("connect_req")[REQ_BLOCKS..]).unwrap()
    }

    #[test]
    fn parse_groups_blocks() {
        let r = req();
        assert_eq!(r.iocrs.len(), 2);
        assert_eq!(r.expected.len(), 5);
        assert_eq!(r.alarm_cr.max_alarm_data_length, 256);
    }

    #[test]
    fn validate_against_pnet_model() {
        let p = validate(&req(), &DeviceModel::pnet_sample(DEVICE_MAC)).unwrap();
        assert_eq!(p.session_key, 2);
        assert_eq!((p.input_cr.frame_id, p.output_cr.frame_id), (0x8000, 0x8001));
        assert_eq!(p.input_cr.reduction_ratio, 32);
        assert_eq!(p.activity_timeout_factor, 200);
        assert_eq!(p.max_alarm_data_length, 200);
    }

    #[test]
    fn connect_response_is_byte_exact() {
        let model = DeviceModel::pnet_sample(DEVICE_MAC);
        let p = validate(&req(), &model).unwrap();
        let out = build_connect_res(&p, &model);
        assert_eq!(out, &golden("connect_res")[RES_BLOCKS..]);
        assert_eq!(out.len(), 90);
    }

    #[test]
    fn mismatching_module_is_rejected_with_explicit_status() {
        let mut model = DeviceModel::pnet_sample(DEVICE_MAC);
        model.slots[4].submodules[0].input_len = 4; // Echo expects 8
        let err = validate(&req(), &model).unwrap_err();
        assert_eq!(err, PnioStatus::connect_reject(ConnectBlock::ExpectedSubmodule, 7));
        let mut model = DeviceModel::pnet_sample(DEVICE_MAC);
        model.slots.pop(); // slot 4 missing
        assert_eq!(validate(&req(), &model).unwrap_err(), PnioStatus::connect_reject(ConnectBlock::ExpectedSubmodule, 7));
    }

    #[test]
    fn bad_frame_id_and_ar_type_are_rejected() {
        let mut r = req();
        r.iocrs[0].frame_id = 0xc000;
        assert_eq!(validate(&r, &DeviceModel::pnet_sample(DEVICE_MAC)).unwrap_err(), PnioStatus::connect_reject(ConnectBlock::IocrBlock, 6));
        let mut r = req();
        r.ar.ar_type = 6;
        assert_eq!(validate(&r, &DeviceModel::pnet_sample(DEVICE_MAC)).unwrap_err(), PnioStatus::connect_reject(ConnectBlock::ArBlock, 1));
    }

    #[test]
    fn missing_alarm_cr_block_is_a_reject() {
        let b = golden("connect_req");
        let without_alarm = &b[REQ_BLOCKS..b.len() - 26]; // AlarmCRBlockReq = 6 + 20 bytes
        assert!(matches!(ConnectReq::parse(without_alarm), Err(CmError::Reject(_))));
    }
}
```

`model.rs` test:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::eth::MacAddr;

    #[test]
    fn pnet_sample_layout() {
        let m = DeviceModel::pnet_sample(MacAddr([0; 6]));
        assert_eq!(m.slots.len(), 5);
        assert_eq!(m.find(0, 0x8001).unwrap().submodule_ident, 0x8001);
        assert_eq!(m.find(4, 1).unwrap().output_len, 8);
        assert!(m.find(9, 1).is_none());
        assert_eq!(m.object_uuid().to_string(), "dea00000-6c97-11d1-8271-000100020493");
    }
}
```

- [ ] **Step 2: Run, expect compile failure.**

- [ ] **Step 3: Implement.** Response block bodies (BE): ARBlockRes body = `ar_type u16, ar_uuid 16, session_key u16, mac 6, 0x8892 u16` (28 bytes → header length 30); IOCRBlockRes body = `iocr_type, reference, frame_id` (6 → length 8); AlarmCRBlockRes body = `1, 0, max_alarm_data_length` (6 → length 8); ARServerBlockRes body = `name_len u16 + name + pad` where pad makes `(6 + body).len() % 4 == 0` (11-char name → 1 pad byte → length 16). Write with `BlockHeader::write(out, ty, body.len())` then the body.

- [ ] **Step 4: Run tests + clippy + fmt** — expected 7 pass; `connect_response_is_byte_exact` is the gate.

- [ ] **Step 5: Commit + push** — `git commit -m "feat(cm): DeviceModel + Connect validation + byte-exact Connect response"`

---
### Task 6: `cm::write` — IODWriteReq / MultipleWrite parsing and byte-exact Write response

**Files:**
- Create: `crates/profinet-rt/src/cm/write.rs`
- Modify: `crates/profinet-rt/src/cm/mod.rs`

**Interfaces:**
- Produces `Record { seq: u16, ar_uuid: Uuid, api: u32, slot: u16, subslot: u16, index: u16, data: Vec<u8> }` (`Debug, Clone, PartialEq`).
- Produces `WriteReq { records: Vec<Record> }` with `WriteReq::parse(blocks: &[u8]) -> Result<WriteReq, CmError>`: one `IODWriteReqHeader` (type `0x0008`) + `record_data_length` data bytes; if `index == 0xe040` (**MultipleWrite**) the data holds nested `IODWriteReqHeader + data` records, each followed by zero padding up to the next 4-byte boundary — the outer container is kept as `records[0]` (with its raw data) and the nested ones follow; otherwise `records` has one element. Anything else → `CmError::Block(UnexpectedType)`.
- Produces `build_write_res(req: &WriteReq) -> Vec<u8>`: one `IODWriteResHeader` (type `0x8008`) per record **in request order**, `record_data_length = 0`, `additional_value_1/2 = 0`, `status = 0` (16-byte padding), nested ones directly concatenated after the outer one (no alignment needed: 64 bytes each).
- Produces `pub const INDEX_MULTIPLE_WRITE: u16 = 0xe040;`.

Body layouts (BE, after the 6-byte header):
- `IODWriteReqHeader` (length 60): `seq u16, ar_uuid 16, api u32, slot u16, subslot u16, pad u16, index u16, record_data_length u32, padding[24]` then `data[record_data_length]`.
- `IODWriteResHeader` (length 60): `seq u16, ar_uuid 16, api u32, slot u16, subslot u16, pad u16, index u16, record_data_length u32, additional_value_1 u16, additional_value_2 u16, status u32, padding[16]`.

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::golden;

    const BLOCKS: usize = 142;

    #[test]
    fn parse_multiple_write_records() {
        let w = WriteReq::parse(&golden("write_req")[BLOCKS..]).unwrap();
        let idx: Vec<u16> = w.records.iter().map(|r| r.index).collect();
        assert_eq!(idx, vec![0xe040, 0x8071, 0x7b, 0x7c, 0x7d]);
        assert_eq!(w.records[0].data.len(), 280);
        assert_eq!((w.records[0].api, w.records[0].slot, w.records[0].subslot), (0xffff_ffff, 0xffff, 0xffff));
        assert_eq!((w.records[1].slot, w.records[1].subslot), (0, 0x8000));
        assert_eq!(w.records[1].data, vec![0x02, 0x50, 0x00, 0x08, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01]);
        assert_eq!((w.records[2].slot, w.records[2].subslot, w.records[2].seq), (3, 1, 2));
        assert_eq!(w.records[4].data, vec![0, 0, 0, 2]);
        assert_eq!(w.records[3].ar_uuid.to_string(), "e5e1aecc-b133-4b4d-b187-cc68b0211ed2");
    }

    #[test]
    fn write_response_is_byte_exact() {
        let w = WriteReq::parse(&golden("write_req")[BLOCKS..]).unwrap();
        let out = build_write_res(&w);
        assert_eq!(out, &golden("write_res")[BLOCKS..]);
        assert_eq!(out.len(), 320);
    }

    #[test]
    fn single_record_write() {
        // hand-built: one record, index 0x7b, 4 data bytes
        let mut b = Vec::new();
        b.extend_from_slice(&[0x00, 0x08, 0x00, 0x3c, 0x01, 0x00, 0x00, 0x05]);
        b.extend_from_slice(&[0x11; 16]); // ar_uuid
        b.extend_from_slice(&[0, 0, 0, 0, 0, 3, 0, 1, 0, 0, 0, 0x7b, 0, 0, 0, 4]);
        b.extend_from_slice(&[0; 24]);
        b.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        let w = WriteReq::parse(&b).unwrap();
        assert_eq!(w.records.len(), 1);
        assert_eq!(w.records[0].data, vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(build_write_res(&w).len(), 64);
    }

    #[test]
    fn truncated_data_is_an_error() {
        let b = &golden("write_req")[BLOCKS..BLOCKS + 100];
        assert!(matches!(WriteReq::parse(b), Err(CmError::Block(_))));
    }

    #[test]
    fn wrong_block_type_is_rejected() {
        let b = &golden("prmend_req")[BLOCKS..];
        assert!(matches!(WriteReq::parse(b), Err(CmError::Block(BlockError::UnexpectedType { .. }))));
    }
}
```

- [ ] **Step 2: Run, expect compile failure.**

- [ ] **Step 3: Implement.** `parse_one(buf) -> Result<(Record, usize /*consumed incl. data*/), BlockError>`; `WriteReq::parse` calls it once, then if `index == INDEX_MULTIPLE_WRITE` loops over `record.data` with `pos = (pos + consumed + 3) & !3` until `pos >= data.len()`. `build_write_res` writes for each record: `BlockHeader::write(out, ty::IOD_WRITE_RES_HEADER, 58)`, then the body fields, `record_data_length = 0`, zeros.

- [ ] **Step 4: Run tests + clippy + fmt** — expected 5 pass.

- [ ] **Step 5: Commit + push** — `git commit -m "feat(cm): IODWrite / MultipleWrite records + byte-exact Write response"`

---

### Task 7: `cm::control` — PrmEnd request/response, ApplicationReady request/response

**Files:**
- Create: `crates/profinet-rt/src/cm/control.rs`
- Modify: `crates/profinet-rt/src/cm/mod.rs`

**Interfaces:**
- Produces `ControlBlock { block_type: u16, ar_uuid: Uuid, session_key: u16, command: u16, properties: u16 }` with `ControlBlock::parse(blocks: &[u8]) -> Result<ControlBlock, CmError>` (accepts types `0x0110`, `0x8110`, `0x0112`, `0x8112`, `0x0114`, `0x8114`; else `Block(UnexpectedType)`), `write(&self, out: &mut Vec<u8>)`.
- Produces command constants `pub mod cmd { PRM_END = 0x0001, APPLICATION_READY = 0x0002, RELEASE = 0x0004, DONE = 0x0008 }`.
- Produces `prm_end_done(req: &ControlBlock) -> ControlBlock` (type `0x8110`, same ar_uuid/session, command `DONE`, properties 0), `app_ready_req(ar_uuid: Uuid, session_key: u16) -> ControlBlock` (type `0x0112`, command `APPLICATION_READY`, properties 0), `release_done(req) -> ControlBlock` (type `0x8114`, command `DONE`).
- Body layout (BE, length 28 → header `00 1c`): `reserved u16 = 0, ar_uuid 16, session_key u16, reserved u16 = 0, command u16, properties u16`.

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::golden;

    const BLOCKS: usize = 142;

    #[test]
    fn parse_prm_end_and_answer_byte_exact() {
        let req = ControlBlock::parse(&golden("prmend_req")[BLOCKS..]).unwrap();
        assert_eq!(req.block_type, ty::IOD_CONTROL_REQ_PRM_END);
        assert_eq!((req.session_key, req.command, req.properties), (2, cmd::PRM_END, 0));
        let mut out = Vec::new();
        prm_end_done(&req).write(&mut out);
        assert_eq!(out, &golden("prmend_res")[BLOCKS..]);
        assert_eq!(out.len(), 32);
    }

    #[test]
    fn app_ready_request_byte_exact() {
        let req = ControlBlock::parse(&golden("prmend_req")[BLOCKS..]).unwrap();
        let mut out = Vec::new();
        app_ready_req(req.ar_uuid, req.session_key).write(&mut out);
        assert_eq!(out, &golden("appready_req")[BLOCKS..]);
    }

    #[test]
    fn parse_app_ready_response_from_cpu() {
        let res = ControlBlock::parse(&golden("appready_res")[BLOCKS..]).unwrap();
        assert_eq!(res.block_type, ty::IOX_BLOCK_RES_APP_READY);
        assert_eq!(res.command, cmd::DONE);
        assert_eq!(res.ar_uuid.to_string(), "e5e1aecc-b133-4b4d-b187-cc68b0211ed2");
    }

    #[test]
    fn rejects_non_control_block() {
        assert!(matches!(ControlBlock::parse(&golden("write_req")[BLOCKS..]), Err(CmError::Block(BlockError::UnexpectedType { .. }))));
    }
}
```

- [ ] **Step 2: Run, expect compile failure.**
- [ ] **Step 3: Implement** per layout.
- [ ] **Step 4: Run tests + clippy + fmt** — expected 4 pass.
- [ ] **Step 5: Commit + push** — `git commit -m "feat(cm): Control blocks (PrmEnd, ApplicationReady, Release) byte-exact"`

---
### Task 8: `cm::ar` — pure AR state machine

**Files:**
- Create: `crates/profinet-rt/src/cm/ar.rs`
- Modify: `crates/profinet-rt/src/cm/mod.rs`

**Interfaces:**
- Produces `ArState { Idle, Connected, AppReadySent, Data }` (`Copy, Debug, PartialEq`), `AbortReason { ControllerRelease, AppReadyFailed, AppReadyRejected(PnioStatus), ActivityTimeout, External(&'static str) }`.
- Produces `Event { ConnectReq(ConnectReq), WriteReq(WriteReq), PrmEndReq(ControlBlock), ReleaseReq(ControlBlock), AppReadyRsp { status: PnioStatus }, Tick, Abort(AbortReason) }`.
- Produces `Action { Respond { status: PnioStatus, blocks: Vec<u8> }, CallController { blocks: Vec<u8> }, Notify { state: ArState, reason: Option<AbortReason> } }`.
- Produces `ArContext { params: ArParams, records: Vec<Record>, connected_at: Instant }` (`pub`), and `Ar { model: DeviceModel, state: ArState, ctx: Option<ArContext>, app_ready_attempts: u8, app_ready_deadline: Option<Instant>, activity_deadline: Option<Instant> }` with:
  - `Ar::new(model: DeviceModel) -> Ar`, `state(&self) -> ArState`, `context(&self) -> Option<&ArContext>`,
  - `on(&mut self, ev: Event, now: Instant) -> Vec<Action>`,
  - `next_deadline(&self) -> Option<Instant>` (earliest of the two timers; the `device` loop sleeps until it and then feeds `Tick`). *Deviation from the spec's `SetTimer` actions: deadlines are queried instead of pushed — one less action type, same behaviour.*
- Constants: `APP_READY_TIMEOUT = 1 s`, `APP_READY_MAX_ATTEMPTS = 3`, `ACTIVITY_TIMEOUT_UNIT = 100 ms`.

Transition table (every cell is a test):

| State \ Event | ConnectReq | WriteReq | PrmEndReq | ReleaseReq | AppReadyRsp | Tick (deadline hit) |
|---|---|---|---|---|---|---|
| Idle | validate: ok → `Connected`, Respond(ok, connect_res), Notify(Connected), activity timer armed; reject → Respond(status, empty), stay Idle | Respond(`control_wrong_state`) | Respond(`control_wrong_state`) | Respond(ok, release_done) (nothing to abort) | ignored | — |
| Connected | same ar_uuid → Respond(ok, same connect_res) (idempotent); other ar_uuid → Respond(`connect_ar_already_exists`) | store records, Respond(ok, write_res), re-arm activity | Respond(ok, prm_end_done) + CallController(app_ready_req) → `AppReadySent`, attempts=1, app-ready timer 1 s | Respond(ok, release_done) + abort(ControllerRelease) | ignored | activity → abort(ActivityTimeout) |
| AppReadySent | as Connected | as Connected | Respond(ok, prm_end_done) again (idempotent), no new call | as Connected | status ok → `Data`, Notify(Data), clear timers; status ≠ ok → abort(AppReadyRejected) | app-ready → attempts < 3: CallController again, re-arm; else abort(AppReadyFailed) |
| Data | other ar_uuid → reject; same → resend | store, Respond(ok) | Respond(ok, prm_end_done) | Respond(ok, release_done) + abort | ignored | — (no timers in Data) |

`abort(reason)`: clear `ctx` and timers, `state = Idle`, emit `Notify { state: Idle, reason: Some(reason) }`. `Event::Abort(r)` in any non-Idle state → `abort(r)`; in Idle → nothing. Every transition logs `log::info!("AR {prev:?} --{event}--> {next:?}")`, aborts at `warn!` with the reason.

The Connect response bytes are cached in `ArContext` (`connect_res: Vec<u8>`) so the idempotent resend is byte-identical.

- [ ] **Step 1: Failing tests** — a helper builds the golden events:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cm::connect::ConnectReq;
    use crate::cm::control::ControlBlock;
    use crate::cm::model::DeviceModel;
    use crate::cm::write::WriteReq;
    use crate::eth::MacAddr;
    use crate::testutil::golden;
    use std::time::{Duration, Instant};

    const BLOCKS: usize = 142;
    const MAC: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);

    fn ar() -> Ar { Ar::new(DeviceModel::pnet_sample(MAC)) }
    fn connect() -> Event { Event::ConnectReq(ConnectReq::parse(&golden("connect_req")[BLOCKS..]).unwrap()) }
    fn write() -> Event { Event::WriteReq(WriteReq::parse(&golden("write_req")[BLOCKS..]).unwrap()) }
    fn prm_end() -> Event { Event::PrmEndReq(ControlBlock::parse(&golden("prmend_req")[BLOCKS..]).unwrap()) }
    fn t0() -> Instant { Instant::now() }

    fn respond_ok(actions: &[Action]) -> &Vec<u8> {
        match &actions[0] { Action::Respond { status, blocks } if status.is_ok() => blocks, other => panic!("{other:?}") }
    }

    #[test]
    fn nominal_idle_to_data() {
        let mut ar = ar();
        let now = t0();
        let a = ar.on(connect(), now);
        assert_eq!(respond_ok(&a), &golden("connect_res")[BLOCKS..]);
        assert!(matches!(a[1], Action::Notify { state: ArState::Connected, reason: None }));
        assert_eq!(ar.state(), ArState::Connected);
        let a = ar.on(write(), now);
        assert_eq!(respond_ok(&a), &golden("write_res")[BLOCKS..]);
        assert_eq!(ar.context().unwrap().records.len(), 5);
        let a = ar.on(prm_end(), now);
        assert_eq!(respond_ok(&a), &golden("prmend_res")[BLOCKS..]);
        assert!(matches!(&a[1], Action::CallController { blocks } if blocks == &golden("appready_req")[BLOCKS..]));
        assert_eq!(ar.state(), ArState::AppReadySent);
        assert_eq!(ar.next_deadline(), Some(now + APP_READY_TIMEOUT));
        let a = ar.on(Event::AppReadyRsp { status: PnioStatus::OK }, now);
        assert!(matches!(a[0], Action::Notify { state: ArState::Data, reason: None }));
        assert_eq!(ar.state(), ArState::Data);
        assert_eq!(ar.next_deadline(), None);
    }

    #[test]
    fn rejected_connect_stays_idle_with_status() {
        let mut ar = Ar::new({ let mut m = DeviceModel::pnet_sample(MAC); m.slots.pop(); m });
        let a = ar.on(connect(), t0());
        assert!(matches!(&a[0], Action::Respond { status, blocks } if !status.is_ok() && blocks.is_empty()));
        assert_eq!(ar.state(), ArState::Idle);
        assert!(ar.context().is_none());
    }

    #[test]
    fn duplicate_connect_is_idempotent_and_other_ar_is_rejected() {
        let mut ar = ar();
        let now = t0();
        let first = respond_ok(&ar.on(connect(), now)).clone();
        assert_eq!(respond_ok(&ar.on(connect(), now)), &first);
        let mut other = match connect() { Event::ConnectReq(c) => c, _ => unreachable!() };
        other.ar.ar_uuid = crate::rpc::Uuid([7; 16]);
        let a = ar.on(Event::ConnectReq(other), now);
        assert!(matches!(&a[0], Action::Respond { status, .. } if *status == PnioStatus::connect_ar_already_exists()));
        assert_eq!(ar.state(), ArState::Connected);
    }

    #[test]
    fn write_or_prm_end_in_idle_is_wrong_state() {
        let mut ar = ar();
        let a = ar.on(write(), t0());
        assert!(matches!(&a[0], Action::Respond { status, .. } if *status == PnioStatus::control_wrong_state()));
        let a = ar.on(prm_end(), t0());
        assert!(matches!(&a[0], Action::Respond { status, .. } if *status == PnioStatus::control_wrong_state()));
    }

    #[test]
    fn app_ready_retries_three_times_then_aborts() {
        let mut ar = ar();
        let now = t0();
        ar.on(connect(), now);
        let a = ar.on(prm_end(), now);
        assert!(matches!(a[1], Action::CallController { .. }));
        let t1 = now + APP_READY_TIMEOUT + Duration::from_millis(1);
        let a = ar.on(Event::Tick, t1);
        assert!(matches!(a[0], Action::CallController { .. }));
        let t2 = t1 + APP_READY_TIMEOUT + Duration::from_millis(1);
        let a = ar.on(Event::Tick, t2);
        assert!(matches!(a[0], Action::CallController { .. }));
        let t3 = t2 + APP_READY_TIMEOUT + Duration::from_millis(1);
        let a = ar.on(Event::Tick, t3);
        assert!(matches!(a[0], Action::Notify { state: ArState::Idle, reason: Some(AbortReason::AppReadyFailed) }));
        assert_eq!(ar.state(), ArState::Idle);
    }

    #[test]
    fn app_ready_bad_status_aborts() {
        let mut ar = ar();
        let now = t0();
        ar.on(connect(), now);
        ar.on(prm_end(), now);
        let bad = PnioStatus::new(0xdd, 0x81, 1, 1);
        let a = ar.on(Event::AppReadyRsp { status: bad }, now);
        assert!(matches!(a[0], Action::Notify { state: ArState::Idle, reason: Some(AbortReason::AppReadyRejected(s)) } if s == bad));
    }

    #[test]
    fn activity_timeout_before_data_aborts() {
        let mut ar = ar();
        let now = t0();
        ar.on(connect(), now); // factor 200 -> 20 s
        assert_eq!(ar.next_deadline(), Some(now + Duration::from_millis(200 * 100)));
        let a = ar.on(Event::Tick, now + Duration::from_secs(21));
        assert!(matches!(a[0], Action::Notify { state: ArState::Idle, reason: Some(AbortReason::ActivityTimeout) }));
    }

    #[test]
    fn release_aborts_and_answers() {
        let mut ar = ar();
        let now = t0();
        ar.on(connect(), now);
        let mut rel = ControlBlock::parse(&golden("prmend_req")[BLOCKS..]).unwrap();
        rel.block_type = ty::RELEASE_BLOCK_REQ;
        rel.command = cmd::RELEASE;
        let a = ar.on(Event::ReleaseReq(rel), now);
        assert!(matches!(&a[0], Action::Respond { status, blocks } if status.is_ok() && blocks[0..2] == [0x81, 0x14]));
        assert!(matches!(a[1], Action::Notify { state: ArState::Idle, reason: Some(AbortReason::ControllerRelease) }));
    }

    #[test]
    fn connect_after_abort_succeeds_with_fresh_context() {
        let mut ar = ar();
        let now = t0();
        ar.on(connect(), now);
        ar.on(Event::Abort(AbortReason::External("test")), now);
        assert!(ar.context().is_none());
        let a = ar.on(connect(), now);
        assert_eq!(respond_ok(&a), &golden("connect_res")[BLOCKS..]);
    }
}
```

- [ ] **Step 2: Run, expect compile failure.**
- [ ] **Step 3: Implement** `ar.rs` following the table. Keep `on` as a `match (self.state, ev)` with small private helpers (`handle_connect`, `handle_write`, `handle_prm_end`, `handle_release`, `handle_tick`, `abort`). `handle_prm_end` in `Connected`: `self.app_ready_attempts = 1; self.app_ready_deadline = Some(now + APP_READY_TIMEOUT); self.activity_deadline = None;`.
- [ ] **Step 4: Run tests + clippy + fmt** — expected 9 pass.
- [ ] **Step 5: Commit + push** — `git commit -m "feat(cm): pure AR state machine (Idle/Connected/AppReadySent/Data) with retries and timeouts"`

---

### Task 9: `cm::Cm` — RPC datagram ↔ AR glue, response cache, byte-exact full PDUs

**Files:**
- Modify: `crates/profinet-rt/src/cm/mod.rs`

**Interfaces:**
- Produces `Outgoing { bytes: Vec<u8>, to: SocketAddr }`, `CmOutput { send: Vec<Outgoing>, notify: Vec<(ArState, Option<AbortReason>)> }` (`Default`).
- Produces `Cm` with `Cm::new(model: DeviceModel, activity_seed: Uuid) -> Cm` (`activity_seed` is the activity UUID used for our ApplicationReady call; the example derives one from the MAC, the tests inject the golden one), `handle_datagram(&mut self, buf: &[u8], from: SocketAddr, now: Instant) -> Result<CmOutput, RpcError>`, `tick(&mut self, now) -> CmOutput`, `next_deadline(&self) -> Option<Instant>`, `state(&self) -> ArState`, `context(&self) -> Option<&ArContext>`.
- Produces `pub const RPC_ARGS_MAX: u32 = 1340;` (our advertised max args for outgoing calls — p-net's value, required by the golden).

Behaviour of `handle_datagram`:
1. `RpcHeader::parse` (errors propagate: the caller logs and drops).
2. `ptype == Response`: this is the controller answering our ApplicationReady → `NdrResponse::parse(body, h.drep)` → `Ar::on(AppReadyRsp { status: PnioStatus(n.status) })`; anything else in the body is ignored. Any other non-Request ptype → log + `Ok(empty)`.
3. `ptype == Request`: `h.interface != PNIO_DEVICE_INTERFACE` → `Err(BadInterface)`. Cache hit on `(h.activity, h.seq_num)` → resend the cached bytes. Else `NdrRequest::parse(body, h.drep)` and dispatch on `Opnum::from_u16(h.opnum)`:
   - `Connect` → `ConnectReq::parse(blocks)`; `Write` → `WriteReq::parse`; `Control` → `ControlBlock::parse` then `PrmEndReq` if `command == PRM_END` (any other command → `control_wrong_state` response); `Release` → `ControlBlock::parse` → `ReleaseReq`; `Read`/`ReadImplicit`/unknown → response with `service_unsupported`, empty blocks.
   - a `CmError::Block` while parsing → response with `PnioStatus::connect_reject(ArBlock, 0xfe)` for Connect, `write_index_unsupported` for Write, `control_wrong_state` for Control (log the error). `CmError::Reject(s)` → response with `s`.
4. Every `Action::Respond` becomes one RPC Response PDU to `from`: header = `RpcHeader { ptype: Response, flags1: FLAG1_IDEMPOTENT | FLAG1_NO_FACK, flags2: 0, drep: Drep::BIG, serial_hi: 0, object: h.object, interface: h.interface, activity: h.activity, server_boot: 0, if_version: 1, seq_num: h.seq_num, opnum: h.opnum, ihint: 0xffff, ahint: 0xffff, frag_len: (20 + blocks.len()) as u16, frag_num: 0, auth_proto: 0, serial_lo: 0 }` + `NdrResponse::ok(req.args_max, blocks.len())` (or `NdrResponse::error(status, req.args_max)` when the status is not ok — then no blocks) + blocks. The PDU is stored in the cache under `(activity, seq_num)` (cache holds the last 4 entries, FIFO eviction).
5. `Action::CallController { blocks }` becomes one RPC Request PDU to `SocketAddr::new(from.ip(), PNIO_UDP_PORT)`: header `{ ptype: Request, flags1: FLAG1_IDEMPOTENT, drep: BIG, object: ctx.params.initiator_object_uuid, interface: PNIO_CONTROLLER_INTERFACE, activity: self.activity_seed, server_boot: 0, if_version: 1, seq_num: self.call_seq (starts at 0, +1 per new call, unchanged on retries), opnum: 4, hints 0xffff, frag_len: (20 + blocks.len()) as u16, .. }` + `NdrRequest::for_blocks(RPC_ARGS_MAX, blocks.len())` + blocks. Retries (from `tick`) reuse the same `seq_num`; `tick` sends to the address remembered from the last Connect (`ctx.params` gets `initiator_addr: SocketAddr` — add this field to `ArParams` in `connect.rs` as `Option<SocketAddr>` set by `Cm` after validation, or keep it in `Cm` as `controller_addr: Option<SocketAddr>`; choose the latter, simpler).
6. `Action::Notify` → `output.notify`.

- [ ] **Step 1: Failing tests** (`cm/mod.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cm::model::DeviceModel;
    use crate::eth::MacAddr;
    use crate::rpc::Uuid;
    use crate::testutil::{golden, RPC_OFF};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::time::Instant;

    const MAC: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);
    fn cpu() -> SocketAddr { SocketAddr::new(IpAddr::V4(Ipv4Addr::new(172, 16, 2, 100)), 54766) }
    fn cpu_cm() -> SocketAddr { SocketAddr::new(IpAddr::V4(Ipv4Addr::new(172, 16, 2, 100)), 34964) }
    fn cm() -> Cm {
        Cm::new(
            DeviceModel::pnet_sample(MAC),
            Uuid::parse_str("14af198a-1234-1056-8079-8cf319cd19f8").unwrap(),
        )
    }
    fn pdu(name: &str) -> Vec<u8> { golden(name)[RPC_OFF..].to_vec() }

    #[test]
    fn full_exchange_is_byte_exact_including_rpc_headers() {
        let mut cm = cm();
        let now = Instant::now();
        let o = cm.handle_datagram(&pdu("connect_req"), cpu(), now).unwrap();
        assert_eq!(o.send.len(), 1);
        assert_eq!(o.send[0].bytes, pdu("connect_res"));
        assert_eq!(o.send[0].to, cpu());
        assert_eq!(o.notify, vec![(ArState::Connected, None)]);
        let o = cm.handle_datagram(&pdu("write_req"), cpu(), now).unwrap();
        assert_eq!(o.send[0].bytes, pdu("write_res"));
        let o = cm.handle_datagram(&pdu("prmend_req"), cpu(), now).unwrap();
        assert_eq!(o.send[0].bytes, pdu("prmend_res"));
        assert_eq!(o.send[1].bytes, pdu("appready_req"));
        assert_eq!(o.send[1].to, cpu_cm());
        assert_eq!(cm.state(), ArState::AppReadySent);
        let o = cm.handle_datagram(&pdu("appready_res"), cpu_cm(), now).unwrap();
        assert!(o.send.is_empty());
        assert_eq!(o.notify, vec![(ArState::Data, None)]);
        assert_eq!(cm.state(), ArState::Data);
    }

    #[test]
    fn retransmitted_request_gets_cached_response() {
        let mut cm = cm();
        let now = Instant::now();
        let first = cm.handle_datagram(&pdu("connect_req"), cpu(), now).unwrap();
        let again = cm.handle_datagram(&pdu("connect_req"), cpu(), now).unwrap();
        assert_eq!(again.send[0].bytes, first.send[0].bytes);
        assert!(again.notify.is_empty());
    }

    #[test]
    fn unsupported_opnum_gets_error_status_response() {
        let mut cm = cm();
        let mut read = pdu("prmend_req");
        read[68] = 2; // opnum Read (LE low byte)
        let o = cm.handle_datagram(&read, cpu(), Instant::now()).unwrap();
        let h = crate::rpc::RpcHeader::parse(&o.send[0].bytes).unwrap();
        assert_eq!(h.ptype, crate::rpc::PacketType::Response);
        assert_eq!(h.opnum, 2);
        let (n, blocks) = crate::rpc::NdrResponse::parse(&o.send[0].bytes[80..], crate::rpc::Drep::BIG).unwrap();
        assert_eq!(PnioStatus(n.status), PnioStatus::service_unsupported());
        assert!(blocks.is_empty());
    }

    #[test]
    fn rejected_connect_has_error_status_and_no_blocks() {
        let mut cm = Cm::new({ let mut m = DeviceModel::pnet_sample(MAC); m.slots.pop(); m }, Uuid::NIL);
        let o = cm.handle_datagram(&pdu("connect_req"), cpu(), Instant::now()).unwrap();
        let (n, blocks) = crate::rpc::NdrResponse::parse(&o.send[0].bytes[80..], crate::rpc::Drep::BIG).unwrap();
        assert_eq!(PnioStatus(n.status), PnioStatus::connect_reject(ConnectBlock::ExpectedSubmodule, 7));
        assert!(blocks.is_empty());
        assert_eq!(cm.state(), ArState::Idle);
    }

    #[test]
    fn wrong_interface_and_garbage_are_errors_not_panics() {
        let mut cm = cm();
        let mut bad = pdu("connect_req");
        bad[24] = 0xff;
        assert!(matches!(cm.handle_datagram(&bad, cpu(), Instant::now()), Err(RpcError::BadInterface(_))));
        assert!(matches!(cm.handle_datagram(&[1, 2, 3], cpu(), Instant::now()), Err(RpcError::TooShort { .. })));
    }

    #[test]
    fn tick_resends_app_ready_to_controller() {
        let mut cm = cm();
        let now = Instant::now();
        cm.handle_datagram(&pdu("connect_req"), cpu(), now).unwrap();
        cm.handle_datagram(&pdu("prmend_req"), cpu(), now).unwrap();
        let o = cm.tick(now + crate::cm::ar::APP_READY_TIMEOUT + std::time::Duration::from_millis(1));
        assert_eq!(o.send[0].bytes, pdu("appready_req"));
        assert_eq!(o.send[0].to, cpu_cm());
    }
}
```

- [ ] **Step 2: Run, expect compile failure.**
- [ ] **Step 3: Implement** per the behaviour list. Note the `serial_lo`/`serial_hi`/`server_boot` fields are 0 in every golden device PDU.
- [ ] **Step 4: Run tests + clippy + fmt** — expected 6 pass; `full_exchange_is_byte_exact_including_rpc_headers` is the gate.
- [ ] **Step 5: Commit + push** — `git commit -m "feat(cm): Cm glue — RPC PDUs <-> AR events, response cache, byte-exact full exchange"`

---
### Task 10: `dcp::set` — DCP Set IP suite (guarded) + dispatch

**Files:**
- Create: `crates/profinet-rt/src/dcp/set.rs`
- Modify: `crates/profinet-rt/src/dcp/mod.rs` (`pub mod set;`, re-exports, dispatch of `FrameId::GetSet`)

**Interfaces:**
- Produces `SetRequest { blocks: Vec<SetBlock> }`, `SetBlock { IpSuite { qualifier: u16, ip: [u8; 4], subnet: [u8; 4], gateway: [u8; 4] }, Other { option: u8, suboption: u8 } }`, `parse_set_request(block_bytes: &[u8]) -> Result<SetRequest, DcpError>` (Set request blocks carry a 2-byte **BlockQualifier** where responses carry BlockInfo → parse with `parse_blocks(bytes, true)` and read `block_info` as the qualifier).
- Produces `BlockError { Ok = 0x00, OptionNotSupported = 0x01, SuboptionNotSupported = 0x02, SuboptionNotSet = 0x03, ResourceError = 0x04, SetNotPossible = 0x05 }` (`u8` repr) — name it `SetBlockError` to avoid clashing with `cm::BlockError`.
- Produces `build_set_response(dst: MacAddr, src: MacAddr, xid: u32, results: &[(u8, u8, SetBlockError)]) -> Vec<u8>`: Ethernet header (untagged) + FrameID `0xfefd` + DCP header `{ Set, ResponseSuccess, xid, response_delay 0, data_length }` + one Control/Response block per result: option `5`, suboption `4`, length `3`, value `[option, suboption, error]`, then the odd-length pad byte (already handled by `write_blocks`).
- Produces `decide_set(req: &SetRequest, current_ip: [u8; 4]) -> Vec<(u8, u8, SetBlockError)>`: `IpSuite` with `ip == current_ip` → `Ok`; `IpSuite` with another ip → `SetNotPossible`; `Other{option, suboption}` → `SuboptionNotSupported`. The interface is **never** modified (spec §5.6).
- Modifies `handle_dcp_frame`: `FrameId::GetSet` with `service_id == Set` and `service_type == Request` → parse, decide with `cfg.properties.ip`, `log::info!` the decision, respond with `build_set_response(eth.src, cfg.mac, header.xid, ..)`. `Get` requests stay unanswered (`Ok(None)`, logged at debug). VLAN-tagged requests already work (`EthHeader::parse` strips the tag; the response is sent untagged like p-net's).

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::dcp::{handle_dcp_frame, DeviceConfig, DeviceProperties};
    use crate::eth::MacAddr;
    use crate::testutil::{golden, VLAN_PAYLOAD_OFF};

    fn cfg(ip: [u8; 4]) -> DeviceConfig {
        DeviceConfig {
            mac: MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]),
            properties: DeviceProperties {
                name_of_station: "rt-labs-dev".into(),
                type_of_station: "P-Net Sample Application".into(),
                vendor_id: 0x0493,
                device_id: 0x0002,
                device_role: 0x0100,
                device_instance: 1,
                device_options: vec![1, 2, 2, 2, 2, 3],
                ip,
                subnet: [255, 255, 255, 0],
                gateway: ip,
                ip_block_info: 1,
            },
        }
    }

    #[test]
    fn parse_golden_set_ip_request() {
        let f = golden("dcp_set_req");
        // FrameID at 18 (VLAN), DCP header at 20, blocks after the 10-byte header
        let (h, blocks) = crate::dcp::DcpHeader::parse(&f[VLAN_PAYLOAD_OFF + 2..]).unwrap();
        assert_eq!(h.xid, 0x0300_012d);
        let req = parse_set_request(blocks).unwrap();
        assert_eq!(
            req.blocks,
            vec![SetBlock::IpSuite { qualifier: 0, ip: [172, 16, 2, 10], subnet: [255, 255, 255, 0], gateway: [172, 16, 2, 10] }]
        );
    }

    #[test]
    fn set_ok_response_is_byte_exact_via_dispatch() {
        let resp = handle_dcp_frame(&golden("dcp_set_req"), &cfg([172, 16, 2, 10])).unwrap().unwrap();
        assert_eq!(resp, golden("dcp_set_res"));
    }

    #[test]
    fn different_ip_is_refused_not_applied() {
        let resp = handle_dcp_frame(&golden("dcp_set_req"), &cfg([172, 16, 2, 99])).unwrap().unwrap();
        // same frame, BlockError = SetNotPossible (0x05) at the last value byte
        let mut expected = golden("dcp_set_res");
        expected[32] = 0x05;
        assert_eq!(resp, expected);
    }

    #[test]
    fn unsupported_option_gets_suboption_not_supported() {
        let req = SetRequest { blocks: vec![SetBlock::Other { option: 2, suboption: 2 }] };
        assert_eq!(decide_set(&req, [1, 2, 3, 4]), vec![(2, 2, SetBlockError::SuboptionNotSupported)]);
    }

    #[test]
    fn get_request_is_ignored() {
        let mut f = golden("dcp_set_req");
        f[VLAN_PAYLOAD_OFF + 2] = 3; // ServiceID Get
        assert_eq!(handle_dcp_frame(&f, &cfg([172, 16, 2, 10])).unwrap(), None);
    }
}
```

- [ ] **Step 2: Run, expect compile failure.**
- [ ] **Step 3: Implement.** In `handle_dcp_frame` replace the `_ => Ok(None)` arm with `Some(FrameId::GetSet) => { let (header, blocks) = DcpHeader::parse(&payload[2..])?; if header.service_id != ServiceId::Set || header.service_type != ServiceType::Request { return Ok(None); } let req = parse_set_request(blocks)?; let results = decide_set(&req, cfg.properties.ip); log::info!(...); Ok(Some(build_set_response(eth.src, cfg.mac, header.xid, &results))) }` and keep `_ => Ok(None)`.
- [ ] **Step 4: Run the whole suite + clippy + fmt** — expected all green (existing dcp tests untouched).
- [ ] **Step 5: Commit + push** — `git commit -m "feat(dcp): Set IP suite (guarded: never re-addresses the interface) + dispatch"`

---

### Task 11: `eth` follow-ups + `rpc::transport` / `rpc::udp`

**Files:**
- Modify: `crates/profinet-rt/src/eth/afpacket.rs`, `crates/profinet-rt/src/eth/transport.rs`
- Create: `crates/profinet-rt/src/rpc/transport.rs`, `crates/profinet-rt/src/rpc/udp.rs`
- Modify: `crates/profinet-rt/src/rpc/mod.rs`

**Interfaces:**
- `AfPacketTransport::open(ifname)` now: socket protocol `htons(0x8892)` (`SockProtocol` has no PROFINET variant → build the socket with `libc::socket(AF_PACKET, SOCK_RAW, htons(0x8892))` or `nix` with a transmuted protocol; simplest: `unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_RAW | libc::SOCK_CLOEXEC, (ETHERTYPE_PROFINET as u16).to_be() as i32) }` wrapped in `OwnedFd`), bind with `sll_protocol = htons(0x8892)`, then `setsockopt(PACKET_ADD_MEMBERSHIP)` with `packet_mreq { mr_ifindex, mr_type: PACKET_MR_MULTICAST, mr_alen: 6, mr_address: 01:0e:cf:00:00:00 }`. VLAN-tagged PROFINET frames still arrive (the kernel matches the inner ethertype after offload strips the tag; with offload off they arrive as `0x8100` frames — keep `is_profinet_frame`).
- `AfPacketTransport::recv(timeout)`: `poll` on the fd with `timeout` (`None` → block); `Ok(None)` on timeout. Add `pub fn as_raw_fd(&self) -> RawFd` (needed by the `device` poll loop) — expose it through the trait: `EthTransport::raw_fd(&self) -> Option<RawFd>` (default `None`; mock returns `None`).
- Update the `recv` doc in `transport.rs` (timeout is honored now).
- Produces `rpc::transport::RpcTransport: Send + Sync { fn send(&self, buf: &[u8], to: SocketAddr) -> Result<(), RpcError>; fn recv(&self, timeout: Option<Duration>) -> Result<Option<(Vec<u8>, SocketAddr)>, RpcError>; fn raw_fd(&self) -> Option<RawFd> { None } }` and `MockRpcTransport { push_rx(bytes, from), sent() -> Vec<(Vec<u8>, SocketAddr)> }` mirroring `MockTransport`.
- Produces `rpc::udp::UdpRpcTransport::bind(addr: SocketAddr) -> Result<Self, RpcError>` (std `UdpSocket`, `set_nonblocking(false)`), `recv` via `poll` + `recv_from` (buffer 1500), `send` via `send_to`, `raw_fd` = the socket's fd. The ApplicationReady call goes out **from this same socket** (spec §7; fallback to a second socket only if the HIL shows the CPU ignoring it).

- [ ] **Step 1: Failing tests**

`rpc/transport.rs` — mock tests identical in spirit to `eth::transport`:
```rust
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
```

`rpc/udp.rs` — loopback round trip (no capability needed):
```rust
#[test]
fn udp_loopback_roundtrip_and_timeout() {
    let a = UdpRpcTransport::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let b = UdpRpcTransport::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let to = b.local_addr().unwrap();
    a.send(&[0xde, 0xad], to).unwrap();
    let (bytes, from) = b.recv(Some(Duration::from_millis(500))).unwrap().unwrap();
    assert_eq!(bytes, vec![0xde, 0xad]);
    assert_eq!(from, a.local_addr().unwrap());
    assert_eq!(b.recv(Some(Duration::from_millis(20))).unwrap(), None);
    assert!(b.raw_fd().is_some());
}
```

`eth/afpacket.rs` — keep the existing tests; the ignored `open_loopback_succeeds` becomes: open `lo`, `recv(Some(10 ms))` returns `Ok(None)` (timeout honored), `raw_fd().is_some()`.

- [ ] **Step 2: Run, expect failures/compile errors.**
- [ ] **Step 3: Implement.** `poll` helper shared by both backends: `fn wait_readable(fd: RawFd, timeout: Option<Duration>) -> std::io::Result<bool>` using `nix::poll::{poll, PollFd, PollFlags}` (`nix` `poll` feature added in Task 1; timeout in ms, `-1` for none; `EINTR` → retry). Put it in `crates/profinet-rt/src/eth/poll.rs` (`pub(crate)`), used by `afpacket.rs` and `rpc/udp.rs`.
- [ ] **Step 4: Run tests + clippy + fmt** — expected green; run the ignored one on the edge later.
- [ ] **Step 5: Commit + push** — `git commit -m "feat(eth,rpc): PROFINET-bound AF_PACKET socket + multicast membership + poll timeouts; UDP RPC transport"`

---
### Task 12: `device` — the acyclic loop

**Files:**
- Create: `crates/profinet-rt/src/device/mod.rs`
- Modify: `crates/profinet-rt/src/lib.rs` (`pub mod device;`)

**Interfaces:**
- Produces `DeviceConfig` is already taken by `dcp` — name this one `DeviceSetup { dcp: crate::dcp::DeviceConfig, model: DeviceModel, activity_seed: Uuid }`.
- Produces `Device<E: EthTransport, R: RpcTransport> { .. }` with `Device::new(setup: DeviceSetup, eth: E, rpc: R) -> Device<E, R>`, `run(&mut self, stop: &AtomicBool) -> Result<(), DeviceError>` (loop until `stop` is set), `step(&mut self, now: Instant, wait: Option<Duration>) -> Result<StepReport, DeviceError>` (one iteration: wait for readiness — via `poll` on both raw fds when available, else `recv(Some(wait))` on each transport in turn for the mock case — then service both transports and `Cm::tick`), `state(&self) -> ArState`, `on_state_change(&mut self, f: impl FnMut(ArState, Option<AbortReason>) + Send + 'static)` (callback, used by the example for logging).
- Produces `StepReport { eth_frames: usize, rpc_datagrams: usize, sent: usize }` and `DeviceError { Eth(TransportError), Rpc(RpcError) }`.
- Policy (spec §8): any parse error from `handle_dcp_frame` or `Cm::handle_datagram` is `log::warn!`ed and **dropped**; only transport I/O errors abort `run`.

`step` pseudo-code:
```
let deadline = cm.next_deadline(); let wait = min(wait, deadline - now)
if both raw fds available: poll([eth_fd, rpc_fd], wait) else: (mock) no wait
loop { match eth.recv(Some(0)) { Ok(Some(frame)) => match handle_dcp_frame(&frame, &setup.dcp) { Ok(Some(resp)) => eth.send(&resp)?, Ok(None) => {}, Err(e) => warn!(..) }, Ok(None) => break, Err(e) => return Err(..) } }
loop { match rpc.recv(Some(0)) { Ok(Some((buf, from))) => match cm.handle_datagram(&buf, from, now) { Ok(out) => dispatch(out), Err(e) => warn!(..) }, Ok(None) => break, Err(e) => return Err(..) } }
dispatch(cm.tick(now))
```
`dispatch`: `rpc.send(&o.bytes, o.to)` for each `Outgoing`; call the state-change callback for each `notify`.

- [ ] **Step 1: Failing tests** (`device/mod.rs`, with both mocks; the golden PDUs are fed as if received)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cm::model::DeviceModel;
    use crate::dcp::{DeviceConfig, DeviceProperties};
    use crate::eth::{MacAddr, MockTransport};
    use crate::rpc::{MockRpcTransport, Uuid};
    use crate::testutil::{golden, RPC_OFF};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const MAC: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);

    fn setup() -> DeviceSetup {
        DeviceSetup {
            dcp: DeviceConfig {
                mac: MAC,
                properties: DeviceProperties {
                    name_of_station: "rt-labs-dev".into(),
                    type_of_station: "P-Net Sample Application".into(),
                    vendor_id: 0x0493, device_id: 0x0002, device_role: 0x0100, device_instance: 1,
                    device_options: vec![1, 2, 2, 2, 2, 3],
                    ip: [172, 16, 2, 10], subnet: [255, 255, 255, 0], gateway: [172, 16, 2, 10], ip_block_info: 1,
                },
            },
            model: DeviceModel::pnet_sample(MAC),
            activity_seed: Uuid::parse_str("14af198a-1234-1056-8079-8cf319cd19f8").unwrap(),
        }
    }

    #[test]
    fn full_bring_up_through_the_loop() {
        let eth = MockTransport::new();
        let rpc = MockRpcTransport::new();
        let cpu = "172.16.2.100:54766".parse().unwrap();
        let cpu_cm = "172.16.2.100:34964".parse().unwrap();
        eth.push_rx(golden("dcp_set_req"));
        rpc.push_rx(golden("connect_req")[RPC_OFF..].to_vec(), cpu);
        rpc.push_rx(golden("write_req")[RPC_OFF..].to_vec(), cpu);
        rpc.push_rx(golden("prmend_req")[RPC_OFF..].to_vec(), cpu);
        rpc.push_rx(golden("appready_res")[RPC_OFF..].to_vec(), cpu_cm);
        let mut dev = Device::new(setup(), eth, rpc);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        dev.on_state_change(move |st, why| s2.lock().unwrap().push((st, why)));
        let r = dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
        assert_eq!((r.eth_frames, r.rpc_datagrams), (1, 4));
        assert_eq!(dev.state(), ArState::Data);
        assert_eq!(dev.eth().sent(), vec![golden("dcp_set_res")]);
        let sent = dev.rpc().sent();
        assert_eq!(sent.len(), 4);
        assert_eq!(sent[0].0, golden("connect_res")[RPC_OFF..]);
        assert_eq!(sent[3].0, golden("appready_req")[RPC_OFF..]);
        assert_eq!(sent[3].1, cpu_cm);
        assert_eq!(*seen.lock().unwrap(), vec![(ArState::Connected, None), (ArState::Data, None)]);
    }

    #[test]
    fn garbage_is_dropped_and_loop_continues() {
        let eth = MockTransport::new();
        let rpc = MockRpcTransport::new();
        let cpu = "172.16.2.100:54766".parse().unwrap();
        rpc.push_rx(vec![1, 2, 3], cpu);
        rpc.push_rx(golden("connect_req")[RPC_OFF..].to_vec(), cpu);
        let mut dev = Device::new(setup(), eth, rpc);
        dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
        assert_eq!(dev.state(), ArState::Connected);
    }

    #[test]
    fn run_stops_on_flag() {
        let stop = std::sync::atomic::AtomicBool::new(true);
        let mut dev = Device::new(setup(), MockTransport::new(), MockRpcTransport::new());
        dev.run(&stop).unwrap();
    }
}
```
(`Device::eth(&self) -> &E` and `rpc(&self) -> &R` accessors are part of the interface, for tests and the example.)

- [ ] **Step 2: Run, expect compile failure.**
- [ ] **Step 3: Implement.** `run`: `while !stop.load(Relaxed) { self.step(Instant::now(), Some(Duration::from_millis(200)))?; }` — the 200 ms cap keeps the stop flag responsive.
- [ ] **Step 4: Run tests + clippy + fmt** — expected 3 pass.
- [ ] **Step 5: Commit + push** — `git commit -m "feat(device): single-threaded acyclic loop (DCP + RPC + AR timers), log-and-drop policy"`

---

### Task 13: HIL example, capture replay test, docs, follow-ups

**Files:**
- Create: `crates/profinet-rt/examples/ar_bringup.rs`, `crates/profinet-rt/tests/ar_replay.rs`
- Modify: `README.md` (status table: `DCP Set-IP ✅`, `cm` ✅ (AR to DATA), `rpc` new row ✅), `FOLLOWUPS.md`, `docs/bench-pnet-device.md` (§6c HIL result), `docs/cm-golden-frames.md` (HIL comparison note)

- [ ] **Step 1: Write `tests/ar_replay.rs`** (integration; uses `tests/common/mod.rs`)

```rust
//! Replay the 2026-08-27 reference AR exchange through Device with mock transports and check
//! every emitted PDU is byte-identical to what p-net sent to the real S7-1500.
mod common;

use common::{golden, RPC_OFF};
use profinet_rt::cm::model::DeviceModel;
use profinet_rt::cm::ArState;
use profinet_rt::dcp::{DeviceConfig, DeviceProperties};
use profinet_rt::device::{Device, DeviceSetup};
use profinet_rt::eth::{MacAddr, MockTransport};
use profinet_rt::rpc::{MockRpcTransport, Uuid};
use std::time::{Duration, Instant};

const MAC: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);

#[test]
fn reference_exchange_replays_byte_exact() {
    let setup = DeviceSetup {
        dcp: DeviceConfig {
            mac: MAC,
            properties: DeviceProperties {
                name_of_station: "rt-labs-dev".into(),
                type_of_station: "P-Net Sample Application".into(),
                vendor_id: 0x0493, device_id: 0x0002, device_role: 0x0100, device_instance: 1,
                device_options: vec![1, 2, 2, 2, 2, 3],
                ip: [172, 16, 2, 10], subnet: [255, 255, 255, 0], gateway: [172, 16, 2, 10], ip_block_info: 1,
            },
        },
        model: DeviceModel::pnet_sample(MAC),
        activity_seed: Uuid::parse_str("14af198a-1234-1056-8079-8cf319cd19f8").unwrap(),
    };
    let eth = MockTransport::new();
    let rpc = MockRpcTransport::new();
    let cpu = "172.16.2.100:54766".parse().unwrap();
    let cpu_cm = "172.16.2.100:34964".parse().unwrap();
    eth.push_rx(golden("dcp_set_req"));
    for name in ["connect_req", "write_req", "prmend_req"] {
        rpc.push_rx(golden(name)[RPC_OFF..].to_vec(), cpu);
    }
    rpc.push_rx(golden("appready_res")[RPC_OFF..].to_vec(), cpu_cm);
    let mut dev = Device::new(setup, eth, rpc);
    dev.step(Instant::now(), Some(Duration::ZERO)).unwrap();
    assert_eq!(dev.state(), ArState::Data);
    assert_eq!(dev.eth().sent(), vec![golden("dcp_set_res")]);
    let sent: Vec<Vec<u8>> = dev.rpc().sent().into_iter().map(|(b, _)| b).collect();
    let expected: Vec<Vec<u8>> = ["connect_res", "write_res", "prmend_res", "appready_req"]
        .iter().map(|n| golden(n)[RPC_OFF..].to_vec()).collect();
    assert_eq!(sent, expected);
}
```
Run: `. "$HOME/.cargo/env" && cargo test -p profinet-rt --test ar_replay` → 1 passed.

- [ ] **Step 2: Write `examples/ar_bringup.rs`**

```rust
//! HIL bring-up: run the device on a real interface facing an S7-1500 configured with the
//! p-net sample GSDML (station `rt-labs-dev`). Success = a log line `AR state: Data`.
//! Needs cap_net_raw + cap_net_admin (AF_PACKET) — e.g. `setcap cap_net_raw,cap_net_admin+eip`.
use clap::Parser;
use profinet_rt::cm::model::DeviceModel;
use profinet_rt::dcp::{DeviceConfig, DeviceProperties};
use profinet_rt::device::{Device, DeviceSetup};
use profinet_rt::eth::{AfPacketTransport, MacAddr};
use profinet_rt::rpc::{UdpRpcTransport, Uuid, PNIO_UDP_PORT};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Parser)]
struct Args {
    /// Interface facing the controller (e.g. eno2)
    #[arg(long)]
    iface: String,
    /// PROFINET station name
    #[arg(long, default_value = "rt-labs-dev")]
    name: String,
    /// IPv4 address configured on the interface (must equal the one TIA assigns)
    #[arg(long)]
    ip: std::net::Ipv4Addr,
}

fn mac_of(iface: &str) -> MacAddr {
    let s = std::fs::read_to_string(format!("/sys/class/net/{iface}/address")).expect("iface mac");
    let mut m = [0u8; 6];
    for (i, p) in s.trim().split(':').enumerate() { m[i] = u8::from_str_radix(p, 16).expect("mac"); }
    MacAddr(m)
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let a = Args::parse();
    let mac = mac_of(&a.iface);
    let ip = a.ip.octets();
    let setup = DeviceSetup {
        dcp: DeviceConfig {
            mac,
            properties: DeviceProperties {
                name_of_station: a.name.clone(),
                type_of_station: "profinet-rt bring-up".into(),
                vendor_id: 0x0493, device_id: 0x0002, device_role: 0x0100, device_instance: 1,
                device_options: vec![1, 2, 2, 2, 2, 3],
                ip, subnet: [255, 255, 255, 0], gateway: ip, ip_block_info: 1,
            },
        },
        model: { let mut m = DeviceModel::pnet_sample(mac); m.station_name = a.name; m },
        activity_seed: { let mut b = [0x14, 0xaf, 0x19, 0x8a, 0x12, 0x34, 0x10, 0x56, 0x80, 0x79, 0, 0, 0, 0, 0, 0]; b[10..].copy_from_slice(&mac.0); Uuid(b) },
    };
    let eth = AfPacketTransport::open(&a.iface).expect("AF_PACKET (need cap_net_raw)");
    let rpc = UdpRpcTransport::bind(std::net::SocketAddr::from(([0, 0, 0, 0], PNIO_UDP_PORT))).expect("udp 34964");
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    ctrlc_like(move || s.store(true, Ordering::Relaxed));
    let mut dev = Device::new(setup, eth, rpc);
    dev.on_state_change(|st, why| match why {
        None => log::info!("AR state: {st:?}"),
        Some(r) => log::warn!("AR state: {st:?} (abort: {r:?})"),
    });
    log::info!("device up on {} as {:?}, waiting for the controller", a.iface, mac);
    if let Err(e) = dev.run(&stop) { log::error!("device loop ended: {e}"); std::process::exit(1); }
}

/// Minimal SIGINT hook without a crate: spawn a thread blocking on stdin EOF is not portable
/// under systemd, so use libc::signal with a static flag.
fn ctrlc_like(f: impl Fn() + Send + 'static) {
    static HANDLER: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>> = std::sync::OnceLock::new();
    let _ = HANDLER.set(Box::new(move || f()));
    extern "C" fn on_sig(_: libc::c_int) { if let Some(h) = HANDLER.get() { h(); } }
    unsafe { libc::signal(libc::SIGINT, on_sig as libc::sighandler_t); libc::signal(libc::SIGTERM, on_sig as libc::sighandler_t); }
}
```
(`impl Fn() + Send + 'static` stored as `Send + Sync` requires the closure to be `Sync`: the `Arc<AtomicBool>` one is. Add `libc` is already a dependency; the example compiles with `cargo build --example ar_bringup`.)

Run: `. "$HOME/.cargo/env" && cargo build --example ar_bringup && cargo clippy --all-targets -- -D warnings && cargo fmt --all --check`.

- [ ] **Step 3: HIL on the edge**

Build for the edge. First try a plain build copied over (`scp target/debug/examples/ar_bringup maintenance@192.168.1.21:bench/`); if it fails with a glibc version error, add the musl target (`rustup target add x86_64-unknown-linux-musl`, `cargo build --release --example ar_bringup --target x86_64-unknown-linux-musl`) and copy that binary. Then, on the edge (user runs the `setcap`, everything else over SSH):
```bash
sudo /usr/sbin/setcap cap_net_raw,cap_net_admin+eip /home/maintenance/bench/ar_bringup
~/bench/pnet-stop.sh
nohup ~/bench/capture.sh hil-ar-bringup > ~/bench/logs/capture-hil.out 2>&1 &
RUST_LOG=info ~/bench/ar_bringup --iface eno2 --name rt-labs-dev --ip 172.16.2.10 2>&1 | tee ~/bench/logs/ar_bringup.log
# wait for "AR state: Data", then ~2 s, Ctrl-C, ~/bench/capture-stop.sh
```
Expected log sequence: `device up`, DCP Set decision line, `AR state: Connected`, `AR state: AppReadySent` is not notified (internal), `AR state: Data`. Then, ~96 ms later, the CPU's ERR-RTA on the alarm channel is *not* seen by us (L2 alarm channel is Plan 5) — the CPU restarts DCP Identify; our device answers again and a **second AR** comes up (Connect → … → Data) — this loop is the expected Plan 3 end state; note the period in the log.

Compare with tshark: `"$TSHARK" -r hil-ar-bringup-*.pcapng -Y "dcerpc or pn_dcp" -T fields -e frame.time_relative -e eth.src -e _ws.col.Info` and check the response PDUs against p-net's: the only allowed differences are the RPC activity/seq numbers, the ARUUID/session chosen by the CPU, the `type_of_station` string, and our IP block.

Record the outcome in `docs/bench-pnet-device.md` §6c (date, binary, capture name, log excerpt, differences observed). If the CPU does not answer the ApplicationReady sent from port 34964 (spec §7 open point): switch `UdpRpcTransport` to a second ephemeral socket for calls, document it, re-run.

- [ ] **Step 4: Docs + follow-ups**

`FOLLOWUPS.md`: mark resolved — `sll_protocol`/BPF (bound to 0x8892 + multicast), `recv` timeout (poll), `DeviceRole` (frame #37: `01 01`, our `0x0100` accepted by the CPU — state what the HIL showed); add open — RPC fragmentation, `ModuleDiffBlock`, `Read`/`ReadImplicit`, `PACKET_AUXDATA` (Plan 4), PnioStatus constants to re-verify against the purchased IEC 61158-6-10, alarm channel ERR-RTA handling (Plan 5), MultipleWrite outer `record_data_length = 0` mirrored from p-net. `README.md` status table updated. `docs/cm-golden-frames.md`: add the HIL note.

- [ ] **Step 5: Final verification + commit + push**

Run: `. "$HOME/.cargo/env" && cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test --all`
Expected: all green (≈ 46 + 60 new tests + doctest).

```bash
git add -A crates/profinet-rt README.md FOLLOWUPS.md docs
git commit -m "feat: AR bring-up example + capture replay test + HIL results (Plan 3 close-out)"
git push
```

Then hand over to `superpowers:finishing-a-development-branch` (merge `feat/cm-ar` into `main`, push).

---

## Self-review notes (done while writing)

- Spec coverage: §5.1-5.2 → Tasks 2-3; §5.3 → Task 4; §5.6 → Task 10; §5.7 → Task 1; §6 → Tasks 5, 8; §5.4-5.5 → Tasks 6-7; §7 → Tasks 11-13; §8 → Tasks 4, 9, 12; §9 → every task's tests + Task 13; §10 → Task 1.
- Deviations from the spec, all deliberate: timers are exposed as `next_deadline()` instead of `SetTimer` actions (Task 8); `Cm` keeps `controller_addr` rather than storing it in `ArParams` (Task 9); `DeviceConfig` name collision → `DeviceSetup` (Task 12).
- Type names used across tasks: `Uuid`, `Drep`, `RpcHeader`, `NdrRequest/NdrResponse`, `PnioStatus`, `ConnectBlock`, `BlockHeader`, `ty::*`, `ArBlockReq`, `IocrBlockReq`, `IocrApi`, `IocrObject`, `ExpectedSubmoduleBlockReq`, `AlarmCrBlockReq`, `DeviceModel`, `ConnectReq`, `ArParams`, `IocrParams`, `WriteReq`, `Record`, `ControlBlock`, `cmd::*`, `Ar`, `ArState`, `AbortReason`, `Event`, `Action`, `ArContext`, `Cm`, `CmOutput`, `Outgoing`, `RpcTransport`, `MockRpcTransport`, `UdpRpcTransport`, `Device`, `DeviceSetup`, `StepReport` — consistent across Tasks 2-13.

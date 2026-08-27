# CM/AR golden frames

## Provenance

Captured on the bench on 2026-08-27, PLC side CPU 1515-2 PN, firmware V2.9.4, IO-device
side p-net v0.2.0. Capture file: `captures/ar-connect-2026-08-27-164334.pcapng`, decoded
with tshark 4.6.6. Ethernet MAC addresses: CPU `8c:f3:19:cd:19:f8`, p-net device
`ec:1c:5d:61:e7:3f`.

Two DCP frames in the sequence (`dcp_set_req` / frame 47, going CPU -> p-net) are VLAN
tagged (802.1Q, `81 00`, no priority/VLAN id set, i.e. tag `00 00`); all other frames
in this set are untagged IPv4/UDP. `ident_ok_pnet` (frame 37) and `dcp_set_res` (frame
48) are raw PROFINET-DCP frames (EtherType `0x8892`), not VLAN tagged and not
IP/UDP/RPC — they use `VLAN_PAYLOAD_OFF`/direct FrameID parsing, not `RPC_OFF`.

The RPC-carrying frames (`connect_req`, `connect_res`, `write_req`, `write_res`,
`prmend_req`, `prmend_res`, `appready_req`, `appready_res`) are untagged
Ethernet/IPv4/UDP frames whose DCE-RPC PDU starts at byte offset 42
(`RPC_OFF = 14 (Ethernet) + 20 (IPv4) + 8 (UDP) = 42`), addressed to/from UDP port 34964
(`0x88 94`, the PROFINET RPC/DCE port) on one side and an ephemeral or reserved port on
the other.

## Inventory

| File | Frame | Direction | Bytes | DREP | Notes |
|---|---|---|---|---|---|
| `ident_ok_pnet.hex` | 37 | p-net -> CPU | 144 | — | DCP Ident Ok, Xid `0x0300012c` |
| `dcp_set_req.hex` | 47 | CPU -> p-net | 64 | — | DCP Set IP, VLAN tagged, Xid `0x0300012d` |
| `dcp_set_res.hex` | 48 | p-net -> CPU | 34 | — | DCP Set Ok |
| `connect_req.hex` | 50 | CPU -> p-net | 699 | LE | Connect request, RPC PDU at offset 42 |
| `connect_res.hex` | 53 | p-net -> CPU | 232 | BE | Connect response, RPC PDU at offset 42 |
| `write_req.hex` | 54 | CPU -> p-net | 486 | LE | Write, MultipleWrite |
| `write_res.hex` | 56 | p-net -> CPU | 462 | BE | Write response |
| `prmend_req.hex` | 57 | CPU -> p-net | 174 | LE | Control, PrmEnd |
| `prmend_res.hex` | 58 | p-net -> CPU | 174 | BE | Control, PrmEnd Done |
| `appready_req.hex` | 59 | p-net -> CPU | 174 | BE | Control, ApplicationReady (src port 49153) |
| `appready_res.hex` | 60 | CPU -> p-net | 174 | LE | Control, ApplicationReady Done, flags1 `0x0a` |

## Key facts

- CPU-originated requests carry DCE-RPC `drep = 10 00 00` (little-endian); p-net's
  responses carry `drep = 00 00 00` (big-endian) with `flags1 = 0x28` (response,
  fragment, no-fragment-ack — the p-net response flag combination seen throughout).
- In each response's NDR header, `max_count` **echoes the corresponding request's
  `args_max`**: Connect 557, Write 344, Control/PrmEnd 32, Control/ApplicationReady
  1340. The last is p-net's own request (`appready_req.hex`, device -> CPU,
  `args_max = 1340`), and the CPU's `appready_res.hex` echoes that same 1340 back —
  it is not the PrmEnd value; the two Control operations use different `args_max`.
  This mirroring is not independently computed by the responder; it is copied from
  the request.
- p-net answers `MaxAlarmDataLength = 200` (`0x00c8`) in the Connect response's
  AlarmCRBlockRes even though the CPU's AlarmCRBlockReq asked for 256 (`0x0100`).
- In the Write response (`write_res.hex`), the outer NDR `record_data_length` field is
  `0` (mirrored/zeroed for byte-exactness against the capture — it is not recomputed
  from the sum of the per-block payload lengths).
- The DCP Set response block (`dcp_set_res.hex`, DCP block starting after the DCP
  header) is `05 04 00 03 | 01 02 00 | 00` — Option 5 (Control), Suboption 4
  (Signal/Response), block length 3, block-qualifier-echoing `01 02 00`
  (Set-IP option/suboption echoed), status/pad `00`.

## RPC header (DCE-RPC connectionless, 80 bytes)

Offsets are relative to the start of the RPC PDU (`RPC_OFF = 42` in the untagged
Ethernet/IPv4/UDP golden frames).

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | `rpc_vers` (= 4) |
| 1 | 1 | `ptype` |
| 2 | 1 | `flags1` |
| 3 | 1 | `flags2` |
| 4 | 3 | `drep[3]` (drep[0] byte order: `0x10`=LE, `0x00`=BE) |
| 7 | 1 | `serial_hi` |
| 8 | 16 | `object` UUID |
| 24 | 16 | `interface` UUID |
| 40 | 16 | `activity` UUID |
| 56 | 4 | `server_boot_time` |
| 60 | 4 | `if_version` |
| 64 | 4 | `seqnum` |
| 68 | 2 | `opnum` |
| 70 | 2 | `ihint` |
| 72 | 2 | `ahint` |
| 74 | 2 | `frag_len` |
| 76 | 2 | `frag_num` |
| 78 | 1 | `auth_proto` |
| 79 | 1 | `serial_lo` |

Total: 80 bytes. NDR payload (the PROFINET block stream) follows immediately at
offset 80.

### NDR request/response counters (5 x u32, 20 bytes)

Requests carry, immediately after the 80-byte RPC header, five little-endian `u32`
counters before the PROFINET block stream: `args_max`, `args_length`, `max_count`,
`offset`, `actual_count`. Responses carry the same five fields, byte-order matching
the response's own `drep` (big-endian for p-net's responses), with `max_count`
mirroring the request's `args_max` per the "Key facts" note above.

## Per-frame block summaries

- **`connect_req.hex`** (Connect.req, RPC header + NDR counters, then blocks in
  order): ARBlockReq, IOCRBlockReq (Input), IOCRBlockReq (Output), AlarmCRBlockReq,
  ExpectedSubmoduleBlockReq (module 1), ExpectedSubmoduleBlockReq (module 2).
- **`connect_res.hex`** (Connect.res): ARBlockRes, IOCRBlockRes (Input), IOCRBlockRes
  (Output), AlarmCRBlockRes, ModuleDiffBlock (`81 06`, one entry, station name
  `rt-labs-dev`).
- **`write_req.hex`** (Write.req, MultipleWrite / PDU stacking, records addressed by
  slot/subslot/index): five stacked IODWriteReqHeader + record-data blocks (indices
  `0x0000`/`0x0001`/`0x8071`/`0x8071`-family covering the multiple-write records the
  CPU pushes at connect time, including the I&M0 records at the end).
- **`write_res.hex`** (Write.res): five stacked IODWriteResHeader blocks mirroring the
  request's five records, each echoing status/index/slot/subslot, no record payload.
- **`prmend_req.hex`** / **`prmend_res.hex`** (Control.req / Control.res,
  PrmEnd / PrmEnd Done): single ControlBlockConnect-family PDU (`BlockType 0x0110`
  request / `0x8110` response), IOXBlockReq/Res with `PrmEnd` control command,
  followed by the RPC ready block (block type `0x0002`).
- **`appready_req.hex`** / **`appready_res.hex`** (Control.req / Control.res,
  ApplicationReady / ApplicationReady Done): same ControlBlockConnect-family shape
  (`BlockType 0x0112` request / `0x8112` response) with the `ApplicationReady` control
  command.

# DCP — Reference Golden Frames (ground truth)

> Set-NameOfStation / Signal / Reset-to-factory frames: see [`pdev-golden-frames.md`](pdev-golden-frames.md) (provisional until the capture campaign).

Real frames captured on bench: **S7-1500 CPU 1515-2 PN (FW V2.9)** = IO-Controller
↔ PLCSIM `i-device` instance. Capture `captures/dcp-identify.pcapng`, cross-decoded
with Wireshark/tshark 4.6.6 (`pn_dcp` dissector). Used as **byte-exact test vectors**
for the `dcp` module.

Common constants:
- PROFINET EtherType: `0x8892` (`eth` module already in place)
- DCP Identify multicast MAC: `01:0e:cf:00:00:00`
- FrameIDs: `0xfefe` = Identify **request** (multicast); `0xfeff` = Identify **response**
  (unicast); `0xfefd` = Get/Set; `0xfefc` = Hello.
- DCP header (after FrameID): `ServiceID(1) ServiceType(1) Xid(4) ResponseDelay/resv(2) DCPDataLength(2)` then blocks.
- ServiceID: Get=3, Set=4, Identify=5, Hello=6. ServiceType: bit0 0=Request 1=Response-success.
- **TLV block**: `Option(1) Suboption(1) DCPBlockLength(2) [BlockInfo(2)] Value(...)`,
  **padded to an even length** (byte `0x00` if odd).
  - ⚠️ In an **Identify request**, the NameOfStation *filter* block has **NO** BlockInfo
    (value = raw name). In a **response**, each block begins with 2 **BlockInfo** bytes.

---

## 1. Identify REQUEST (controller → multicast) — 56 bytes, FrameID 0xfefe

```
010ecf000000ec1c5d61e73f8892fefe0500030001520001000c02020008692d646576696365000000000000000000000000000000000000
```

| Offset | Bytes | Field | Value |
|---|---|---|---|
| 0 | `01 0e cf 00 00 00` | Eth dst | DCP multicast |
| 6 | `ec 1c 5d 61 e7 3f` | Eth src | controller (Siemens) |
| 12 | `88 92` | EtherType | PROFINET |
| 14 | `fe fe` | FrameID | Identify req |
| 16 | `05` | ServiceID | Identify |
| 17 | `00` | ServiceType | Request |
| 18 | `03 00 01 52` | Xid | 0x03000152 |
| 22 | `00 01` | ResponseDelay | 1 |
| 24 | `00 0c` | DCPDataLength | 12 |
| 26 | `02 02 00 08` | Block: opt=2(DeviceProperties) sub=2(NameOfStation) len=8 | **filter, no BlockInfo** |
| 30 | `69 2d 64 65 76 69 63 65` | NameOfStation | "i-device" |
| 38.. | `00…` | Padding | minimum 56-byte frame padding |

## 2. Identify RESPONSE (device → controller) — 114 bytes, FrameID 0xfeff

```
ec1c5d61e73f02c0a8010f028892feff050103000152000000580202000a0000692d646576696365020500040000020702010012000053372d313530302028504c4353494d29020300060000002a010e020400040000000002070004000010640102000e0001c0a8013dffffff00c0a8013d
```

Header: `feff` / ServiceID=05 / ServiceType=01 / Xid=`03000152` (= that of the req) /
ResponseDelay=`0000` / DCPDataLength=`0058` (88). Then 7 blocks (each with **2-byte BlockInfo**):

| opt.sub | Name | len | BlockInfo | Decoded value |
|---|---|---|---|---|
| 2.2 | NameOfStation | 10 | `0000` | "i-device" |
| 2.5 | DeviceOptions | 4 | `0000` | `0207` |
| 2.1 | TypeOfStation | 18 | `0000` | "S7-1500 (PLCSIM)" |
| 2.3 | DeviceID (Vendor/Device) | 6 | `0000` | VendorID `0x002A` (Siemens), DeviceID `0x010E` |
| 2.4 | DeviceRole | 4 | `0000` | `0000` |
| 2.7 | DeviceInstance | 4 | `0000` | `1064` |
| 1.2 | IPParameter | 14 | `0001` | ip `192.168.1.61` mask `255.255.255.0` gw `192.168.1.61` |

Notes:
- The device responds with **the request's Xid** (request/response correlation).
- IPParameter BlockInfo `0x0001` = IP set/valid; values in big-endian (`c0a8013d` = 192.168.1.61).
- "i-device" = 8 bytes (even) → no padding byte after this block; TypeOfStation = 16 bytes (even) too.

## Sources
- `captures/dcp-identify.pcapng` (bench 2026-06-26)
- Decoded with: Wireshark/tshark 4.6.6 `pn_dcp`
- ⚠️ No golden frames yet for **Get / Set-name / Set-IP / Flash** (PLCSIM does not receive
  Set; to be captured on a real device or via p-net). Identical TLV structure; to be validated later.

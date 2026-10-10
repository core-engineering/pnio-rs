# PDEV / LLDP / DCP wire facts (Plan 8)

> **Status: PROVISIONAL.** Sections 1-3 are pinned from the public Wireshark dissector sources (commit/URL cited per fact), not from the capture campaign. The capture campaign (plan Task 2) will add section 4 (campaign table), section 5 (decoded goldens) and confirm or override every line below; a confirmed line gets the frame number that proves it.

**Sources.** Wireshark `master` at commit
[`7e397dba1e948fa1e2e117083e9a7cf7df901472`](https://github.com/wireshark/wireshark/commit/7e397dba1e948fa1e2e117083e9a7cf7df901472)
(2026-10-10), fetched raw from the GitHub mirror. Line numbers below refer to that commit.

| Tag | File (permalink base) |
|---|---|
| **[DCP]** | [`plugins/epan/profinet/packet-pn-dcp.c`](https://github.com/wireshark/wireshark/blob/7e397dba1e948fa1e2e117083e9a7cf7df901472/plugins/epan/profinet/packet-pn-dcp.c) |
| **[PNIO]** | [`plugins/epan/profinet/packet-dcerpc-pn-io.c`](https://github.com/wireshark/wireshark/blob/7e397dba1e948fa1e2e117083e9a7cf7df901472/plugins/epan/profinet/packet-dcerpc-pn-io.c) (there is no `packet-pn-io.c`: the PNIO record dissector is `packet-dcerpc-pn-io.c`; 404 on both `epan/dissectors/` and `plugins/epan/profinet/packet-pn-io.c`) |
| **[PN]** | [`plugins/epan/profinet/packet-pn.c`](https://github.com/wireshark/wireshark/blob/7e397dba1e948fa1e2e117083e9a7cf7df901472/plugins/epan/profinet/packet-pn.c) (shared helpers: `dissect_pn_align4`) |
| **[LLDP]** | [`epan/dissectors/packet-lldp.c`](https://github.com/wireshark/wireshark/blob/7e397dba1e948fa1e2e117083e9a7cf7df901472/epan/dissectors/packet-lldp.c) |
| **[OUI]** | [`epan/oui.h`](https://github.com/wireshark/wireshark/blob/7e397dba1e948fa1e2e117083e9a7cf7df901472/epan/oui.h) |
| **[XSD]** | TIA V21 install: `C:\Program Files\Siemens\Automation\Portal V21\Data\Hwcn\Custom\Interpreter\GSD\XSD\GSDML-DeviceProfile-v2.31.xsd` (sha256 `f8ddb451…0f918469`) and `GSDML-Primitives-v2.31.xsd` (sha256 `259e8561…0ca0c83d44c`) |
| **[GOLD]** | This repo, `crates/pnio/testdata/cm/write_req.hex` (frame 54 of the Plan 3 capture: CPU 1515-2 MultipleWrite) |

All multi-byte fields are big-endian (the PNIO block dissector forces it: `drep[0] &= ~DREP_LITTLE_ENDIAN;` "from here, we only have big endian" — [PNIO] `dissect_block`, L17856ff).

## 1. DCP

### 1.1 Option 5 (Control) suboptions

[DCP] L370-385:

```c
#define PNDCP_SUBOPTION_CONTROL_START_TRANS 0x01
#define PNDCP_SUBOPTION_CONTROL_END_TRANS   0x02
#define PNDCP_SUBOPTION_CONTROL_SIGNAL      0x03
#define PNDCP_SUBOPTION_CONTROL_RESPONSE    0x04
#define PNDCP_SUBOPTION_CONTROL_FACT_RESET  0x05
#define PNDCP_SUBOPTION_CONTROL_RESET_TO_FACT  0x06
    { PNDCP_SUBOPTION_CONTROL_FACT_RESET,  "Reset Factory Settings" },
    { PNDCP_SUBOPTION_CONTROL_RESET_TO_FACT,"Reset to Factory" },
```

| Sub | Name (dissector) | Payload as decoded (`dissect_PNDCP_Suboption_Control`, L1400-1484) |
|---|---|---|
| 1 | Start Transaction | BlockQualifier u16 |
| 2 | End Transaction | BlockQualifier u16 |
| 3 | Signal | BlockQualifier u16, SignalValue u16 |
| 4 | Response | Option u8, Suboption u8, BlockError u8 |
| 5 | Reset Factory Settings (legacy) | BlockQualifier u16, only `0x0000` "Default" named (`pn_dcp_suboption_other`) |
| 6 | Reset to Factory | BlockQualifier u16 carrying the mode (1.2) |

A Set block with `block_length == 0` under option 5 is shown as "Erroneous DCPSet block" (L1418-1420).

### 1.2 ResetToFactory (5/6) BlockQualifier

[DCP] L213-226 and L1471-1477, header field L1890-1893:

```c
static const value_string pn_dcp_BlockQualifier[] = {
    /* The mode is encoded in bits 1-15. */
    { 0x0000, "Reserved" },
    { 0x0001, "Reset application data" },
    { 0x0002, "Reset communication parameter" },
    { 0x0003, "Reset engineering parameter" },
    { 0x0004, "Reset all stored data except the security configuration" },
    { 0x0005, "Reserved" },  { 0x0006, "Reserved" },  { 0x0007, "Reserved" },
    { 0x0008, "Reset all stored data to factory values" },
    { 0x0009, "Reset and restore data" },
...
            val_to_str_const(BlockQualifier >> 1, pn_dcp_BlockQualifier, "reserved"));
...
          { "BlockQualifier: ResettoFactory", "pn_dcp.block_qualifier_reset",
            FT_UINT16, BASE_DEC, VALS(pn_dcp_BlockQualifier), 0xFFFE,
```

- Mode = `BlockQualifier >> 1` (field mask `0xFFFE`); bit 0 is not interpreted by the dissector.
- **Mode 2 "Reset communication parameter" → BlockQualifier `0x0004`.**
- **Mode 4 "Reset all stored data except the security configuration" → BlockQualifier `0x0008`.**
- Mode 8 "Reset all stored data to factory values" → `0x0010`; mode 9 "Reset and restore data" → `0x0012`; mode 1 → `0x0002`; mode 3 → `0x0006`.
- The encoding matches spec §4.1 (`mode << 1`). **Note on spec §3.4 / §5.1:** the dissector's text for mode 4 is "all stored data *except the security configuration*", and the name the spec gives to mode 8 ("device restart", §2 Out) is "Reset all stored data to factory values" in the dissector. Which mode TIA sends for "Reset to factory settings" with / without "retain I&M data" is capture-only.

### 1.3 Signal (5/3)

[DCP] L279-282 and L1433-1439:

```c
static const value_string pn_dcp_suboption_control_signal_value[] = {
    {0x0100, "Flash Once"},
...
        case PNDCP_SUBOPTION_CONTROL_SIGNAL:
            offset = dissect_pn_uint16(tvb, offset, pinfo, tree, hf_pn_dcp_block_qualifier, &block_qualifier);
            offset = dissect_pn_uint16(tvb, offset, pinfo, tree, hf_pn_dcp_suboption_control_signal_value, &u16SignalValue);
```

Payload: BlockQualifier u16 (decoded with the temporary/permanent table of 1.4), SignalValue u16. **SignalValue `0x0100` = "Flash Once"**, the only named value. Matches spec §4.1.

### 1.4 NameOfStation Set (2/2) BlockQualifier

[DCP] L206-211 and L700-705 (BlockQualifier is read for every `Set` request block under option 2):

```c
static const value_string pn_dcp_block_qualifier[] = {
    { 0x0000, "Use the value temporary" },
    { 0x0001, "Save the value permanent" },
    /*0x0002 - 0xffff reserved */
```

The dissector compares the whole u16 (no mask): `0x0000` temporary, `0x0001` permanent, anything else "Unknown"/reserved. Same table for IP (1/2) Set. The name follows the qualifier as `block_length - 2` bytes of ASCII (L780; comment above it, comment there cites IEC 61158-6-10 Ed. 4 §4.3.1.4.15: "OctetString with 1 to 240 octets", RFC 5890). Matches spec §4.1 (bit 0 = permanent); the spec should treat `0x0002..0xFFFF` as reserved (→ answer an error) rather than masking bit 0.

### 1.5 BlockError (Control/Response)

[DCP] L188-199:

```c
static const value_string pn_dcp_block_error[] = {
    { 0x00, "Ok" },
    { 0x01, "Option unsupp." },
    { 0x02, "Suboption unsupp. or no DataSet avail." },
    { 0x03, "Suboption not set" },
    { 0x04, "Resource Error" },
    { 0x05, "SET not possible by local reasons" },
    { 0x06, "In operation, SET not possible" },
    { 0x07, "SET or GET not possible by security reasons" },
```

Matches the four codes used in spec §4.1 (`0x00`, `0x02`, `0x03`, `0x05`). Additional codes the spec does not use: `0x01` Option unsupported, `0x04` Resource error, **`0x06` "In operation, SET not possible"** (the standard code for refusing a Set while an AR is running — spec §3.3 deliberately accepts instead), `0x07` security. A non-zero BlockError is flagged by the dissector as an expert info (L1451-1454).

## 2. PNIO records (BlockTypes, layouts, diagnosis values)

### 2.1 Block header and padding rule

- Block header ([PNIO] `dissect_block`, L17856ff): BlockType u16, BlockLength u16, BlockVersionHigh u8, BlockVersionLow u8. "block length is without type and length fields, but with version field" → body after the header = `BlockLength - 2`; the next block starts at `header_start + 4 + BlockLength` (`offset += u16BodyLength;`, L18455). Bytes inside BlockLength the block dissector did not consume (e.g. trailing padding) are skipped silently.
- A block list ends early when the next u16 is `0x0000` (`if (tvb_get_ntohs(tvb, offset) == 0) return tvb_reported_length(tvb);`, L17871).
- "Padding" = `dissect_pn_align4` ([PN] L1506-1519): `padding = WS_PADDING_TO_4(offset);` — alignment is **relative to the start of the tvb**, not to the block. For a top-level record block the tvb is the DCE/RPC stub (NDR header 20 bytes + IODReadResHeader 64 bytes = 84, 4-aligned), so in practice padding is relative to the block start. For the sub-blocks of PDPortDataCheck / PDPortDataAdjust / MultipleBlockHeader the dissector creates **one** sub-tvb starting at the first sub-block (`tvb_new_subset_length(tvb, offset, u16BodyLength); dissect_blocks(new_tvb, 0, …)`), so alignment is relative to the first sub-block: a writer that keeps every block length (4 + BlockLength) a multiple of 4 decodes identically under both readings. Whether TIA pads short sub-blocks (CheckPeers, AdjustMulticastBoundary) to a multiple of 4 inside their BlockLength is capture-only.
- Every block below is decoded only for **BlockVersion 1.0** (`if (u8BlockVersionHigh != 1 || u8BlockVersionLow != 0) … "Block version %u.%u not implemented yet!"`), except AdjustDomainBoundary (1.0 and 1.1).

### 2.2 BlockType values

[PNIO] `pn_io_block_type[]` (L1337ff) and the dispatch switch in `dissect_block` (L18073-18321):

| BlockType | Dissector string (exact) | Line | Dissect function |
|---|---|---|---|
| `0x0200` | `"PDPortDataCheck"` | 1407 | `dissect_PDPortData_Check_block` |
| `0x0201` | `"PDevData"` | 1408 | `dissect_PDevData_block` |
| `0x0202` | `"PDPortDataAdjust"` | 1409 | `dissect_PDPortData_Adjust_block` |
| `0x0209` | `"AdjustDomainBoundary"` | 1416 | `dissect_AdjustDomainBoundary_block` |
| `0x020A` | `"CheckPeers"` | 1417 | `dissect_CheckPeers_block` |
| `0x020B` | `"CheckLineDelay"` | 1418 | `dissect_CheckLineDelay_block` |
| `0x020C` | `"Checking MAUType"` | 1419 | `dissect_CheckMAUType_block` |
| `0x020D` | — (absent from the table and the switch) | — | — |
| `0x020E` | `"Adjusting MAUType"` | 1420 | `dissect_AdjustMAUType_block` |
| `0x020F` | `"PDPortDataReal"` | 1421 | `dissect_PDPortDataReal_block` |
| `0x0210` | `"AdjustMulticastBoundary"` | 1422 | `dissect_AdjustMulticastBoundary_block` |
| `0x0213` | `"PDInterfaceMrpDataCheck"` | 1425 | `dissect_PDInterfaceMrpDataCheck_block` |
| `0x0214` / `0x0215` | `"PDPortMrpDataAdjust"` / `"PDPortMrpDataReal"` | 1426-1427 | `dissect_PDPortMrpData_block` |
| `0x021B` | `"Adjust LinkState"` | 1433 | `dissect_AdjustPortState_block` |
| `0x021C` | `"Checking LinkState"` | 1434 | `dissect_CheckPortState_block` |
| `0x021E` | `"CheckSyncDifference"` | 1436 | `dissect_CheckSyncDifference_block` |
| `0x021F` | `"CheckMAUTypeDifference"` | 1437 | `dissect_CheckMAUTypeDifference_block` |
| `0x0224` | `"Adjust PeerToPeerBoundary"` | 1442 | `dissect_AdjustPeerToPeerBoundary_block` |
| `0x0225` | `"Adjust DCPBoundary"` | 1443 | `dissect_AdjustDCPBoundary_block` |
| `0x0226` | `"Adjust PreambleLength"` | 1444 | `dissect_AdjustPreambleLength_block` |
| `0x0227` | `"CheckMAUType-Extension"` | 1445 | `dissect_CheckMAUTypeExtension_block` |
| `0x0229` | `"AdjustMAUType-Extension"` | 1447 | `dissect_AdjustMAUTypeExtension_block` |
| `0x022C` | `"PDPortDataRealExtended"` | 1450 | `dissect_PDPortDataRealExtended_block` |
| `0x0240` | `"PDInterfaceDataReal"` | 1460 | `dissect_PDInterfaceDataReal_block` |
| `0x0250` | `"PDInterfaceAdjust"` | 1462 | `dissect_PDInterfaceAdjust_block` |
| `0x0251` | `"PDPortStatistic"` | 1463 | `dissect_PDPortStatistic_block` |
| `0x0400` | `"MultipleBlockHeader"` | 1497 | `dissect_MultipleBlockHeader_block` |

- "CheckMRPPortData": **not found** under that name. The only MRP check blocks are `0x0213` PDInterfaceMrpDataCheck (interface, not port) and `0x0235` "PDPortMrpIcDataCheck"; no `CheckMRP*` sub-block exists in the table.
- PDRealData / PDExpectedData: **not BlockTypes**; they are record indices (`0xF841` / `0xF842`, 2.9) whose content is a sequence of blocks (MultipleBlockHeader-wrapped).
- All BlockType values guessed in spec §4.3 (`0x020F`, `0x0240`, `0x0400`, `0x0200`, `0x020A`, `0x020C`, `0x0202`, `0x0250`) are confirmed.

### 2.3 PDPortDataReal (`0x020F`), BlockVersion 1.0 only

[PNIO] `dissect_PDPortDataReal_block`, L10082-10213. Offsets assume the block starts 4-aligned.

| Off | Field | Size | Evidence |
|---|---|---|---|
| 0 | BlockHeader (`0x020F`, len, 1, 0) | 6 | — |
| 6 | Padding | 2 | `offset = dissect_pn_align4(…)` right after the version check |
| 8 | SlotNumber | u16 | `hf_pn_io_slot_nr` |
| 10 | SubslotNumber | u16 | `hf_pn_io_subslot_nr` |
| 12 | LengthOwnPortID | u8 | `hf_pn_io_length_own_port_id` |
| 13 | OwnPortID | L bytes, ASCII | `offset += u8LengthOwnPortID;` |
| 13+L | NumberOfPeers | u8 | `hf_pn_io_number_of_peers` (FT_UINT8) |
| | Padding → 4 | 0-3 | `/* Padding */ offset = dissect_pn_align4(…)` |
| | *per peer:* LengthPeerPortID | u8 | |
| | PeerPortID | ASCII | |
| | LengthPeerChassisID | u8 | |
| | PeerChassisID | ASCII | |
| | Padding → 4 | 0-3 | `/* Padding */` |
| | LineDelay | u32 | `dissect_Line_Delay`: bit 31 FormatIndicator (`0x80000000`); 0 → LineDelayValue (mask `0x7FFFFFFF`, ns), 1 → CableDelay value |
| | PeerMACAddress | 6 | `hf_pn_io_peer_macadd` |
| | Padding → 4 | 2 | `/* Padding */` |
| | MAUType | u16 | `hf_pn_io_mau_type` |
| | Padding → 4 | 2 | `/* Padding */` |
| | DomainBoundary | u32 | `hf_pn_io_domain_boundary` FT_UINT32 |
| | MulticastBoundary | u32 | `hf_pn_io_multicast_boundary` FT_UINT32 |
| | LinkState.Port | u8 | `hf_pn_io_link_state_port` (first byte) |
| | LinkState.Link | u8 | `hf_pn_io_link_state_link` (second byte) |
| | Padding → 4 | 2 | `/* Padding */` |
| | MediaType | u32 | `hf_pn_io_media_type` FT_UINT32 |

Value tables ([PNIO] L3538-3572, L3264-3300):

```c
static const value_string pn_io_link_state_port[] = {
    { 0x00, "unknown" }, { 0x01, "disabled/discarding" }, { 0x02, "blocking" },
    { 0x03, "listening" }, { 0x04, "learning" }, { 0x05, "forwarding" }, { 0x06, "broken" },
static const value_string pn_io_link_state_link[] = {
    { 0x00, "reserved" }, { 0x01, "up" }, { 0x02, "down" }, { 0x03, "testing" },
    { 0x04, "unknown" }, { 0x05, "dormant" }, { 0x06, "notpresent" }, { 0x07, "lowerlayerdown" },
static const value_string pn_io_media_type[] = {
    { 0x0000, "Unknown" }, { 0x0001, "Copper cable" }, { 0x0002, "Fiber optic cable" }, { 0x0003, "Radio communication" },
static const value_string pn_io_mau_type[] = {
    { 0x000F, "100BASETXHD" },
    { 0x0010, "100BASETXFD" },
    { 0x001E, "1000BASETFD" },
```

**Differs from spec §4.3:**
- No padding between OwnPortID and NumberOfPeers; the padding comes **after** NumberOfPeers (spec: "OwnPortID (padded to 2)"). There are also 2 padding bytes between the block header and SlotNumber, and per-peer padding before LineDelay and after PeerMACAddress.
- LinkState is two u8 fields, **Port first, Link second**. The up/down values (1 up, 2 down) belong to **LinkState.Link** (second byte = bits 0-7 of a BE u16); the first byte is the bridge port state (0 unknown … 5 forwarding). The spec's bytes `00 01` for "link up" are right, but its names are swapped ("bits 0-7 port state: 1 up, 2 down; bits 8-15 link state"). What the 1515 puts in LinkState.Port for an end-station port (0 unknown vs 5 forwarding) is capture-only.
- **BlockVersion 1.2 does not exist in the dissector**: only 1.0 is decoded; anything else is "Block version x.y not implemented yet!". Use 1.0 (spec's own rule: "the version the dissector decodes without unknown remainder").
- MediaType is u32, `0x00000001` "Copper cable" (matches); MAUType `0x0010` "100BASETXFD" (matches).

### 2.4 PDInterfaceDataReal (`0x0240`), BlockVersion 1.0

[PNIO] `dissect_PDInterfaceDataReal_block`, L12643-12680:

| Off | Field | Size |
|---|---|---|
| 0 | BlockHeader | 6 |
| 6 | LengthOwnStationName (`"LengthOwnStationName"`, u8) | 1 |
| 7 | OwnStationName (ASCII) | L |
| | Padding → 4 | 0-3 |
| | MACAddressValue | 6 |
| | Padding → 4 | 2 |
| | IPAddress | 4 |
| | Subnetmask | 4 |
| | StandardGateway | 4 |

**Differs from spec §4.3:** no padding after the header (the length byte is at offset 6, unlike PDPortDataReal), padding after the name and after the MAC (spec lists the fields without padding). The dissector calls the first field LengthOwnStationName / OwnStationName (spec: LengthOwnChassisID / OwnChassisID — same bytes).

### 2.5 PDPortDataCheck (`0x0200`) and its sub-blocks

PDPortDataCheck ([PNIO] L10012-10045): BlockHeader, Padding (align4 → 2), SlotNumber u16, SubslotNumber u16, then `u16BodyLength -= 6;` and the rest is a list of sub-blocks (each with its own BlockHeader).

| Sub-block | Layout after its 6-byte header | Evidence |
|---|---|---|
| CheckPeers `0x020A` | NumberOfPeers u8; per peer: LengthPeerPortID u8, PeerPortID, LengthPeerChassisID u8, PeerChassisID. **No padding decoded** (none after NumberOfPeers, none at the end) | L10921-10963 |
| Checking MAUType `0x020C` | MAUType u16 (no padding) | L10871-10893 |
| CheckLineDelay `0x020B` | Padding (2), LineDelay u32 (bit 31 format indicator) | L10896-10918 |
| Checking LinkState `0x021C` | PortState u16 (no padding): `pn_io_port_state` 1 up, 2 down, 3 testing, 4 unknown | L10997-11016, L3527 |
| CheckSyncDifference `0x021E` | CheckSyncMode u16 (bit 1 SyncMaster, bit 0 CableDelay) | L10687-10721 |
| CheckMAUTypeDifference `0x021F` | MAUTypeMode u16 (`0x0000` OFF, `0x0001` ON) | L10723-10743 |
| CheckMAUType-Extension `0x0227` | MauTypeExtension u16 | L9951-9966 |

Note: CheckPeers is the block whose PeerPortID string must agree with our LLDP Port ID (spec §4.2). With "port-001" (8) + a chassis name, the block length is generally not a multiple of 4; whether TIA adds trailing padding inside BlockLength is capture-only (the dissector skips it either way).

### 2.6 PDPortDataAdjust (`0x0202`) and its sub-blocks

PDPortDataAdjust ([PNIO] L9971-10007): same frame as PDPortDataCheck (header, Padding 2, SlotNumber, SubslotNumber, sub-blocks).

| Sub-block | Layout after its 6-byte header | Evidence |
|---|---|---|
| AdjustDomainBoundary `0x0209` v1.0 | Padding (2), DomainBoundary u32, AdjustProperties u16, Padding (2) | L10746-10805 |
| AdjustDomainBoundary `0x0209` v1.1 | Padding (2), DomainBoundaryIngress u32, DomainBoundaryEgress u32, AdjustProperties u16, Padding (2) | same |
| AdjustMulticastBoundary `0x0210` | Padding (2), MulticastBoundary u32, AdjustProperties u16 (no trailing padding decoded) | L10808-10836 |
| Adjusting MAUType `0x020E` | Padding (2), MAUType u16, AdjustProperties u16 | L10839-10868 |
| Adjust LinkState `0x021B` | Padding (2), PortState u16 (`pn_io_port_state`), AdjustProperties u16 | L10965-10994 |
| Adjust PeerToPeerBoundary `0x0224` | Padding (2), PeerToPeerBoundary u32, AdjustProperties u16 | L11216-11255 |
| Adjust DCPBoundary `0x0225` | Padding (2), DCPBoundary u32, AdjustProperties u16 | L11258-11294 |
| Adjust PreambleLength `0x0226` | Padding (2), PreambleLength u16 (`0x0000` "Seven octets Preamble shall be used", `0x0001` "One octet Preamble shall be used"), AdjustProperties u16 | L9923-9946 |

Boundary bit meanings ([PNIO] L3322-3345, hf masks L22500-22538):

```c
pn_io_peer_to_peer_boundary_value_bit0: { 0x01, "The LLDP agent shall not send LLDP frames (egress filter)." }   /* mask 0x1 */
pn_io_peer_to_peer_boundary_value_bit1: { 0x01, "The PTCP ASE shall not send PTCP_DELAY request frames (egress filter)." } /* 0x2 */
pn_io_peer_to_peer_boundary_value_bit2: { 0x01, "The Time ASE shall not send PATH_DELAY request frames (egress filter)." }  /* 0x4 */
pn_io_dcp_boundary_value_bit0: { 0x01, "Block an outgoing DCP_Identify frame (egress filter) with the multicast MAC address 01-0E-CF-00-00-00" } /* 0x1 */
pn_io_dcp_boundary_value_bit1: { 0x01, "Block an outgoing DCP_Hello frame (egress filter) with the multicast MAC address 01-0E-CF-00-00-01" }  /* 0x2 */
```

AdjustPeerToPeerBoundary bit 0 = 1 means "do not transmit LLDP on this port". The spec stores Adjust blocks without behaviour (§2 Out); if TIA writes bit 0 = 1, a conforming device stops sending LLDP. Flag for the capture: check the bit in the CPU's `0x802F` write.

### 2.7 PDInterfaceAdjust (`0x0250`) — and a Port ID hint

[PNIO] L11428-11447 and L1987-1990, hf mask L21794-21797:

```c
    /* Padding */
    offset = dissect_pn_align4(tvb, offset, pinfo, tree);
    /* MultipleInterfaceMode */  … hf_pn_io_MultipleInterfaceMode_NameOfDevice  (FT_UINT32, mask 0x01)
static const value_string pn_io_MultipleInterfaceMode_NameOfDevice[] = {
    { 0x00000000, "PortID of LLDP contains name of port (Default)" },
    { 0x00000001, "PortID of LLDP contains name of port and NameOfStation" },
```

Layout: header, Padding (2), MultipleInterfaceMode u32 (bit 0 NameOfDevice, other bits reserved).

[GOLD] frame 54 (the CPU's MultipleWrite in our existing Connect golden) contains
`02 50 00 08 01 00 | 00 00 | 00 00 00 01` → **MultipleInterfaceMode.NameOfDevice = 1: the 1515 instructs the device that its LLDP PortID "contains name of port and NameOfStation"**. This decides the *form* (`<port>.<NameOfStation>`) for that engineering; the exact port-name string is still capture-only (3.5).

### 2.8 MultipleBlockHeader (`0x0400`)

[PNIO] `dissect_MultipleBlockHeader_block`, L16832-16862:

```c
    offset = dissect_pn_align4(tvb, offset, pinfo, tree);
    offset = dissect_dcerpc_uint32(…, hf_pn_io_api, &u32Api);
    offset = dissect_dcerpc_uint16(…, hf_pn_io_slot_nr, &u16SlotNr);
    offset = dissect_dcerpc_uint16(…, hf_pn_io_subslot_nr, &u16SubslotNr);
    new_tvb = tvb_new_subset_length(tvb, offset, u16BodyLength-10);
    offset = dissect_blocks(new_tvb, 0, pinfo, tree, drep);
```

Layout: header (BlockVersion 1.0), Padding (2), API u32, SlotNumber u16, SubslotNumber u16, then the nested blocks. BlockLength = 2 (version) + 2 (padding) + 8 + length of the nested blocks. Matches spec §4.3 (API, SlotNumber, SubslotNumber) plus the 2 padding bytes the spec does not list.

### 2.9 Record indices

[PNIO] `pn_io_index[]` (L2149ff):

| Index | Dissector string | Line | Read decoding |
|---|---|---|---|
| `0x802A` | `"PDPortDataReal for one subslot"` | 2175 | multiple blocks (`case(0x802a)`, L19055) |
| `0x802B` | `"PDPortDataCheck for one subslot"` | 2176 | multiple blocks |
| `0x802F` | `"PDPortDataAdjust for one subslot"` | 2180 | multiple blocks |
| `0x8071` | `"PDInterfaceAdjust for one subslot"` | 2228 | single block (the read-switch comment at L18955 wrongly says "PDPortStatistic"; `0x8072` is PDPortStatistic, L2229) |
| `0x8080` | `"PDInterfaceDataReal"` | 2231 | single block |
| `0xF841` | `"PDRealData"` | 2416 | `dissect_blocks` (L19113-19116) |
| `0xF842` | `"PDExpectedData"` | 2417 | `dissect_blocks` |

For `0xF841` / `0xF842` the dissector accepts any block sequence; the MultipleBlockHeader wrapping and the order of the inner blocks are capture-only (spec §4.3 already says so).

### 2.10 Diagnosis: ChannelErrorType / ExtChannelErrorType

Diagnosis item layout ([PNIO] `dissect_Diagnosis`, L7066ff): ChannelNumber u16, ChannelProperties u16, ChannelErrorType u16; for USI `0x8000` stop there; otherwise ExtChannelErrorType u16, then ExtChannelAddValue u32 (`hf_pn_io_ext_channel_add_value`, FT_UINT32). USI `0x8002` = `"ExtChannelDiagnosis"` (L2452).

ChannelErrorType ([PNIO] `pn_io_channel_error_type`, L2515-2520):

```c
    { 0x8000, "Data transmission impossible" },
    { 0x8001, "Remote mismatch" },
    { 0x8002, "Media redundancy mismatch - Ring" },
    { 0x8003, "Sync mismatch" },
    { 0x8004, "IsochronousMode mismatch" },
    { 0x8005, "Multicast CR mismatch" },
```

ExtChannelErrorType for ChannelErrorType `0x8000` "Data transmission impossible" ([PNIO] `pn_io_ext_channel_error_type0x8000`, L2913-2922):

| Ext | Name |
|---|---|
| `0x8000` | "Link State mismatch - Link down" |
| `0x8001` | "MAUType mismatch" |
| `0x8002` | "Line Delay mismatch" |
| `0x8003..0x8FFF` | Reserved |

ExtChannelErrorType for ChannelErrorType `0x8001` "Remote mismatch" ([PNIO] `pn_io_ext_channel_error_type0x8001`, L2926-2943):

| Ext | Name |
|---|---|
| `0x8000` | "Peer name of station mismatch" |
| `0x8001` | "Peer name of port mismatch" |
| `0x8002` | "Peer RT_CLASS_3 mismatch a" |
| `0x8003` | "Peer MAUType mismatch" |
| `0x8004` | "Peer MRP domain mismatch" |
| `0x8005` | "No peer detected" |
| `0x8006` | "Reserved" |
| `0x8007` | "Peer Line Delay mismatch" |
| `0x8008` | "Peer PTCP mismatch b" |
| `0x8009` | "Peer Preamble Length mismatch" |
| `0x800A` | "Peer Fragmentation mismatch" |
| `0x800B` | "Peer MRP Interconnection domain mismatch" |
| `0x800C..0x8FFF` | Reserved |

Dispatch ([PNIO] L7093-7101): `else if (u16ChannelErrorType == 0x8000) … hf_pn_io_ext_channel_error_type0x8000` / `else if (u16ChannelErrorType == 0x8001) … hf_pn_io_ext_channel_error_type0x8001`.

**Differs from spec §4.4:** the peer-mismatch ExtChannelErrorTypes (`0x8000` peer name of station, `0x8001` peer name of port, `0x8005` no peer detected) are defined under **ChannelErrorType `0x8001` "Remote mismatch"**, not `0x8000` "Data transmission impossible". Under `0x8000` the same Ext values mean "Link State mismatch - Link down" / "MAUType mismatch" / reserved. The remote-mismatch diagnosis should therefore be ChannelErrorType **`0x8001`** with Ext `0x8000` / `0x8001` / `0x8005` (Ext values themselves match the spec). A CheckMAUType failure is Ext `0x8001` under ChannelErrorType `0x8000` (local) or Ext `0x8003` under `0x8001` (peer). The TIA diagnostic-buffer text for each is capture-only.

## 3. LLDP

### 3.1 Framing and basic TLVs

- TLV header: `#define TLV_TYPE_MASK 0xFE00`, `#define TLV_INFO_LEN_MASK 0x01FF` ([LLDP] L599-602): 7-bit type, 9-bit length. Types: End `0x00`, Chassis ID 1, Port ID 2, TTL 3, Organization Specific `0x7F` (L587-614).
- Chassis ID subtypes ([LLDP] L618-628): `{ 4, "MAC address"}`, **`{ 7, "Locally assigned"}`** (L626).
- Port ID subtypes ([LLDP] L638-648): `{ 3, "MAC address"}`, `{ 5, "Interface name"}`, **`{ 7, "Locally assigned"}`** (L646).
- **Shutdown PDU** ([LLDP] L5151-5156): "Shutdown PDU: Verify that only ChassisID, PortID, TTL and optionally END TLVs are present" — any other TLV raises the expert info `"TLV type 0x%02X not allowed in Shutdown PDU"`. **Note for spec §4.2:** the TTL-0 PDU on `stop()` must carry only Chassis ID, Port ID, TTL = 0 and End (no PROFINET / 802.3 TLVs), otherwise the HIL dissector check flags it.

### 3.2 PROFINET organisationally specific TLVs (OUI `00-0E-CF`)

`#define OUI_PROFINET        0x000ECF    /* PROFIBUS Nutzerorganisation e.V. */` ([OUI] L42).
Subtypes ([LLDP] `profinet_subtypes`, L810-826):

| Sub | Name | Decoded layout (`dissect_profinet_tlv`, L3949ff) |
|---|---|---|
| 1 | "Measured Delay Values" | RxDelayLocal u32, RxDelayRemote u32, TxDelayLocal u32, TxDelayRemote u32, CableDelayLocal u32 (value 0 shown as "(unknown)", else ns) — `case 1: /* LLDP_PNIO_DELAY */` L3975 |
| 2 | "Port Status" | RTClass2 PortStatus u16, RTClass3 PortStatus u16 — `case 2: /* LLDP_PNIO_PORTSTATUS */` L4019 |
| 3 | "Alias" | not decoded: `/*case 3:*/ /* XXX - LLDP_PNIO_ALIAS */` L4032 |
| 4 | "MRP Port Status" | DomainUUID 16, MRRT PortStatus u16 |
| 5 | "Chassis MAC" | 6 bytes, `hf_profinet_cm_mac` "CMMacAdd" (blurb "CMResponderMacAdd or CMInitiatorMacAdd") — `case 5: /* LLDP_PNIO_CHASSIS_MAC */` L4044 |
| 6 | "PTCP Status" | MasterSourceAddress 6, SubdomainUUID 16, IRDataUUID 16, LengthOfPeriod u32, Red/Orange/GreenPeriodBegin u32 (bit 31 valid) |
| 7 | "MauType Extension" | named, not decoded |
| 8 | "MRPIC Port Status" | DomainID u16, Role u16, MICPosition u16 |
| 9-12 | NME domain identification / Management Address / Name UUID / Parameter UUID | — |
| 13-14 | AS Working Clock / AS Global Time | — |

Port Status encodings ([LLDP] L1410-1432, hf L6381-6401):

```c
profinet_port2_status_vals: { 0, "OFF" }, { 1, "SYNCDATA_LOADED" }, { 2, "RTCLASS2_UP" }, { 3, "Reserved" }   /* whole u16 */
profinet_port3_status_vals: { 0, "OFF" }, { 1, "reserved" }, { 2, "RTCLASS3_UP" }, { 3, "RTCLASS3_DOWN" }, { 4, "RTCLASS3_RUN" }  /* mask 0x07 */
RTClass3_PortStatus.reserved       mask 0x0FF8
RTClass3_PortStatus.Fragmentation  mask 0x1000 (on/off)
RTClass3_PortStatus.PreambleLength mask 0x2000 (0 "Seven octets", 1 "One octet")
```

Spec §4.2 (subtype 1 = 5 × u32 all 0, subtype 2 = RTClass2 0 + RTClass3 0, subtype 5 = 6-byte MAC) **matches**. RTClass3 `0x0000` = OFF, Fragmentation off, PreambleLength seven octets.

### 3.3 IEEE 802.3 MAC/PHY Configuration/Status (OUI `00-12-0F`, subtype 1)

`#define OUI_IEEE_802_3      0x00120F    /* IEEE 802.3 */` ([OUI] L48); `{ 0x01, "MAC/PHY Configuration/Status" }` ([LLDP] L759-760).

Layout (`dissect_ieee_802_3_tlv`, L3190-3265): subtype u8 = 1, Auto-Negotiation Support/Status u8, PMD Auto-Negotiation Advertised Capability u16, Operational MAU Type u16.

- Autoneg byte ([LLDP] hf L5765-5772): bit `0x01` = supported, bit `0x02` = enabled → `0x03` = supported + enabled (matches spec).
- PMD advertised capability, standard bit order ([LLDP] L1353-1368; comments give the IEEE `ifMauAutoNegCapAdvertisedBits` bit numbers):

```c
#define AUTONEG_OTHER          0x8000 /* bOther(0) */
#define AUTONEG_10BASE_T       0x4000 /* b10baseT(1),      -- 10BASE-T  half duplex mode */
#define AUTONEG_10BASET_FD     0x2000 /* b10baseTFD(2),    -- 10BASE-T  full duplex mode */
#define AUTONEG_100BASE_T4     0x1000 /* b100baseT4(3) */
#define AUTONEG_100BASE_TX     0x0800 /* b100baseTX(4),    -- 100BASE-TX half duplex mode */
#define AUTONEG_100BASE_TXFD   0x0400 /* b100baseTXFD(5),  -- 100BASE-TX full duplex mode */
#define AUTONEG_100BASE_T2     0x0200 /* b100baseT2(6) */
#define AUTONEG_100BASE_T2FD   0x0100 /* b100baseT2FD(7) */
#define AUTONEG_FDX_PAUSE      0x0080 /* bFdxPause(8) */
#define AUTONEG_FDX_APAUSE     0x0040 /* bFdxAPause(9) */
#define AUTONEG_FDX_SPAUSE     0x0020 /* bFdxSPause(10) */
#define AUTONEG_FDX_BPAUSE     0x0010 /* bFdxBPause(11) */
#define AUTONEG_1000BASE_X     0x0008 /* b1000baseX(12) */
#define AUTONEG_1000BASE_XFD   0x0004 /* b1000baseXFD(13) */
#define AUTONEG_1000BASE_T     0x0002 /* b1000baseT(14),   -- 1000BASE-T half duplex mode */
#define AUTONEG_1000BASE_TFD   0x0001 /* b1000baseTFD(15)  -- 1000BASE-T full duplex mode */
```

  The dissector also shows the same u16 decoded with the bit order reversed (`INV_AUTONEG_*`, L1376-1391) under the subtree "Same in inverse (wrong) bitorder" (L3243), i.e. it knows some devices send it reversed.
  **10/100 HD/FD advertisement = `0x4000 | 0x2000 | 0x0800 | 0x0400` = `0x6C00`** (inverse order would be `0x0036`).
- Operational MAU type ([LLDP] `operational_mau_type_values`, L1141ff): `{ 15, "100BaseTXHD - …half duplex mode" }` (L1159), **`{ 16, "100BaseTXFD - 2 pair category 5 UTP, full duplex mode" }`** (L1160), `{ 30, "1000BaseTFD - Four-pair Category 5 UTP, full duplex mode" }` (L1175). So 100BASE-TX FD = `0x0010` (matches), HD = `0x000F`, 1000BASE-T FD = `0x001E`.

**Differs from spec §4.2:** `0x6c01` = `0x6C00` + `AUTONEG_1000BASE_TFD` — it advertises 1000BASE-T full duplex, which a 100 Mbit/s port does not have. The dissector-derived value for "10/100 HD/FD" is `0x6C00`; whether the 1515's X1 advertises pause bits or `bOther` is capture-only, and the device should derive the field from the Linux interface's advertised modes rather than a constant.

### 3.4 Chassis ID / Port ID for PROFINET

- Chassis ID subtype 7 = "Locally assigned", Port ID subtype 7 = "Locally assigned" (3.1). Both strings are stored for the PROFINET column (`chassis_id_locally_assigned`, L1884; `port_id_locally_assigned`, L2063).
- The dissector's PROFINET column mode ([LLDP] `set_name_of_station_for_profinet_specialized_column_info`, L3897-3933) splits a locally-assigned Port ID on `"."`:

```c
	const char *delimForProfinetv23 = ".";
	…
				tokenPortId = strtok(lldpPortIdCombinedWithNameOfStation, delimForProfinetv23);
				tokenNameOfStation = strtok(NULL, delimForProfinetv23);
				col_append_fstr(pinfo->cinfo, COL_INFO, "NoS = %s ", tokenNameOfStation);
				col_append_fstr(pinfo->cinfo, COL_INFO, "Port Id = %s ", tokenPortId);
```

  This is only applied when no NameOfStation was already taken from a locally-assigned Chassis ID (`if (pn_lldp_column_info->is_nos_assigned != true)`). Hint: the PROFINET "V2.3" combined form is `<portname>.<NameOfStation>`; the dissector says nothing about the port-name text itself (no `port-001` literal anywhere in the file). Note `strtok` splits on the first dot, so a dotted NameOfStation would be truncated in the column (display only).
- Combined with 2.7 ([GOLD] MultipleInterfaceMode.NameOfDevice = 1 from the 1515), the expected Port ID form is `<port>.<NameOfStation>`; the `<port>` literal (`port-001`?) and the Chassis ID content when the combined form is used are **capture only**.

## 4. Capture campaign — pending

## 5. Decoded goldens — pending

## 6. Spec §4 (verify) items — resolution table

| Spec item | Provisional value | Evidence | Status |
|---|---|---|---|
| ResetToFactory qualifier encoding (§4.1) | Mode = `BlockQualifier >> 1`; mode 2 → `0x0004`, mode 4 → `0x0008` (mode 8 → `0x0010`). Matches spec. Mode 4 text is "Reset all stored data except the security configuration"; which mode TIA sends with / without "retain I&M data" unknown | [DCP] L213-226, L1477 (`BlockQualifier >> 1`), L1892 (mask `0xFFFE`) | provisional (encoding pinned; TIA's mode choice capture-only) |
| PMD auto-negotiation advertised (§4.2) | `0x6C00` (10BASE-T HD+FD, 100BASE-TX HD+FD, IEEE bit order); **spec's `0x6c01` adds 1000BASE-T FD** | [LLDP] L1353-1368 (`AUTONEG_*`), L3222-3243 | provisional |
| Autoneg support/status; operational MAU (§4.2) | `0x03` (bit0 supported, bit1 enabled); MAU `0x0010` 100BaseTXFD | [LLDP] L5765-5772, L1160 | provisional (matches spec) |
| PDPortDataReal field order / BlockVersion (§4.3) | BlockVersion **1.0 only**; header, pad 2, Slot, Subslot, LenOwnPortID, OwnPortID, NumberOfPeers, pad→4, [LenPeerPortID, PeerPortID, LenPeerChassisID, PeerChassisID, pad→4, LineDelay u32, PeerMAC 6, pad→4], MAUType u16, pad 2, DomainBoundary u32, MulticastBoundary u32, LinkState.Port u8, LinkState.Link u8, pad 2, MediaType u32 | [PNIO] L10082-10213, [PN] L1506-1519 | provisional |
| LinkState naming (§4.3) | Byte 1 = LinkState.Port (0 unknown … 5 forwarding), byte 2 = LinkState.Link (1 up, 2 down); spec swaps the names, bytes `00 01` unchanged | [PNIO] L3538-3561, L10184-10190 | provisional (Port value capture-only) |
| PDInterfaceDataReal layout (§4.3) | header, LenOwnStationName u8 (no pad before), name, pad→4, MAC 6, pad→4, IP, mask, gateway | [PNIO] L12643-12680 | provisional |
| ExtChannelErrorType values (§4.4) | **ChannelErrorType `0x8001` "Remote mismatch"** (not `0x8000`); Ext `0x8000` "Peer name of station mismatch", `0x8001` "Peer name of port mismatch", `0x8005` "No peer detected"; ExtChannelAddValue u32 | [PNIO] L2515-2516, L2926-2943, L7093-7101 | provisional (TIA buffer text capture-only) |
| ResetToFactoryModes XSD separator | `base:ValueListT`, pattern `((\d+\.\.\d+)\|(\d+))(( \d+\.\.\d+)\|( \d+))*` → **space-separated** values or `a..b` ranges, e.g. `ResetToFactoryModes="2 4"` | [XSD] DeviceProfile-v2.31 L424 `<xsd:attribute name="ResetToFactoryModes" type="base:ValueListT" />`; Primitives-v2.31 L72-79 | pinned from the TIA-shipped XSD (GSD checker still to confirm) |
| CertificationInfo position in DAP | Child element of `DeviceAccessPointItem`, sequence `ModuleInfo` → **`CertificationInfo` (minOccurs 0)** → `SubslotList` → `IOConfigData` → `UseableModules` → `ARVendorBlock` … | [XSD] DeviceProfile-v2.31 L268-276 (`DeviceAccessPointItemT`: `<xsd:element name="ModuleInfo" …/>` then `<xsd:element name="CertificationInfo" type="gsdml:CertificationInfoT" minOccurs="0" />`) | pinned from XSD |
| CertificationInfo enumerations | `ConformanceClass` required, pattern `A\|B\|C`; `ApplicationClass` required, `base:TokenListT` = pattern `([0-9a-zA-Z_]+;)*([0-9a-zA-Z_]+)?` (semicolon-separated tokens, **no enumeration in the XSD**, empty allowed); `NetloadClass` optional, pattern `I\|II\|III`; optional child `ProfileList/ProfileItem(@ProfileName, @Class)` | [XSD] DeviceProfile-v2.31 L443-470; Primitives-v2.31 L91-98. In v2.4 `ApplicationClass` is optional on `CertificationInfo` and a `CertificationInfoExt` child appears (DeviceProfile-v2.4 L298-302, L506-520) | pinned from XSD (allowed ApplicationClass tokens: not in XSD, checker/spec text needed) |
| LLDP Port ID format (§4.2) | Form `<port>.<NameOfStation>` indicated by the CPU's PDInterfaceAdjust (MultipleInterfaceMode.NameOfDevice = 1); the dissector parses `<port>.<NoS>` on `"."` but gives no port-name literal | [PNIO] L1987-1990; [GOLD] write_req.hex `02 50 00 08 01 00 00 00 00 00 00 01`; [LLDP] L3900-3920 | **capture only** |
| Shutdown LLDPDU content (§4.2) | Chassis ID, Port ID, TTL = 0, End only | [LLDP] L5151-5156 | provisional (dissector rule) |
| CheckPeers / short sub-block padding (§4.3) | Dissector decodes no padding in CheckPeers; TIA's trailing padding inside BlockLength unknown | [PNIO] L10921-10963, L18455 | capture only |
| AdjustPeerToPeerBoundary bit 0 (§4.3, "no behaviour") | bit 0 = 1 means "LLDP agent shall not send LLDP frames" on that port | [PNIO] L3334-3337, mask L22520-22523 | provisional (whether TIA sets it: capture only) |

**Not found:** a block named "CheckMRPPortData" (searched `pn_io_block_type[]` and the `dissect_block` switch; only `0x0213` PDInterfaceMrpDataCheck and `0x0235` PDPortMrpIcDataCheck exist); a decoder for LLDP PROFINET subtypes 3 (Alias) and 7 (MauType Extension) (named, not dissected); any `port-001` literal or Siemens-specific Port ID format in `packet-lldp.c`; BlockVersion 1.1/1.2 branches for PDPortDataReal (only 1.0); `packet-pn-io.c` (the file is `packet-dcerpc-pn-io.c`).

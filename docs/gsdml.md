# GSDML — generation, layout, import, validation

`pnio`'s GSDML is not hand-written: `gsdml::render(&cfg, &meta)` (`crates/pnio/src/gsdml.rs`)
renders it straight from the same [`DeviceConfig`](../crates/pnio/src/config.rs) that also
derives the device's `cm::DeviceModel`, so the file cannot drift from what the device answers on
the wire — same idents, same field order, same lengths, by construction. This page documents
what the generator emits, how a declared field maps to a controller address, how to import and
validate the result in TIA Portal, and what is deliberately not declared yet.

## What the generator emits

One `DeviceAccessPointItem` ("DAP1") plus one `ModuleItem` per declared slot:

- **DAP** (slot 0): `ModuleIdentNumber="0x00000001"`, `PNIO_Version="V2.3"` (see
  [Validation](#validation) for why), `MinDeviceInterval` from `cfg.min_device_interval()`,
  `DNS_CompatibleName` = the station name, plus four more `DeviceAccessPointItem` attributes:
  two the v2.4 XSD actually requires — `CheckDeviceID_Allowed="true"` and
  `NameOfStationNotTransferable="false"` (TIA's XSD validation failed without them, the HIL
  bench's first rejection, `docs/bench-pnet-device.md` §6g) — and two more that are not
  schema-mandated but are declared to match the rt-labs reference file TIA accepted,
  `MultipleWriteSupported="true"` and `DeviceAccessSupported="false"`. `IOConfigData` carries
  `MaxInputLength`/`MaxOutputLength`/`MaxDataLength` — see [Validation](#validation), these are
  not the plain per-direction data sums. The interface submodule (subslot `32768`) and port
  submodule (subslot `32769`) are system-defined, fixed.
- **One module per slot, pinned to that slot**: slot *n* → `ModuleItem ID="M<n>"
  ModuleIdentNumber="0x100+n"`, referenced from the DAP's `UseableModules` as
  `ModuleItemRef ModuleItemTarget="M<n>" AllowedInSlots="<n>"` — module *n* is allowed in slot
  *n* only, so TIA's hardware catalog has no ambiguous choice to make when a module is dropped
  onto a slot.
- **DataItem encoding**: each field in a slot's `inputs`/`outputs` list becomes one entry in
  declaration order. A non-`Bool` field is one scalar `DataItem` (`Float32` for `Real`,
  `Integer16` for `Int`, `Unsigned16` for `Word`, `Integer32` for `Dint`). A run of up to 8
  `Bool` fields sharing the same byte (as [the layout rule](#the-layout-rule) computes it) is
  rendered as a single `DataItem DataType="Unsigned8" UseAsBits="true"` carrying one
  `BitDataItem BitOffset="i"` child per bit, `i` = the bit's `FieldRef.bit`.
- **Texts**: every `TextId` referenced anywhere (`InfoText`, module names/info, one text per
  scalar `DataItem`, one per `Bool`-group `DataItem`, one per `BitDataItem`) is defined in
  `ExternalTextList/PrimaryLanguage`. Text fields are XML-escaped (`& < > " '`).

Nothing else is emitted: no alarms, no diagnosis, no I&M, no `IsochroneMode`, no parameter
records — see [What is not declared yet](#what-is-not-declared-yet).

## The layout rule

`config::layout(fields: &[FieldType]) -> (Vec<FieldRef>, u16)` walks a slot's field list in
declaration order and assigns each field a byte offset (and, for `Bool`, a bit): `Bool` fields
pack 8 per byte, LSB-first; a `Bool` right after a non-`Bool` field opens a new byte; a
non-`Bool` field always closes whatever bit-byte is open, lands at the next free byte, and
advances the cursor by its size (`Int`/`Word` 2 bytes, `Dint`/`Real` 4 bytes, big-endian,
no padding).

Worked example, `[Real, Bool, Bool, Int, Bool]`:

| Field | Byte | Bit |
|---|---|---|
| `Real` | 0 | — |
| `Bool` | 4 | 0 |
| `Bool` | 4 | 1 |
| `Int` | 5 | — |
| `Bool` | 7 | 0 |

Total: **8 bytes**. The `Real` takes bytes 0-3; the two `Bool`s that follow open byte 4 (bits 0
and 1); the `Int` closes that bit-byte and takes bytes 5-6; the last `Bool` can't reuse byte 4
(a non-`Bool` came between), so it opens a fresh byte 7.

A declared field maps to its TIA address as **module base + `FieldRef.byte`**, with
**bit = `FieldRef.bit`** (0 for anything but `Bool`) — where the module base is the byte offset
TIA assigns the slot's module within `%I`/`%Q`, i.e. the sum of the input (resp. output) lengths
of every lower-numbered slot (TIA packs modules in slot order starting at 0). `DeviceConfig`
exposes each slot's own byte length via `input_len(slot)`/`output_len(slot)`, so a caller can
compute the same running offset `gen_gsdml` prints (see below) without re-deriving the layout by
hand.

## Declaration → TIA address map for the sample config

`examples/gen_gsdml.rs` declares the sample device (station `pnio-dev`, identity `0xFFFF`/
`0x0001`) — slot 1: 16 `Real` in, slot 2: 32 `Bool` in, slot 3: 16 `Real` out, slot 4: 32 `Bool`
out — and prints the resulting address map:

```
slot  dir     bytes  fields
1     Input   64     16 fields -> %IB0..63
2     Input   4      32 fields -> %IB64..67
3     Output  64     16 fields -> %QB0..63
4     Output  4      32 fields -> %QB64..67
(controller addresses assume TIA packs the modules in slot order from 0; check the device view)
```

These are exactly the addresses TIA assigned when this GSDML was imported and plugged (HIL,
2026-08-29): input slots at `%I0..63`/`%I64..67`, output slots at `%Q0..63`/`%Q64..67` — computed
and observed addresses matched. **The device view in TIA is authoritative**: it is what
`gen_gsdml`'s comment says to check, because a project that plugs the modules in a different
slot order, or leaves a slot unplugged, shifts every address after it — the printed map is a
convenience for the common case (every declared slot plugged in slot order), not a substitute
for reading the actual device view.

## Importing in TIA

1. Generate the file: `cargo run --example gen_gsdml -- --station <name> --vendor-id <id>
   --device-id <id> --interval 32|16 --out <dir>`. It writes
   `GSDML-V2.4-<Vendor>-<Product>-<YYYYMMDD>.xml` and prints the address map above.
2. **Options → Manage GSD files**, point at the directory holding the file, select it,
   **Install**.
3. Hardware catalog: *Other field devices → PROFINET IO → Core Engineering → pnio* (vendor/
   product family from `GsdmlMeta`) → drag onto the controller's PROFINET IO system.
4. Set the device's PROFINET name to the declared station name and its IP address.
5. Plug modules: because each `ModuleItem` is `AllowedInSlots="<its own slot number>"`, there is
   exactly one legal module per slot — **modules land in their declared slots**, no ordering
   choice to get wrong.
6. Set the interface's update (send-clock × reduction ratio) time to match
   `min_device_interval` — 1 ms for `32`, 500 µs for `16` (see
   [`min_device_interval`](#min_device_interval) below).
7. Compile and download.

**Uninstall/reinstall quirk**: if the declaration changes (fields, slots, identity, …) but the
file name stays the same — same station/vendor/product-family/date, so `file_name()` produces
the same string — TIA does not pick up the new content on a plain re-import over the old one.
Uninstall the old GSD first (**Manage GSD files** → select it → **Uninstall**), then install the
regenerated file. Observed 2026-08-29 during the Plan 6 HIL bring-up.

## Validation

**XSD recipe** (works with TIA Portal V21, which ships the PI schema locally): from WSL,
```bash
python3 -c "from lxml import etree; x=etree.XMLSchema(etree.parse('/mnt/c/Program Files/Siemens/Automation/Portal V21/Data/Hwcn/Custom/Interpreter/GSD/XSD/GSDML-DeviceProfile-v2.4.xsd')); print(x.validate(etree.parse('FILE')), x.error_log)"
```
(needs `GSDML-Primitives-v2.4.xsd` alongside it, same directory). The XSD itself is **not
vendored in this repo** — its licence has not been checked — but it ships with TIA Portal, so
the recipe above works on any machine with TIA V21 installed. XSD validation is **necessary but
not sufficient**: TIA's own GSD checker applies additional, version-dependent rules on top of
schema conformance, so a file that validates cleanly against the XSD can still be rejected by
TIA at import or compile time. Three checker/schema rules shaped this crate's rendering:

- **`PNIO_Version="V2.3"`, not `"V2.4"`**. TIA's GSD checker (beyond the XSD) rejects a DAP
  declaring `PNIO_Version="V2.4"` unless it also claims features this device does not
  implement: for `PNIO_Version >= "V2.31"` it mandates `CertificationInfo` at the DAP,
  `LLDP_NoD_Supported="true"`, `ResetToFactoryModes="2"`, `PTP_BoundarySupported="true"` and
  `DCP_BoundarySupported="true"` (TIA V21's checker, rule codes
  `0x00020020_0/5/6/10/11`) — none of which this device implements (no LLDP, no PTP/DCP
  boundary, no ResetToFactory). `V2.3` is the last profile version without those mandates and
  still allows `StartupMode="Advanced"`.
- **`SupportedProtocols=""`**. This one answers an XSD validation failure, not a TIA popup: the
  v2.4 XSD marks the attribute `use="required"` on `InterfaceSubmoduleItem`, but its type
  (`base:TokenListT`, pattern `(([0-9a-zA-Z_]+;)*[0-9a-zA-Z_]+)?`) allows an empty token list —
  so an empty value is the honest declaration (the device implements neither SNMP nor LLDP) and
  passes the recipe above; omitting the attribute entirely fails XSD validation before TIA is
  even involved.
- **`IOConfigData`'s `MaxInputLength`/`MaxOutputLength` count the IOPS/IOCS bytes**
  (`DeviceConfig::input_cr_len`/`output_cr_len`, `75`/`75` for the sample config above, not the
  plain data sum `64 + 4 = 68`). Declaring the plain sum answers this TIA compile-time message:
  *"The amount of input data (including user data qualifier) of 75 bytes exceeds the maximum
  permitted data amount of 68 bytes"* (same wording for output). TIA counts the Input/Output CR
  C-SDU length exactly as `rt::Layout` builds it — 3 bytes of DAP IOPS/IOCS, plus one
  `(data_len + 1)` per submodule with data in that direction, plus one IOCS byte per submodule
  with data only in the other direction — not the sum of each module's own `IOData` length.
  `MaxDataLength` (present, optional in the v2.4 XSD) is their sum.

## Identity caveat

The default identity — `vendor_id = 0xFFFF`, `device_id = 0x0001` — is a **development value,
not a PI-assigned identity**. TIA accepts it in a project, so it is convenient for bring-up, but
it must never appear in a real deployment: replace it via `.identity(vendor_id, device_id)` on
the builder, or `--vendor-id`/`--device-id` on `gen_gsdml`, with a Vendor ID actually assigned by
PI before leaving the lab.

## `min_device_interval`

`DeviceConfigBuilder::min_device_interval` is the AR's cyclic update time, in units of
31.25 µs: `16` = 500 µs, `32` = 1 ms (the default). No other value is accepted — `8` would need a
busy-poll device this crate doesn't implement, and `64`/`128` are not send clocks it tests or
declares. This value only affects the **GSDML** (`MinDeviceInterval` and `TimingProperties`'
`SendClock`); the actual update time is whatever TIA's interface properties are set to (it must
be one the GSDML declares as supported — set it explicitly during import, see
[Importing in TIA](#importing-in-tia)). `gen_gsdml --interval 16` renders the 500 µs variant.

**A GSDML declaring 500 µs does not guarantee TIA will offer it.** The controller's own
interface can cap the achievable send clock below what the GSDML declares: on the HIL bench
(`docs/bench-pnet-device.md` §6g), a 1515-2 PN's **X2** port — the device-facing segment — is
RT-only with a fixed 1 ms send clock; 250/500 µs and IRT are only available on **X1**. TIA
accepted `MinDeviceInterval="16"` in the file but its update-time list stopped at 1 ms anyway,
because the ceiling is the physical port, not the GSDML. With the device cable moved to X1 (same
IP, X1 send clock 0.5 ms) the same file, binary and edge ran at 500 µs for 5 minutes with zero
missed ticks (`docs/bench-pnet-device.md` §6g).

## Using the device

```rust
let cfg = DeviceConfig::builder("pnio-dev")
    .input(Slot(1), &[FieldType::Real; 16])
    .output(Slot(3), &[FieldType::Real; 16])
    .build()?;

let dev = IoDevice::start(cfg, StartOptions { iface, ip, rt, app_cpus })?;
while !dev.ready() {
    std::thread::sleep(Duration::from_millis(5));
}
let out = dev.outputs(Slot(3))?;         // consistent snapshot + Validity
dev.with_inputs(Slot(1), |w| w.real(0, out.real(0)?))?;
```

`DeviceConfig::builder(..).build()` validates the declaration and derives everything else
(`model()`, the field table, DCP properties, `setup()`); `IoDevice::start` opens the real
interface, starts the acyclic thread and (if `rt` is set) the RT thread, and returns a handle a
caller can share (`IoDevice` is `Send + Sync`).

Before the AR reaches `Data` — and for a few microseconds after it first does, until the RT
runner actually rebuilds the I/O image — every read/write returns `ApiError::NoLayoutYet`
instead of a stale or zeroed value. `IoDevice::ready()` polls both conditions (`ar_state() ==
Data` *and* the image non-empty) and is the right thing to wait on, not `ar_state()` alone.

`with_inputs`'s closure runs on a scratch copy of the slot's working buffer: on success the
whole slot publishes in one image write (one consistent frame for however many fields the
closure set); on an `Err` — or a panicking unwind — nothing is published and the working copy is
left exactly as it was. The closure must **not** call back into the same `IoDevice` for the same
slot (another `with_inputs`/`write_*` on it): the per-slot lock it holds is not reentrant.

Values written while the AR is down are not lost: the working copy is updated before the publish
to the image is attempted, so a `with_inputs`/`write_*` call that succeeds on the working copy
but then sees the image reject it with `NoLayoutYet` keeps the value — it is published whole by
the first call that succeeds after the AR reconnects.

## What is not declared yet

- **Alarms, diagnosis, I&M** — no alarm block, no `ProblemIndicator`/diagnosis reporting, no
  I&M records in the GSDML; the device implements none of it yet. Plan 5.
- **The V2.31+ profile** — `LLDP_NoD_Supported`, the `PTP_BoundarySupported`/
  `DCP_BoundarySupported` claims, `ResetToFactoryModes`, `CertificationInfo`: all require
  `PNIO_Version >= "V2.31"` (see [Validation](#validation)) and none are implemented by this
  device today. Revisit together, as a version bump, once LLDP/PTP-DCP-boundary/ResetToFactory
  support lands.

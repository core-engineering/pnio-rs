# Spec — Plan 6: `config` + `gsdml` + `api` (typed configuration, generated GSDML, device facade)

Date: 2026-08-29. Status: design validated in brainstorm, awaiting user review.
Parent: [`2026-06-25-profinet-rt-device-design.md`](2026-06-25-profinet-rt-device-design.md) §5.1 (`config`, `api` modules), §5.3 (data types), §5.4 (API sketch), §7 (deliverables: example GSDML 16 `REAL` + 32 `BOOL`).
Builds on Plans 3, 4 and 7: `cm::DeviceModel` (slots/submodules/idents/lengths) is already generic, `rt::IoImage` gives per-submodule consistent reads/writes, `device::Device` runs the acyclic loop, `rt::RtOptions` the RT thread. The crate is `pnio`, repository `core-engineering/pnio-rs`.

## 1. Goal

A user of the crate declares the device's process data **in Rust**, gets a **GSDML that matches by construction**, imports it in TIA, and exchanges **typed values** (`BOOL`, `INT`, `WORD`, `DINT`, `REAL`) through a small facade — without touching `cm`, `rt` or `device`.

**Success criteria**
1. `DeviceConfig::builder(..)…build()` validates a declaration and derives `DeviceModel`, the field table (slot, direction, index) → (byte, bit, type), the DCP properties and a `DeviceSetup`.
2. `gsdml::render(&cfg, &meta)` produces a GSDML V2.4 file that TIA imports, whose module/submodule idents equal `cfg.model()`, and whose `DataItem` order equals the layout rule (§4).
3. `api::IoDevice` starts the device from a config in one call and offers typed reads/writes plus per-cycle-consistent slot snapshots; no change to the RT path.
4. HIL: our GSDML (dev identity `0xFFFF`/`0x0001`, station `pnio-dev`, 16 `REAL` + 32 `BOOL` per direction) imported in TIA, `typed_bringup` reaches `Data`, TIA addresses equal the computed ones, typed round-trips verified in the watch table (`REAL` 1.0 and −2.5, `%Q64.0`, `%Q67.7`), 10-minute run at 1 ms with `VERDICT: PASS` (Plan 7 thresholds, L2-pair profile). Bonus, non-blocking: a 5-minute run at 500 µs.
5. `rt_bringup`/`ar_bringup` and the rt-labs profile (`DeviceModel::pnet_sample`, goldens, `ar_replay`/`rt_replay`) untouched and green.

## 2. Scope

In:
- `data`: `Value::encode/decode` wired; bit-order test vector from the `q-bits` capture; `FieldType` kept as the name (spec §5.4's `Field` reconciled in docs).
- `config` (new): `Slot`, `Direction`, `FieldType` re-export, `SubmoduleSpec`, `DeviceConfig` + builder, `ConfigError`, layout rule, derivations (`model()`, `field()`, `fields(slot, dir)`, `dcp_properties()`, `setup()`).
- `gsdml` (new): `GsdmlMeta`, `render()`, `file_name()`, XML escaping; golden + structural tests (`roxmltree` **dev-dependency only**).
- `api` (new): `IoDevice`, `StartOptions`, `SlotSnapshot`, `SlotWriter`, `ApiError`.
- Examples: `gen_gsdml`, `typed_bringup`. Integration test `tests/typed_replay.rs`.
- Docs: `docs/gsdml.md` (new), bench §6g, README (status, Quick Start), FOLLOWUPS.

Out (FOLLOWUPS at close-out):
- A facade that absorbs `Device`/threads entirely (`ProfinetDevice` of §5.4 — approach 2).
- Config files (TOML/YAML) in the library; parsing GSDML.
- Alarms, diagnosis, I&M in the GSDML (Plan 5), `IsochroneMode`, IRT, multiple ARs, PI certification, an official Vendor ID.
- 500 µs as a criterion (bonus run only).

## 3. Decisions (locked in brainstorm)

| Subject | Decision | Why |
|---|---|---|
| Identity | `vendor_id`/`device_id` are config fields; dev default `0xFFFF`/`0x0001` documented as unofficial | rt-labs' `0x0493` must not appear in our GSDML (public repo); an official ID is the user's later step |
| Declaration | Rust builder only, no file format, no new runtime dependency | Same object renders the GSDML → no drift; YAGNI |
| Structure | `config` is the single source; `DeviceModel`, field table, DCP properties, `DeviceSetup` and GSDML are derived from it; `device`/`rt`/`cm` untouched | Approach 1: keeps the bench-validated code as is |
| Submodule | one slot = one submodule (subslot 1) = an ordered field list, mixed types allowed | `I8O8`-like records; TIA addresses follow declaration order |
| Layout | BOOL packed 8 per byte LSB-first in declaration order, a BOOL after a non-BOOL starts a new byte; INT/WORD 2 B, DINT/REAL 4 B, big-endian, no padding | Bit order verified on the wire 2026-08-27; matches rt-labs' `Unsigned8 UseAsBits` encoding |
| Idents | DAP = today's (`0x1`, `0x8000`, `0x8001`); slot *n* → module `0x100 + n`, submodule `0x1`; module *n* allowed only in slot *n* | Deterministic, identical in code and GSDML; TIA import without choices |
| GSDML | V2.4 schema (the one TIA accepted for rt-labs), text template, `StartupMode="Advanced"`, RT class 1 only, `MinDeviceInterval` from config (32 or 16) | What the bench already proved; nothing declared that the device cannot do |
| Facade | `api::IoDevice` (thin, over `Device`), typed unit accessors + per-slot consistent snapshot/writer | Removes the boilerplate both examples duplicate; RT path untouched |
| Examples | new `typed_bringup` + `gen_gsdml`; `rt_bringup`/`ar_bringup` unchanged | Non-regression of the rt-labs bench profile |
| Dev-dependency | `roxmltree` (dev only) for GSDML well-formedness/structure tests | Pure Rust, not in the published crate's dependency tree |
| HIL cycle | 1 ms = criterion; 500 µs = 5-minute bonus, non-blocking | GSDML declares 16; determinism at 500 µs is a separate question |

## 4. `config`

```rust
pub struct Slot(pub u16);                       // 1..=0x7FFF; 0 is the DAP
pub enum Direction { Input, Output, InputOutput } // device point of view: Input = device → CPU
pub use crate::data::FieldType;                 // Bool, Int, Word, Dint, Real
pub struct SubmoduleSpec { pub slot: Slot, pub name: String,
                           pub inputs: Vec<FieldType>, pub outputs: Vec<FieldType> }
pub struct FieldRef { pub byte: u16, pub bit: u8 /* 0 for non-BOOL */, pub ty: FieldType }
pub struct DeviceConfig { /* all fields private; getters */ }
pub struct DeviceConfigBuilder { .. }
```
`Direction` is derived: inputs only → `Input`, outputs only → `Output`, both → `InputOutput`.

**Builder**
```rust
let cfg = DeviceConfig::builder("edge-reg-01")        // station name
    .station_type("pnio edge")                        // DCP type-of-station, default "pnio device"
    .identity(0xFFFF, 0x0001)                         // vendor_id, device_id (defaults = these dev values)
    .min_device_interval(32)                          // 32 = 1 ms (default), 16 = 500 µs
    .input(Slot(1), &[FieldType::Real; 16])           // device → CPU (CPU %I)
    .input(Slot(2), &[FieldType::Bool; 32])
    .output(Slot(3), &[FieldType::Real; 16])          // CPU → device (CPU %Q)
    .output(Slot(4), &[FieldType::Bool; 32])
    .submodule(Slot(5), "mixed", &[FieldType::Int, FieldType::Bool], &[FieldType::Dint])
    .build()?;                                        // Result<DeviceConfig, ConfigError>
```
`ConfigError` (thiserror, `PartialEq`): `SlotZeroReserved`, `DuplicateSlot(u16)`, `EmptySubmodule(u16)`, `NoSubmodule`, `TooLong { slot, bytes, max: 1440 }`, `BadStationName(String)` (DCP rule: lowercase `a-z0-9-.` labels, ≤ 240 bytes, no leading/trailing `-`, not all digits), `BadInterval(u16)` (allowed: 8, 16, 32, 64, 128), `BadIdentity` (`vendor_id == 0`), `TooManyFields { slot, count, max: 1440 }`.

**Layout rule** (`config::layout(fields: &[FieldType]) -> (Vec<FieldRef>, u16 /* bytes */)`): walk the fields in order; a `Bool` takes bit `k % 8` of the current bit-byte (opened at the first `Bool` after a non-`Bool` or at the start); a non-`Bool` closes the current bit-byte, is placed at the next byte, advances by its size. Example `[Real, Bool, Bool, Int, Bool]` → `(0,·) (4,0) (4,1) (5,·) (7,0)`, 8 bytes; 32 `Bool` → 4 bytes; 9 `Bool` → 2 bytes; `[Bool, Int, Bool]` → 4 bytes.

**Derivations** (computed once in `build()`, immutable afterwards):
- `model(&self) -> DeviceModel`: DAP slot 0 exactly as `pnet_sample` (module `0x1`; submodules `1/0x1`, `0x8000/0x8000`, `0x8001/0x8001`, lengths 0); slot *n* → `SlotModel { slot: n, module_ident: 0x100 + n, submodules: [SubmoduleModel { subslot: 1, submodule_ident: 0x1, input_len, output_len }] }`, slots sorted ascending; `max_alarm_data_length: 200`; `vendor_id`/`device_id`/`instance: 1`/`station_name` from the config; `mac` given at `setup()`.
- `field(&self, slot, dir: Direction /* Input|Output */, index) -> Option<FieldRef>`; `fields(&self, slot, dir) -> &[FieldRef]`; `submodule(&self, slot) -> Option<&SubmoduleSpec>`; `submodules()`.
- `dcp_properties(&self, ip: [u8; 4]) -> DeviceProperties` (`device_role 0x0100`, `device_instance 1`, `device_options [1, 2, 2, 2, 2, 3]`, subnet `/24`, gateway = ip, `ip_block_info 1` — the values both examples use today).
- `setup(&self, mac: MacAddr, ip: [u8; 4], rt: Option<RtOptions>) -> DeviceSetup` (activity seed derived from the MAC as in the examples).
- `DeviceModel::pnet_sample` stays (compatibility profile); nothing in `config` depends on it.

**`data`**: `Value::encode(&self, dst: &mut [u8], bit: usize) -> Result<(), CodecError>` (BOOL uses `bit`, others ignore it and write big-endian at `dst[0..]`), `Value::decode(ty: FieldType, src: &[u8], bit: usize) -> Result<Value, CodecError>`, `Value::field_type()`. Existing `encode_*`/`decode_*`/`get_bit`/`set_bit` unchanged.

## 5. `gsdml`

```rust
pub struct GsdmlMeta { pub vendor_name: String, pub product_family: String, pub info_text: String,
                       pub date: (u16, u8, u8), pub order_number: String }
pub fn render(cfg: &DeviceConfig, meta: &GsdmlMeta) -> String;
pub fn file_name(meta: &GsdmlMeta) -> String;   // "GSDML-V2.4-<VendorName>-<ProductFamily>-<YYYYMMDD>.xml"
```
Vendor/product names in the file name are sanitized to `[A-Za-z0-9]` (TIA requires the pattern). Text fields are XML-escaped (`& < > " '`).

Generated document (schema `http://www.profibus.com/GSDML/2003/11/DeviceProfile`, `SchemaVersion="2.4"`), in this order:
- `ProfileHeader` (fixed PI values), `ProfileBody/DeviceIdentity VendorID DeviceID` + `InfoText` + `VendorName`; `DeviceFunction/Family MainFamily="I/O" ProductFamily=…`.
- `ApplicationProcess/DeviceAccessPointList/DeviceAccessPointItem ID="DAP1" PhysicalSlots="0..N" ModuleIdentNumber="0x00000001" MinDeviceInterval="<cfg>" DNS_CompatibleName="<station>" FixedInSlots="0" ObjectUUID_LocalIndex="1"`, `ModuleInfo` (name/order number), `SubslotList` (`0x8000` interface, `0x8001` port), `IOConfigData MaxInputLength/MaxOutputLength` = sums, `UseableModules` with one `ModuleItemRef ModuleItemTarget="M<n>" AllowedInSlots="<n>"` per slot, `VirtualSubmoduleList/VirtualSubmoduleItem ID="DAP1_SM" SubmoduleIdentNumber="0x00000001"` with empty `IOData`, `SystemDefinedSubmoduleList`: `InterfaceSubmoduleItem SubslotNumber="32768" SubmoduleIdentNumber="0x00008000" SupportedRT_Classes="RT_CLASS_1" SupportedProtocols="SNMP;LLDP"` + `ApplicationRelations StartupMode="Advanced"` + `TimingProperties SendClock="32" ReductionRatio="1 2 4 8 16 32 64 128 256 512"` (`SendClock="16 32"` when `min_device_interval == 16`), `PortSubmoduleItem SubslotNumber="32769" SubmoduleIdentNumber="0x00008001" MAUTypes="16"`.
- `ModuleList`: per slot `ModuleItem ID="M<n>" ModuleIdentNumber="0x00000<1nn>"`, `ModuleInfo/Name TextId`, `VirtualSubmoduleList/VirtualSubmoduleItem ID="M<n>_SM" SubmoduleIdentNumber="0x00000001"`, `IOData/Input` and/or `Output` with one `DataItem` per field in declaration order: `Float32`, `Integer16`, `Unsigned16`, `Integer32`; each BOOL group (≤ 8, same byte) → `DataItem DataType="Unsigned8" UseAsBits="true" TextId=…` with `BitDataItem BitOffset="i" TextId=…` children.
- `ExternalTextList/PrimaryLanguage`: one `Text TextId="…" Value="…"` per referenced id (`M<n>_Name`, `M<n>_SM_Name`, `M<n>_<In|Out><i>`, bit ids `M<n>_<In|Out><i>_b<j>`).
- Nothing else: no alarms, no diagnosis, no I&M, no `IsochroneMode`, no parameter records.

Tests: golden byte-exact for the sample config (`crates/pnio/testdata/gsdml/sample-16real-32bool.xml`, hand-reviewed once); `roxmltree` structural checks (§8); escaping; file name.

## 6. `api`

```rust
pub struct StartOptions { pub iface: String, pub ip: [u8; 4], pub rt: Option<RtOptions>,
                          pub app_cpus: Option<Vec<usize>> }
pub struct IoDevice { .. }                                  // Send + Sync
impl IoDevice {
    pub fn start(cfg: DeviceConfig, opts: StartOptions) -> Result<IoDevice, ApiError>;
    #[doc(hidden)] pub fn start_with<E: EthTransport + 'static, R: RpcTransport + 'static>(
        cfg, mac: MacAddr, ip, rt, eth: E, rpc: R,
        runner_factory: impl Fn(RtConfig) -> Result<RtHandle, RtError> + Send + 'static) -> Result<IoDevice, ApiError>;
    pub fn config(&self) -> &DeviceConfig;
    pub fn ar_state(&self) -> ArState;  pub fn last_abort(&self) -> Option<AbortReason>;
    pub fn validity(&self) -> Validity;  pub fn freshness(&self) -> Freshness;
    pub fn stats(&self) -> StatsSnapshot;  pub fn rt_stats(&self) -> Arc<RtStats>;
    // CPU → device (slots with outputs)
    pub fn read(&self, slot: Slot, index: usize) -> Result<Value, ApiError>;
    pub fn read_bool/read_int/read_word/read_dint/read_real(&self, slot, index) -> Result<T, ApiError>;
    pub fn outputs(&self, slot: Slot) -> Result<SlotSnapshot, ApiError>;   // consistent copy + Validity
    // device → CPU (slots with inputs)
    pub fn write(&self, slot: Slot, index: usize, v: Value) -> Result<(), ApiError>;
    pub fn write_bool/…/write_real(&self, slot, index, v) -> Result<(), ApiError>;
    pub fn with_inputs<T>(&self, slot: Slot, f: impl FnOnce(&mut SlotWriter) -> Result<T, ApiError>) -> Result<T, ApiError>;
    pub fn stop(self) -> Result<(), DeviceError>;
}
pub struct SlotSnapshot { bytes: Vec<u8>, pub validity: Validity, fields: Arc<[FieldRef]> }  // typed getters: real(i), bool(i), int(i), word(i), dint(i), get(i) -> Result<Value>
pub struct SlotWriter<'a> { .. }  // real(i, v), bool(i, v), …, set(i, Value); commit = end of with_inputs
pub enum ApiError { UnknownSlot(u16), IndexOutOfRange { slot, index, len }, TypeMismatch { slot, index, expected, got },
                    WrongDirection { slot, expected: Direction }, NoLayoutYet, Image(ImageError), Codec(CodecError), Io(std::io::Error) }
```
- `start`: reads the interface MAC (`/sys/class/net/<iface>/address`), `AfPacketTransport::open` + `attach_filter(acyclic_filter())`, `UdpRpcTransport::bind(0.0.0.0:34964)`, `cfg.setup(mac, ip, rt)`, `Device::new`, `on_state_change` → shared `(ArState, Option<AbortReason>)`, spawns thread `pnio-acyclic` running `Device::run(&stop)` (affinity `app_cpus` set inside the thread via `rt::sched::set_affinity`; `lock_memory` stays the runner's job). Any error before the spawn returns `ApiError` with nothing left running.
- Unit writes: per input-slot working copy (`Mutex<Vec<u8>>`, application side), modified then published whole via `IoImage::write_inputs`; `with_inputs` = several fields, one publish. Reads: `IoImage::read_outputs` (consistent copy under lock), decode via `Value::decode`. Before the AR reaches `Data` the image has no cells → `NoLayoutYet` (never zeros).
- `stop(self)`: set the flag, join (bounded by `Device::run`'s 200 ms poll), return the loop's result; `Drop` = stop without result.
- The facade does not touch `rt::runner`: no new syscall, lock or allocation on the RT path.

## 7. Examples and HIL

- `examples/gen_gsdml.rs`: `--out DIR` (default `.`), `--station NAME`, `--vendor-id/--device-id` (defaults dev), `--interval 32|16`; writes `file_name()` and prints the computed TIA address map (slot, direction, byte range).
- `examples/typed_bringup.rs`: sample config, `IoDevice::start`, application loop at 1 ms mirroring slot 3 → slot 1 (16 REAL) and slot 4 → slot 2 (32 BOOL) with `with_inputs`; same flags/CSV/verdict/thresholds as `rt_bringup` (shared code duplicated on purpose: the example must stay standalone).
- HIL (§1.4) on the edge, L2-pair profile, TIA update time 1 ms; expected TIA addresses: slot 1 `%IB0..63`, slot 2 `%IB64..67`, slot 3 `%QB0..63`, slot 4 `%QB64..67`; watch-table checks: `%QD0 := 1.0 → %ID0`, `%QD60 := -2.5 → %ID60`, `%Q64.0 → %I64.0`, `%Q67.7 → %I67.7`; STOP→RUN; diagnostic buffer. Bonus: 500 µs, 5 minutes.
- The CPU and TIA project keep the rt-labs device available for `rt_bringup` regression (a second device object in the project, or re-import when needed).

## 8. Tests

- `config`: layout cases of §4; every `ConfigError`; model derivation vs expected idents/lengths; `Layout::from_ar` on a synthetic Connect built for the sample config (reuse the `cm` test helpers) → C-SDU offsets (DAP IOPS ×3, then per slot data + IOxS); `field()` boundaries; `Value` round trips + `q-bits` vector (`%Q0.0` → `0x01`, `0x80` → bit 7).
- `gsdml`: golden; `roxmltree`: well-formed, `VendorID/DeviceID/DNS_CompatibleName/MinDeviceInterval`, one `ModuleItem` per slot with ident `0x100+n`, one `DataItem` per non-BOOL field and one `Unsigned8 UseAsBits` per BOOL group with correct `BitOffset`s, every `TextId` resolved, `AllowedInSlots` = slot; escaping; `file_name`.
- `api` (mock transports + `start_with`): `NoLayoutYet` before `Data`; after a replayed synthetic Connect: `with_inputs` publishes the whole submodule; injected CPU frame → `outputs(Slot(3)).real(0)` with the frame's `Validity`; `TypeMismatch`, `IndexOutOfRange`, `WrongDirection`, `UnknownSlot`; `stop()` joins; `Drop` without `stop` does not panic.
- `tests/typed_replay.rs`: end-to-end with the sample config (synthetic Connect + fabricated `0x8001` frame for the 68-byte layout).
- Existing suites unchanged and green; `cargo package -p pnio` includes `testdata/gsdml`.

## 9. Errors and edge cases

- All declaration errors surface at `build()`; nothing can fail later because of the config.
- GSDML and code cannot diverge (same object, deterministic idents/order); the golden test pins the rendering.
- Reads without a layout → `NoLayoutYet`; type/index/direction errors are explicit, never a reinterpretation.
- Application-side `Mutex` for the working copy only; the RT thread's `try_lock` discipline is unchanged.
- Station names: DCP rules enforced at build; TIA additionally lowercases — documented.
- Vendor ID `0xFFFF`: TIA accepts it in a project; the docs say it is not a PI-assigned ID and must be replaced before any deployment outside the lab.

## 10. Docs

`docs/gsdml.md` (layout rule with the worked example, declaration → TIA address, import steps, identity caveat, 500 µs note); `docs/bench-pnet-device.md` §6g; `README.md` (Status rows `config`/`gsdml`/`api`, Quick Start on `IoDevice` + `gen_gsdml`, identity warning); `FOLLOWUPS.md` (Plan 6 items resolved; new: GSDML alarms/I&M with Plan 5, `IsochroneMode`, application config file, official Vendor ID).

## 11. Dependencies

Runtime: none new. Dev: `roxmltree` (tests only). `clap`/`env_logger` already dev-deps for the examples.

## 12. Roles

Me: code (subagent-driven), GSDML generation, musl build/deploy, campaign, docs. User: TIA import + device replacement + update time, `setcap` after each copy, watch table and diagnostic buffer, STOP→RUN, the 500 µs attempt.

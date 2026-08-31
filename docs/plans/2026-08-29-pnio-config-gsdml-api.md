# Plan 6 — `config` + `gsdml` + `api` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Declare the device's process data in Rust (`DeviceConfig` builder), derive `DeviceModel` / field table / DCP properties / `DeviceSetup` from it, render a matching GSDML V2.4, and expose a thin typed facade (`api::IoDevice`) — without touching `cm`, `rt` or `device`.

**Architecture:** `config` is the single source of truth (spec §3). `gsdml::render` is a text template over the config. `api::IoDevice` wraps `device::Device` (acyclic loop in a thread) and `rt::IoImage` (typed reads/writes through the config's field table). New examples `gen_gsdml` and `typed_bringup`; the rt-labs profile (`DeviceModel::pnet_sample`, `rt_bringup`, `ar_bringup`, goldens) is untouched.

**Tech Stack:** Rust 1.96 workspace toolchain, crate `pnio` (`crates/pnio`); deps unchanged (`libc`, `log`, `nix`, `pcap-file`, `thiserror`); new **dev**-dependency `roxmltree = "0.20"`; existing dev-deps `clap`, `env_logger`.

**Spec:** `docs/design/2026-08-29-pnio-config-gsdml-api-design.md` (read it; sections referenced as spec §N).

## Global Constraints

- `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all` green after every task (CI runs exactly these). rustfmt `max_width = 100`. `cargo package -q --allow-dirty -p pnio` must succeed at the end.
- **No new runtime dependency.** `roxmltree` is a `[dev-dependencies]` entry only.
- `cm`, `rt`, `device`, `dcp`, `rpc`, `eth` are **not modified** (adding test-only helpers under `src/testutil.rs` / `tests/common/mod.rs` is fine). The RT path gains no syscall, lock or allocation.
- Layout rule (spec §4): fields in declaration order; `Bool` takes bit `k % 8` of the current bit-byte, a `Bool` after a non-`Bool` (or the first field) opens a new byte; `Int`/`Word` 2 bytes, `Dint`/`Real` 4 bytes, big-endian, no padding.
- Idents (spec §3): DAP slot 0 = module `0x1`, submodules `1/0x1`, `0x8000/0x8000`, `0x8001/0x8001` (lengths 0); slot *n* → module ident `0x100 + n`, one submodule subslot `1`, submodule ident `0x1`.
- Direction is the device's point of view: `Input` = device → CPU (CPU `%I`), `Output` = CPU → device (CPU `%Q`).
- Dev identity defaults: `vendor_id 0xFFFF`, `device_id 0x0001`, station type `"pnio device"`, `min_device_interval 32`.
- Project language English; commit subjects `feat(scope): …` / `test(scope): …` / `docs: …`. Cargo needs `. "$HOME/.cargo/env" &&` before every cargo command. Implementers **commit but never push**.

---

## File map

| File | Responsibility | Task |
|---|---|---|
| `crates/pnio/src/data.rs` | `Value::encode/decode/field_type`, q-bits vector | 1 |
| `crates/pnio/src/config.rs` | `Slot`, `Direction`, `SubmoduleSpec`, `FieldRef`, `layout()`, builder, `ConfigError`, derivations | 2, 3 |
| `crates/pnio/src/testutil.rs`, `crates/pnio/tests/common/mod.rs` | `synthetic_connect_req(model)` | 3 |
| `crates/pnio/src/gsdml.rs`, `crates/pnio/testdata/gsdml/sample-16real-32bool.xml` | GSDML rendering + golden | 4 |
| `crates/pnio/src/api.rs` | `IoDevice`, `StartOptions`, `SlotSnapshot`, `SlotWriter`, `ApiError` | 5 |
| `crates/pnio/src/lib.rs` | `pub mod config; pub mod gsdml; pub mod api;` | 2, 4, 5 |
| `crates/pnio/tests/typed_replay.rs`, `crates/pnio/examples/{gen_gsdml,typed_bringup}.rs` | end-to-end + examples | 6 |
| `docs/gsdml.md`, `docs/bench-pnet-device.md` §6g, `README.md`, `FOLLOWUPS.md` | docs | 7 |

---

### Task 1: `data::Value` — encode/decode + wire vector

**Files:**
- Modify: `crates/pnio/src/data.rs`

**Interfaces:**
- Produces: `Value::field_type(&self) -> FieldType`; `Value::encode(&self, dst: &mut [u8], bit: usize) -> Result<(), CodecError>`; `Value::decode(ty: FieldType, src: &[u8], bit: usize) -> Result<Value, CodecError>`.
- Consumes: existing `encode_*`/`decode_*`/`get_bit`/`set_bit`.

- [ ] **Step 1: Write the failing tests** (append to the `tests` module of `data.rs`)

```rust
    #[test]
    fn value_round_trips_every_type() {
        let cases = [
            (Value::Int(-2), FieldType::Int, vec![0xFF, 0xFE]),
            (Value::Word(0xBEEF), FieldType::Word, vec![0xBE, 0xEF]),
            (Value::Dint(-1), FieldType::Dint, vec![0xFF, 0xFF, 0xFF, 0xFF]),
            (Value::Real(1.0), FieldType::Real, vec![0x3F, 0x80, 0x00, 0x00]),
        ];
        for (v, ty, bytes) in cases {
            let mut buf = vec![0u8; bytes.len()];
            v.encode(&mut buf, 0).unwrap();
            assert_eq!(buf, bytes, "{v:?}");
            assert_eq!(Value::decode(ty, &buf, 0).unwrap(), v);
            assert_eq!(v.field_type(), ty);
        }
    }

    #[test]
    fn bool_value_uses_the_bit_argument_lsb_first() {
        // Bench 2026-08-27 (captures/q-bits): TIA `%Q0.0 := TRUE` alone -> output byte 0x01;
        // device input byte 0x80 -> `%I0.7` in TIA.
        let mut buf = [0u8; 1];
        Value::Bool(true).encode(&mut buf, 0).unwrap();
        assert_eq!(buf, [0x01]);
        assert_eq!(Value::decode(FieldType::Bool, &[0x80], 7).unwrap(), Value::Bool(true));
        assert_eq!(Value::decode(FieldType::Bool, &[0x80], 6).unwrap(), Value::Bool(false));
        // bit 9 lives in byte 1
        let mut two = [0u8; 2];
        Value::Bool(true).encode(&mut two, 9).unwrap();
        assert_eq!(two, [0x00, 0x02]);
    }

    #[test]
    fn value_codec_errors_are_typed() {
        assert!(matches!(
            Value::Real(0.0).encode(&mut [0u8; 3], 0),
            Err(CodecError::TooShort { need: 4, have: 3 })
        ));
        assert!(matches!(
            Value::decode(FieldType::Bool, &[0u8; 1], 8),
            Err(CodecError::BitOutOfRange { bit: 8, bytes: 1 })
        ));
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `. "$HOME/.cargo/env" && cargo test -p pnio data:: 2>&1 | tail -5`
Expected: compile error (`encode`/`decode`/`field_type` not found on `Value`).

- [ ] **Step 3: Implement**

Add after the `Value` enum:

```rust
impl Value {
    /// The process type this value carries.
    pub fn field_type(&self) -> FieldType {
        match self {
            Value::Bool(_) => FieldType::Bool,
            Value::Int(_) => FieldType::Int,
            Value::Word(_) => FieldType::Word,
            Value::Dint(_) => FieldType::Dint,
            Value::Real(_) => FieldType::Real,
        }
    }

    /// Write this value at the start of `dst` (byte types, big-endian) or at bit index
    /// `bit` of `dst` (`Bool`, LSB-first: byte `bit / 8`, mask `1 << (bit % 8)`).
    pub fn encode(&self, dst: &mut [u8], bit: usize) -> Result<(), CodecError> {
        fn put<const N: usize>(dst: &mut [u8], bytes: [u8; N]) -> Result<(), CodecError> {
            if dst.len() < N {
                return Err(CodecError::TooShort {
                    need: N,
                    have: dst.len(),
                });
            }
            dst[..N].copy_from_slice(&bytes);
            Ok(())
        }
        match *self {
            Value::Bool(b) => set_bit(dst, bit, b),
            Value::Int(v) => put(dst, encode_i16(v)),
            Value::Word(v) => put(dst, encode_u16(v)),
            Value::Dint(v) => put(dst, encode_i32(v)),
            Value::Real(v) => put(dst, encode_f32(v)),
        }
    }

    /// Read a value of type `ty` from the start of `src` (byte types) or from bit
    /// index `bit` (`Bool`).
    pub fn decode(ty: FieldType, src: &[u8], bit: usize) -> Result<Value, CodecError> {
        Ok(match ty {
            FieldType::Bool => Value::Bool(get_bit(src, bit)?),
            FieldType::Int => Value::Int(decode_i16(src)?),
            FieldType::Word => Value::Word(decode_u16(src)?),
            FieldType::Dint => Value::Dint(decode_i32(src)?),
            FieldType::Real => Value::Real(decode_f32(src)?),
        })
    }
}
```

Replace the module-level comment on `Value` ("forward declaration") if any with `/// Typed process value, encoded/decoded through the codecs below.`

- [ ] **Step 4: Run, fmt, clippy**

Run: `. "$HOME/.cargo/env" && cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test -p pnio data:: 2>&1 | grep -E "^test result|FAILED"`
Expected: `ok`, 3 new tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/pnio/src/data.rs
git commit -m "feat(data): wire Value encode/decode with the LSB-first bit vector from the bench"
```

---

### Task 2: `config` — types, layout rule, builder, validation

**Files:**
- Create: `crates/pnio/src/config.rs`
- Modify: `crates/pnio/src/lib.rs` (`pub mod config;` after `pub mod cm;` in alphabetical order)

**Interfaces:**
- Produces: `config::{Slot, Direction, SubmoduleSpec, FieldRef, ConfigError, DeviceConfig, DeviceConfigBuilder, layout, MAX_SUBMODULE_BYTES}`; `DeviceConfig::builder(station_name) -> DeviceConfigBuilder`; builder methods `station_type`, `identity`, `min_device_interval`, `input`, `output`, `submodule`, `build`; getters `station_name()`, `station_type()`, `vendor_id()`, `device_id()`, `min_device_interval()`, `submodules() -> &[SubmoduleSpec]`, `submodule(Slot) -> Option<&SubmoduleSpec>`, `fields(Slot, Direction) -> Option<&[FieldRef]>`, `field(Slot, Direction, usize) -> Option<FieldRef>`, `input_len(Slot) -> Option<u16>`, `output_len(Slot) -> Option<u16>`.
- Consumes: `data::FieldType`.

- [ ] **Step 1: Write the failing tests** (bottom of `config.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use FieldType::*;

    fn refs(v: &[(u16, u8, FieldType)]) -> Vec<FieldRef> {
        v.iter().map(|&(byte, bit, ty)| FieldRef { byte, bit, ty }).collect()
    }

    #[test]
    fn layout_mixes_bools_and_byte_types_in_declaration_order() {
        let (f, len) = layout(&[Real, Bool, Bool, Int, Bool]);
        assert_eq!(f, refs(&[(0, 0, Real), (4, 0, Bool), (4, 1, Bool), (5, 0, Int), (7, 0, Bool)]));
        assert_eq!(len, 8);
    }

    #[test]
    fn layout_packs_bools_eight_per_byte() {
        assert_eq!(layout(&[Bool; 32]).1, 4);
        let (f, len) = layout(&[Bool; 9]);
        assert_eq!(len, 2);
        assert_eq!(f[8], FieldRef { byte: 1, bit: 0, ty: Bool });
        assert_eq!(layout(&[Bool, Int, Bool]).1, 4);
        assert_eq!(layout(&[]).1, 0);
    }

    fn sample() -> DeviceConfig {
        DeviceConfig::builder("pnio-dev")
            .input(Slot(1), &[Real; 16])
            .input(Slot(2), &[Bool; 32])
            .output(Slot(3), &[Real; 16])
            .output(Slot(4), &[Bool; 32])
            .build()
            .unwrap()
    }

    #[test]
    fn builder_defaults_and_getters() {
        let cfg = sample();
        assert_eq!(cfg.station_name(), "pnio-dev");
        assert_eq!(cfg.station_type(), "pnio device");
        assert_eq!((cfg.vendor_id(), cfg.device_id()), (0xFFFF, 0x0001));
        assert_eq!(cfg.min_device_interval(), 32);
        assert_eq!(cfg.submodules().len(), 4);
        assert_eq!(cfg.input_len(Slot(1)), Some(64));
        assert_eq!(cfg.output_len(Slot(1)), Some(0));
        assert_eq!(cfg.output_len(Slot(4)), Some(4));
        assert_eq!(cfg.submodule(Slot(2)).unwrap().direction(), Direction::Input);
        assert_eq!(cfg.field(Slot(2), Direction::Input, 31), Some(FieldRef { byte: 3, bit: 7, ty: Bool }));
        assert_eq!(cfg.field(Slot(2), Direction::Input, 32), None);
        assert_eq!(cfg.field(Slot(2), Direction::Output, 0), None);
        assert_eq!(cfg.fields(Slot(3), Direction::Output).unwrap().len(), 16);
        assert_eq!(cfg.fields(Slot(9), Direction::Output), None);
    }

    #[test]
    fn slots_are_sorted_regardless_of_insertion_order() {
        let cfg = DeviceConfig::builder("a")
            .output(Slot(4), &[Bool])
            .input(Slot(1), &[Int])
            .build()
            .unwrap();
        let slots: Vec<u16> = cfg.submodules().iter().map(|s| s.slot.0).collect();
        assert_eq!(slots, vec![1, 4]);
    }

    #[test]
    fn mixed_submodule_has_both_directions() {
        let cfg = DeviceConfig::builder("a")
            .submodule(Slot(5), "mixed", &[Int, Bool], &[Dint])
            .build()
            .unwrap();
        let sm = cfg.submodule(Slot(5)).unwrap();
        assert_eq!(sm.direction(), Direction::InputOutput);
        assert_eq!((cfg.input_len(Slot(5)), cfg.output_len(Slot(5))), (Some(3), Some(4)));
        assert_eq!(sm.name, "mixed");
    }

    #[test]
    fn every_config_error_is_reported() {
        let e = |b: DeviceConfigBuilder| b.build().unwrap_err();
        assert_eq!(e(DeviceConfig::builder("a").input(Slot(0), &[Bool])), ConfigError::SlotZeroReserved);
        assert_eq!(
            e(DeviceConfig::builder("a").input(Slot(1), &[Bool]).output(Slot(1), &[Bool])),
            ConfigError::DuplicateSlot(1)
        );
        assert_eq!(e(DeviceConfig::builder("a").input(Slot(1), &[])), ConfigError::EmptySubmodule(1));
        assert_eq!(e(DeviceConfig::builder("a")), ConfigError::NoSubmodule);
        assert_eq!(
            e(DeviceConfig::builder("a").input(Slot(1), &[Real; 361])),
            ConfigError::TooLong { slot: 1, bytes: 1444, max: MAX_SUBMODULE_BYTES }
        );
        for bad in ["Edge_01", "-edge", "edge-", "123", "", "a..b", "édge"] {
            assert_eq!(
                e(DeviceConfig::builder(bad).input(Slot(1), &[Bool])),
                ConfigError::BadStationName(bad.to_string()),
                "{bad}"
            );
        }
        assert!(DeviceConfig::builder("edge-reg-01.plant2").input(Slot(1), &[Bool]).build().is_ok());
        assert_eq!(
            e(DeviceConfig::builder("a").min_device_interval(24).input(Slot(1), &[Bool])),
            ConfigError::BadInterval(24)
        );
        assert_eq!(
            e(DeviceConfig::builder("a").identity(0, 1).input(Slot(1), &[Bool])),
            ConfigError::BadIdentity
        );
    }

    #[test]
    fn station_name_is_lowercased_on_input() {
        // TIA lowercases; we accept mixed case and normalize so the DCP answer matches.
        let cfg = DeviceConfig::builder("Pnio-Dev").input(Slot(1), &[Bool]).build().unwrap();
        assert_eq!(cfg.station_name(), "pnio-dev");
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `. "$HOME/.cargo/env" && cargo test -p pnio config:: 2>&1 | tail -5`
Expected: compile error (module missing).

- [ ] **Step 3: Implement `config.rs`**

```rust
//! Typed device configuration: the single source from which the device model, the
//! per-field byte/bit table, the DCP identity and the GSDML are derived (spec §4).

use crate::data::FieldType;
use thiserror::Error;

/// Largest C-SDU a submodule may occupy in one direction (RT frame budget).
pub const MAX_SUBMODULE_BYTES: u16 = 1440;

/// A module slot number; slot 0 is the DAP and cannot carry user data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slot(pub u16);

/// Data direction from the device's point of view: `Input` = device → controller
/// (the controller's `%I`), `Output` = controller → device (its `%Q`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Input,
    Output,
    InputOutput,
}

/// One submodule: an ordered list of input fields and/or output fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmoduleSpec {
    pub slot: Slot,
    pub name: String,
    pub inputs: Vec<FieldType>,
    pub outputs: Vec<FieldType>,
}

impl SubmoduleSpec {
    pub fn direction(&self) -> Direction {
        match (self.inputs.is_empty(), self.outputs.is_empty()) {
            (false, true) => Direction::Input,
            (true, false) => Direction::Output,
            _ => Direction::InputOutput,
        }
    }
}

/// Where one field lives inside its submodule's data: byte offset, bit (LSB-first,
/// `0` for byte types) and type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldRef {
    pub byte: u16,
    pub bit: u8,
    pub ty: FieldType,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("slot 0 is the DAP and cannot carry process data")]
    SlotZeroReserved,
    #[error("slot {0} declared twice")]
    DuplicateSlot(u16),
    #[error("slot {0} has neither inputs nor outputs")]
    EmptySubmodule(u16),
    #[error("no submodule declared")]
    NoSubmodule,
    #[error("slot {slot}: {bytes} bytes exceed the {max}-byte submodule limit")]
    TooLong { slot: u16, bytes: u32, max: u16 },
    #[error("station name {0:?} is not a valid PROFINET name of station")]
    BadStationName(String),
    #[error("min device interval {0} is not one of 8, 16, 32, 64, 128")]
    BadInterval(u16),
    #[error("vendor id must be non-zero")]
    BadIdentity,
}

/// Lay out `fields` per the declaration-order rule: `Bool`s pack 8 per byte
/// (LSB-first), a `Bool` after a byte-typed field opens a new byte, byte types are
/// placed back-to-back big-endian with no padding. Returns the refs and the byte
/// length.
pub fn layout(fields: &[FieldType]) -> (Vec<FieldRef>, u16) {
    let mut refs = Vec::with_capacity(fields.len());
    let mut next_byte: u32 = 0; // first free byte
    let mut bit_byte: Option<(u32, u8)> = None; // (byte, next bit) of the open bit-byte
    for &ty in fields {
        match ty.byte_len() {
            None => {
                let (byte, bit) = match bit_byte {
                    Some((b, bit)) if bit < 8 => (b, bit),
                    _ => {
                        let b = next_byte;
                        next_byte += 1;
                        (b, 0)
                    }
                };
                bit_byte = Some((byte, bit + 1));
                refs.push(FieldRef { byte: byte as u16, bit, ty });
            }
            Some(n) => {
                bit_byte = None;
                refs.push(FieldRef { byte: next_byte as u16, bit: 0, ty });
                next_byte += n as u32;
            }
        }
    }
    (refs, next_byte as u16)
}

/// Per-submodule derived tables.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Derived {
    inputs: Vec<FieldRef>,
    input_len: u16,
    outputs: Vec<FieldRef>,
    output_len: u16,
}

/// A validated device configuration (build it with [`DeviceConfig::builder`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceConfig {
    station_name: String,
    station_type: String,
    vendor_id: u16,
    device_id: u16,
    min_device_interval: u16,
    submodules: Vec<SubmoduleSpec>,
    derived: Vec<Derived>, // parallel to `submodules`
}

pub struct DeviceConfigBuilder {
    station_name: String,
    station_type: String,
    vendor_id: u16,
    device_id: u16,
    min_device_interval: u16,
    submodules: Vec<SubmoduleSpec>,
}

impl DeviceConfig {
    pub fn builder(station_name: &str) -> DeviceConfigBuilder {
        DeviceConfigBuilder {
            station_name: station_name.to_string(),
            station_type: "pnio device".to_string(),
            vendor_id: 0xFFFF,
            device_id: 0x0001,
            min_device_interval: 32,
            submodules: Vec::new(),
        }
    }

    pub fn station_name(&self) -> &str { &self.station_name }
    pub fn station_type(&self) -> &str { &self.station_type }
    pub fn vendor_id(&self) -> u16 { self.vendor_id }
    pub fn device_id(&self) -> u16 { self.device_id }
    pub fn min_device_interval(&self) -> u16 { self.min_device_interval }
    pub fn submodules(&self) -> &[SubmoduleSpec] { &self.submodules }

    fn index_of(&self, slot: Slot) -> Option<usize> {
        self.submodules.iter().position(|s| s.slot == slot)
    }

    pub fn submodule(&self, slot: Slot) -> Option<&SubmoduleSpec> {
        self.index_of(slot).map(|i| &self.submodules[i])
    }

    /// The field refs of one direction of a slot (`InputOutput` is not a lookup key:
    /// ask for `Input` or `Output`).
    pub fn fields(&self, slot: Slot, dir: Direction) -> Option<&[FieldRef]> {
        let d = &self.derived[self.index_of(slot)?];
        match dir {
            Direction::Input if !d.inputs.is_empty() => Some(&d.inputs),
            Direction::Output if !d.outputs.is_empty() => Some(&d.outputs),
            _ => None,
        }
    }

    pub fn field(&self, slot: Slot, dir: Direction, index: usize) -> Option<FieldRef> {
        self.fields(slot, dir)?.get(index).copied()
    }

    pub fn input_len(&self, slot: Slot) -> Option<u16> {
        self.index_of(slot).map(|i| self.derived[i].input_len)
    }

    pub fn output_len(&self, slot: Slot) -> Option<u16> {
        self.index_of(slot).map(|i| self.derived[i].output_len)
    }
}

impl DeviceConfigBuilder {
    pub fn station_type(mut self, s: &str) -> Self { self.station_type = s.to_string(); self }
    pub fn identity(mut self, vendor_id: u16, device_id: u16) -> Self {
        self.vendor_id = vendor_id;
        self.device_id = device_id;
        self
    }
    pub fn min_device_interval(mut self, v: u16) -> Self { self.min_device_interval = v; self }
    /// Device → controller data in `slot` (the controller's inputs).
    pub fn input(self, slot: Slot, fields: &[FieldType]) -> Self {
        self.submodule(slot, &format!("in{}", slot.0), fields, &[])
    }
    /// Controller → device data in `slot` (the controller's outputs).
    pub fn output(self, slot: Slot, fields: &[FieldType]) -> Self {
        self.submodule(slot, &format!("out{}", slot.0), &[], fields)
    }
    pub fn submodule(mut self, slot: Slot, name: &str, inputs: &[FieldType], outputs: &[FieldType]) -> Self {
        self.submodules.push(SubmoduleSpec {
            slot,
            name: name.to_string(),
            inputs: inputs.to_vec(),
            outputs: outputs.to_vec(),
        });
        self
    }

    pub fn build(self) -> Result<DeviceConfig, ConfigError> {
        let station_name = normalize_station_name(&self.station_name)
            .ok_or_else(|| ConfigError::BadStationName(self.station_name.clone()))?;
        if !matches!(self.min_device_interval, 8 | 16 | 32 | 64 | 128) {
            return Err(ConfigError::BadInterval(self.min_device_interval));
        }
        if self.vendor_id == 0 {
            return Err(ConfigError::BadIdentity);
        }
        if self.submodules.is_empty() {
            return Err(ConfigError::NoSubmodule);
        }
        let mut submodules = self.submodules;
        submodules.sort_by_key(|s| s.slot);
        let mut derived = Vec::with_capacity(submodules.len());
        for (i, sm) in submodules.iter().enumerate() {
            if sm.slot.0 == 0 {
                return Err(ConfigError::SlotZeroReserved);
            }
            if i > 0 && submodules[i - 1].slot == sm.slot {
                return Err(ConfigError::DuplicateSlot(sm.slot.0));
            }
            if sm.inputs.is_empty() && sm.outputs.is_empty() {
                return Err(ConfigError::EmptySubmodule(sm.slot.0));
            }
            let (inputs, input_len) = checked_layout(sm.slot, &sm.inputs)?;
            let (outputs, output_len) = checked_layout(sm.slot, &sm.outputs)?;
            derived.push(Derived { inputs, input_len, outputs, output_len });
        }
        Ok(DeviceConfig {
            station_name,
            station_type: self.station_type,
            vendor_id: self.vendor_id,
            device_id: self.device_id,
            min_device_interval: self.min_device_interval,
            submodules,
            derived,
        })
    }
}

/// `layout` plus the size guard (computed in `u32` so an oversized declaration is
/// reported, not wrapped).
fn checked_layout(slot: Slot, fields: &[FieldType]) -> Result<(Vec<FieldRef>, u16), ConfigError> {
    let bytes: u32 = {
        let mut n = 0u32;
        let mut open_bits = 0u32;
        for f in fields {
            match f.byte_len() {
                None => {
                    if open_bits == 0 {
                        n += 1;
                    }
                    open_bits = (open_bits + 1) % 8;
                }
                Some(k) => {
                    open_bits = 0;
                    n += k as u32;
                }
            }
        }
        n
    };
    if bytes > MAX_SUBMODULE_BYTES as u32 {
        return Err(ConfigError::TooLong { slot: slot.0, bytes, max: MAX_SUBMODULE_BYTES });
    }
    Ok(layout(fields))
}

/// PROFINET name-of-station rule (DCP): 1..=240 bytes, labels of `[a-z0-9-]`
/// separated by `.`, no label empty, no label starting/ending with `-`, at least one
/// label not all digits (a pure number would look like an IP). Uppercase is
/// lowercased (TIA does the same).
fn normalize_station_name(s: &str) -> Option<String> {
    let s = s.to_ascii_lowercase();
    if s.is_empty() || s.len() > 240 || !s.is_ascii() {
        return None;
    }
    let mut any_non_numeric = false;
    for label in s.split('.') {
        if label.is_empty() || label.starts_with('-') || label.ends_with('-') {
            return None;
        }
        if !label.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
            return None;
        }
        if !label.bytes().all(|b| b.is_ascii_digit()) {
            any_non_numeric = true;
        }
    }
    any_non_numeric.then_some(s)
}
```
(rustfmt will expand the one-line getters; that is fine.) `lib.rs`: add `pub mod config;`.

- [ ] **Step 4: Run, fmt, clippy**

Run: `. "$HOME/.cargo/env" && cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test -p pnio config:: 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: 7 tests pass. If `TooLong` reports a different byte count for `[Real; 361]`, the expected value is `361 × 4 = 1444` — fix the implementation, not the test.

- [ ] **Step 5: Commit**

```bash
git add crates/pnio/src/config.rs crates/pnio/src/lib.rs
git commit -m "feat(config): typed device configuration — builder, validation, declaration-order layout rule"
```

---

### Task 3: `config` derivations + synthetic Connect test helper

**Files:**
- Modify: `crates/pnio/src/config.rs`
- Modify: `crates/pnio/src/testutil.rs` and `crates/pnio/tests/common/mod.rs` (add `synthetic_connect_req`)

**Interfaces:**
- Produces: `DeviceConfig::model(&self, mac: MacAddr) -> DeviceModel`; `DeviceConfig::dcp_properties(&self, ip: [u8; 4]) -> DeviceProperties`; `DeviceConfig::setup(&self, mac: MacAddr, ip: [u8; 4], rt: Option<RtOptions>) -> DeviceSetup`; `DeviceConfig::activity_seed(mac) -> Uuid`; test helper `synthetic_connect_req(model: &DeviceModel) -> Vec<u8>` (a full DCE-RPC Connect request PDU — header + NDR + blocks — as `MockRpcTransport::push_rx` expects, i.e. what the golden `connect_req[RPC_OFF..]` is).
- Consumes: `cm::{DeviceModel, SlotModel, SubmoduleModel}`, `dcp::DeviceProperties`, `device::{DeviceSetup, RtOptions}`, `rpc::{RpcHeader, NdrRequest, Drep, Uuid}`, `cm::block::ty` constants and `BlockHeader::write`, golden `connect_req` (for the RPC header template).

- [ ] **Step 1: Write the failing tests** (append to the `config` tests module)

```rust
    #[test]
    fn model_is_derived_deterministically() {
        use crate::cm::{DeviceModel, SubmoduleModel};
        let mac = crate::eth::MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);
        let m = sample().model(mac);
        let dap = DeviceModel::pnet_sample(mac);
        assert_eq!(m.slots[0], dap.slots[0]); // DAP identical to the rt-labs profile
        assert_eq!((m.vendor_id, m.device_id, m.instance), (0xFFFF, 0x0001, 1));
        assert_eq!(m.station_name, "pnio-dev");
        assert_eq!(m.max_alarm_data_length, 200);
        let idents: Vec<(u16, u32)> = m.slots[1..].iter().map(|s| (s.slot, s.module_ident)).collect();
        assert_eq!(idents, vec![(1, 0x101), (2, 0x102), (3, 0x103), (4, 0x104)]);
        assert_eq!(
            m.slots[1].submodules,
            vec![SubmoduleModel { subslot: 1, submodule_ident: 0x1, input_len: 64, output_len: 0 }]
        );
        assert_eq!(m.find(4, 1).unwrap().output_len, 4);
    }

    #[test]
    fn dcp_properties_and_setup_carry_the_identity() {
        let mac = crate::eth::MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);
        let cfg = sample();
        let p = cfg.dcp_properties([172, 16, 2, 10]);
        assert_eq!((p.vendor_id, p.device_id, p.device_role, p.device_instance), (0xFFFF, 1, 0x0100, 1));
        assert_eq!(p.name_of_station, "pnio-dev");
        assert_eq!(p.type_of_station, "pnio device");
        assert_eq!((p.ip, p.subnet, p.gateway, p.ip_block_info), ([172, 16, 2, 10], [255, 255, 255, 0], [172, 16, 2, 10], 1));
        assert_eq!(p.device_options, vec![1, 2, 2, 2, 2, 3]);
        let s = cfg.setup(mac, [172, 16, 2, 10], None);
        assert_eq!(s.dcp.mac, mac);
        assert_eq!(s.model, cfg.model(mac));
        assert_eq!(s.activity_seed.0[10..], mac.0);
        assert!(s.rt.is_none());
    }

    #[test]
    fn layout_from_ar_accepts_the_derived_model() {
        use crate::cm::{validate, ConnectReq};
        use crate::rt::Layout;
        use crate::testutil::{synthetic_connect_req, SYNTH_BLOCKS_OFF};
        let mac = crate::eth::MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);
        let model = sample().model(mac);
        let pdu = synthetic_connect_req(&model);
        let req = ConnectReq::parse(&pdu[SYNTH_BLOCKS_OFF..]).unwrap();
        let params = validate(&req, &model).unwrap();
        let layout = Layout::from_ar(&params, &model).unwrap();
        // Input CR: DAP IOPS ×3 (offsets 0,1,2), then slot 1 data@3 (64 B)+IOPS@67,
        // slot 2 data@68 (4 B)+IOPS@72, IOCS for slots 3 and 4 at 73, 74 -> 75 bytes.
        assert_eq!(layout.input_cr.data_length, 75);
        assert_eq!(layout.output_cr.data_length, 75);
        let s1 = layout.input_cr.objects.iter().find(|o| o.slot == 1).unwrap();
        assert_eq!((s1.data_off, s1.data_len, s1.iops_off), (3, 64, 67));
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `. "$HOME/.cargo/env" && cargo test -p pnio config:: 2>&1 | tail -5`
Expected: compile errors (`model`, `synthetic_connect_req` missing).

- [ ] **Step 3: Implement the derivations** (in `config.rs`)

```rust
use crate::cm::{DeviceModel, SlotModel, SubmoduleModel};
use crate::dcp::{DeviceConfig as DcpDeviceConfig, DeviceProperties};
use crate::device::{DeviceSetup, RtOptions};
use crate::eth::MacAddr;
use crate::rpc::Uuid;

impl DeviceConfig {
    /// The plug-and-play model the `cm` layer validates Connect requests against.
    pub fn model(&self, mac: MacAddr) -> DeviceModel {
        let sm = |subslot: u16, ident: u32, i: u16, o: u16| SubmoduleModel {
            subslot,
            submodule_ident: ident,
            input_len: i,
            output_len: o,
        };
        let mut slots = vec![SlotModel {
            slot: 0,
            module_ident: 0x1,
            submodules: vec![sm(1, 0x1, 0, 0), sm(0x8000, 0x8000, 0, 0), sm(0x8001, 0x8001, 0, 0)],
        }];
        for (spec, d) in self.submodules.iter().zip(&self.derived) {
            slots.push(SlotModel {
                slot: spec.slot.0,
                module_ident: 0x100 + spec.slot.0 as u32,
                submodules: vec![sm(1, 0x1, d.input_len, d.output_len)],
            });
        }
        DeviceModel {
            vendor_id: self.vendor_id,
            device_id: self.device_id,
            instance: 1,
            station_name: self.station_name.clone(),
            mac,
            max_alarm_data_length: 200,
            slots,
        }
    }

    /// DCP identity answered on the wire (Identify/Set), for the given IP (/24, no router).
    pub fn dcp_properties(&self, ip: [u8; 4]) -> DeviceProperties {
        DeviceProperties {
            name_of_station: self.station_name.clone(),
            type_of_station: self.station_type.clone(),
            vendor_id: self.vendor_id,
            device_id: self.device_id,
            device_role: 0x0100,
            device_instance: 1,
            device_options: vec![1, 2, 2, 2, 2, 3],
            ip,
            subnet: [255, 255, 255, 0],
            gateway: ip,
            ip_block_info: 1,
        }
    }

    /// The activity UUID seed our outgoing RPC calls use: fixed prefix + the MAC.
    pub fn activity_seed(mac: MacAddr) -> Uuid {
        let mut b = [0x14, 0xaf, 0x19, 0x8a, 0x12, 0x34, 0x10, 0x56, 0x80, 0x79, 0, 0, 0, 0, 0, 0];
        b[10..].copy_from_slice(&mac.0);
        Uuid(b)
    }

    /// Everything `device::Device::new` needs.
    pub fn setup(&self, mac: MacAddr, ip: [u8; 4], rt: Option<RtOptions>) -> DeviceSetup {
        DeviceSetup {
            dcp: DcpDeviceConfig { mac, properties: self.dcp_properties(ip) },
            model: self.model(mac),
            activity_seed: Self::activity_seed(mac),
            rt,
        }
    }
}
```

- [ ] **Step 4: Implement `synthetic_connect_req`** in `src/testutil.rs` (and duplicate verbatim in `tests/common/mod.rs`, replacing `crate::` by `pnio::` — the integration tests cannot see `testutil`)

```rust
/// Offset of the first PNIO block inside a Connect request PDU (RPC header 80 + NDR 20).
pub const SYNTH_BLOCKS_OFF: usize = crate::rpc::RpcHeader::LEN + crate::rpc::NdrRequest::LEN;

/// Build a complete DCE-RPC Connect request PDU (header + NDR + blocks) for `model`,
/// the way the S7-1500 does it on the bench: ARBlockReq (advanced startup, activity
/// timeout 200, station "plcxbbench.profinetxainterfacexb25fbd"), Input CR
/// (FrameID 0x8000) and Output CR (FrameID 0x8001) with the §6b object order (DAP IOxS
/// first, then per slot data + IOxS, IOCS of the opposite direction last), one
/// ExpectedSubmoduleBlockReq per slot, AlarmCRBlockReq. The RPC header is the golden
/// `connect_req` one with `frag_len` recomputed; `cm` checks only the interface UUID.
pub fn synthetic_connect_req(model: &crate::cm::DeviceModel) -> Vec<u8> {
    use crate::cm::block::{ty, BlockHeader};
    use crate::rpc::{Drep, NdrRequest, RpcHeader};

    fn block(out: &mut Vec<u8>, block_type: u16, body: &[u8]) {
        BlockHeader::write(out, block_type, body.len() as u16);
        out.extend_from_slice(body);
    }
    fn u16(b: &mut Vec<u8>, v: u16) { b.extend_from_slice(&v.to_be_bytes()); }
    fn u32(b: &mut Vec<u8>, v: u32) { b.extend_from_slice(&v.to_be_bytes()); }

    let golden = golden("connect_req");
    let mut hdr = RpcHeader::parse(&golden[RPC_OFF..]).unwrap();
    let ar = crate::cm::ArBlockReq::parse(&golden[RPC_OFF + SYNTH_BLOCKS_OFF + 6..]).unwrap();

    // --- ARBlockReq: same values as the bench CPU, station name included ---
    let mut body = Vec::new();
    u16(&mut body, 1);
    ar.ar_uuid.write(&mut body, Drep::BIG);
    u16(&mut body, ar.session_key);
    body.extend_from_slice(&ar.initiator_mac.0);
    ar.initiator_object_uuid.write(&mut body, Drep::BIG);
    u32(&mut body, ar.ar_properties);
    u16(&mut body, ar.activity_timeout_factor);
    u16(&mut body, 0x8892);
    u16(&mut body, ar.station_name.len() as u16);
    body.extend_from_slice(ar.station_name.as_bytes());
    let mut blocks = Vec::new();
    block(&mut blocks, ty::AR_BLOCK_REQ, &body);

    // --- IOCR objects in §6b order ---
    struct Obj { slot: u16, subslot: u16, off: u16 }
    let mut in_data = Vec::new(); // objects we produce (Input CR): DAP IOPS, then inputs
    let mut in_iocs = Vec::new(); // our IOCS for the outputs we consume
    let mut out_data = Vec::new(); // Output CR: outputs the CPU produces
    let mut out_iocs = Vec::new(); // CPU's IOCS for our inputs
    let mut in_off: u16 = 0;
    let mut out_off: u16 = 0;
    for s in &model.slots {
        for sm in &s.submodules {
            let is_dap = s.slot == 0;
            let has_in = sm.input_len > 0 || is_dap;
            let has_out = sm.output_len > 0;
            if has_in {
                in_data.push(Obj { slot: s.slot, subslot: sm.subslot, off: in_off });
                in_off += sm.input_len + 1;
                out_iocs.push(Obj { slot: s.slot, subslot: sm.subslot, off: out_off });
                out_off += 1;
            }
            if has_out {
                out_data.push(Obj { slot: s.slot, subslot: sm.subslot, off: out_off });
                out_off += sm.output_len + 1;
                in_iocs.push(Obj { slot: s.slot, subslot: sm.subslot, off: in_off });
                in_off += 1;
            }
        }
    }
    let iocr = |iocr_type: u16, reference: u16, frame_id: u16, len: u16, data: &[Obj], iocs: &[Obj]| {
        let mut b = Vec::new();
        u16(&mut b, iocr_type);
        u16(&mut b, reference);
        u16(&mut b, 0x8892); // LT
        u32(&mut b, 0x0000_0002); // IOCRProperties: RTClass 2 (what the CPU sends)
        u16(&mut b, len.max(40));
        u16(&mut b, frame_id);
        u16(&mut b, 32); // send clock factor
        u16(&mut b, 1); // reduction ratio (1 ms)
        u16(&mut b, 1); // phase
        u16(&mut b, 0); // sequence
        u32(&mut b, 0xffff_ffff); // frame send offset
        u16(&mut b, 3); // watchdog factor
        u16(&mut b, 3); // data hold factor
        u16(&mut b, 0xc000); // tag header
        b.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // multicast MAC
        u16(&mut b, 1); // number of APIs
        u32(&mut b, 0);
        u16(&mut b, data.len() as u16);
        for o in data { u16(&mut b, o.slot); u16(&mut b, o.subslot); u16(&mut b, o.off); }
        u16(&mut b, iocs.len() as u16);
        for o in iocs { u16(&mut b, o.slot); u16(&mut b, o.subslot); u16(&mut b, o.off); }
        b
    };
    block(&mut blocks, ty::IOCR_BLOCK_REQ, &iocr(1, 1, 0x8000, in_off, &in_data, &in_iocs));
    block(&mut blocks, ty::IOCR_BLOCK_REQ, &iocr(2, 2, 0x8001, out_off, &out_data, &out_iocs));

    // --- one ExpectedSubmoduleBlockReq per slot ---
    for s in &model.slots {
        let mut b = Vec::new();
        u16(&mut b, 1); // number of APIs
        u32(&mut b, 0);
        u16(&mut b, s.slot);
        u32(&mut b, s.module_ident);
        u16(&mut b, 0); // module properties
        u16(&mut b, s.submodules.len() as u16);
        for sm in &s.submodules {
            u16(&mut b, sm.subslot);
            u32(&mut b, sm.submodule_ident);
            let props: u16 = match (sm.input_len > 0, sm.output_len > 0) {
                (false, false) => 0,
                (true, false) => 1,
                (false, true) => 2,
                (true, true) => 3,
            };
            u16(&mut b, props);
            let desc = |b: &mut Vec<u8>, tag: u16, len: u16| { u16(b, tag); u16(b, len); b.push(1); b.push(1); };
            match props {
                0 | 1 => desc(&mut b, 1, sm.input_len),
                2 => desc(&mut b, 2, sm.output_len),
                _ => { desc(&mut b, 1, sm.input_len); desc(&mut b, 2, sm.output_len); }
            }
        }
        block(&mut blocks, ty::EXPECTED_SUBMODULE_BLOCK_REQ, &b);
    }

    // --- AlarmCRBlockReq (bench values) ---
    let mut b = Vec::new();
    u16(&mut b, 1); u16(&mut b, 0x8892); u32(&mut b, 0); u16(&mut b, 1); u16(&mut b, 3);
    u16(&mut b, 1); u16(&mut b, 200); u16(&mut b, 0xc000); u16(&mut b, 0xa000);
    block(&mut blocks, ty::ALARM_CR_BLOCK_REQ, &b);

    // --- PDU: header (frag_len = NDR + blocks) + NDR + blocks ---
    let ndr = NdrRequest::for_blocks(blocks.len() as u32 + 16, blocks.len() as u32);
    hdr.frag_len = (NdrRequest::LEN + blocks.len()) as u16;
    let mut pdu = Vec::new();
    hdr.write(&mut pdu);
    ndr.write(&mut pdu, hdr.drep);
    pdu.extend_from_slice(&blocks);
    pdu
}
```
Check against the parsers while writing: `ExpectedSubmoduleBlockReq::parse` reads `properties & 0x3` to decide which `DataDescription`s follow (Type 0 still carries one Input description with length 0 — the code above does that), `DataDescription` = tag u16, length u16, `length_iocs` u8, `length_iops` u8. `AlarmCrBlockReq` = type, LT, properties u32, rta timeout, retries, local ref, max alarm data length, tag high, tag low. If a field is `pub(crate)`/private (e.g. `ArBlockReq`, `BlockHeader`, `ty`), make the minimal `pub` change in `cm` visibility only (no logic change) and say so in the report. `NdrRequest::for_blocks(args_max, len)` and `RpcHeader::write` exist; the golden's `connect_req` ArBlockReq lives at `RPC_OFF + 100 + 6` (header 80 + NDR 20 + block header 6).

Sanity test for the helper itself (in `testutil` tests): `synthetic_connect_req(&DeviceModel::pnet_sample(mac))` must parse (`ConnectReq::parse(&pdu[SYNTH_BLOCKS_OFF..])`) and `validate` against `pnet_sample` with the same IOCR data lengths as the golden (`input.data_length == 40`, `output.data_length == 40` after the `.max(40)` padding; objects: DAP IOPS at 0,1,2, slot 1 data at 3 — compare with `docs/bench-pnet-device.md` §6b's table).

- [ ] **Step 5: Run, fmt, clippy**

Run: `. "$HOME/.cargo/env" && cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test -p pnio 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: green; the three new config tests + the helper sanity test pass.

- [ ] **Step 6: Commit**

```bash
git add crates/pnio/src/config.rs crates/pnio/src/testutil.rs crates/pnio/tests/common/mod.rs crates/pnio/src/cm
git commit -m "feat(config): derive DeviceModel, DCP identity and DeviceSetup; test-only synthetic Connect request builder"
```

---

### Task 4: `gsdml` — rendering, file name, golden + structural tests

**Files:**
- Create: `crates/pnio/src/gsdml.rs`, `crates/pnio/testdata/gsdml/sample-16real-32bool.xml`
- Modify: `crates/pnio/src/lib.rs` (`pub mod gsdml;`), `crates/pnio/Cargo.toml` (`[dev-dependencies] roxmltree = "0.20"`)

**Interfaces:**
- Produces: `gsdml::{GsdmlMeta, render, file_name, escape}`.
- Consumes: `DeviceConfig` getters (`submodules`, `fields`, `input_len`, `output_len`, identity, `min_device_interval`, `station_name`).

- [ ] **Step 1: Write the failing tests** (bottom of `gsdml.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DeviceConfig, Slot};
    use crate::data::FieldType::*;

    fn sample() -> DeviceConfig {
        DeviceConfig::builder("pnio-dev")
            .input(Slot(1), &[Real; 16])
            .input(Slot(2), &[Bool; 32])
            .output(Slot(3), &[Real; 16])
            .output(Slot(4), &[Bool; 32])
            .build()
            .unwrap()
    }

    fn meta() -> GsdmlMeta {
        GsdmlMeta {
            vendor_name: "Core Engineering".into(),
            product_family: "pnio".into(),
            info_text: "pnio sample device: 16 REAL + 32 BOOL per direction".into(),
            order_number: "PNIO-SAMPLE".into(),
            date: (2026, 8, 29),
        }
    }

    #[test]
    fn file_name_follows_the_tia_pattern() {
        assert_eq!(file_name(&meta()), "GSDML-V2.4-CoreEngineering-pnio-20260829.xml");
    }

    #[test]
    fn escape_handles_the_five_xml_specials() {
        assert_eq!(escape("a<b&c>\"d'"), "a&lt;b&amp;c&gt;&quot;d&apos;");
    }

    #[test]
    fn render_matches_the_golden() {
        let got = render(&sample(), &meta());
        let want = std::fs::read_to_string(format!(
            "{}/testdata/gsdml/sample-16real-32bool.xml",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        assert_eq!(got, want.replace("\r\n", "\n"));
    }

    #[test]
    fn render_is_well_formed_and_structurally_consistent() {
        let cfg = sample();
        let xml = render(&cfg, &meta());
        let doc = roxmltree::Document::parse(&xml).expect("well-formed");
        let find = |name: &str| doc.descendants().filter(move |n| n.has_tag_name(name)).collect::<Vec<_>>();
        let ident = find("DeviceIdentity")[0];
        assert_eq!(ident.attribute("VendorID"), Some("0xFFFF"));
        assert_eq!(ident.attribute("DeviceID"), Some("0x0001"));
        let dap = find("DeviceAccessPointItem")[0];
        assert_eq!(dap.attribute("DNS_CompatibleName"), Some("pnio-dev"));
        assert_eq!(dap.attribute("MinDeviceInterval"), Some("32"));
        assert_eq!(dap.attribute("PhysicalSlots"), Some("0..4"));
        let modules = find("ModuleItem");
        assert_eq!(modules.len(), 4);
        let mac = crate::eth::MacAddr([0; 6]);
        let model = cfg.model(mac);
        for (m, s) in modules.iter().zip(&model.slots[1..]) {
            assert_eq!(m.attribute("ModuleIdentNumber"), Some(format!("0x{:08X}", s.module_ident).as_str()));
        }
        let refs = find("ModuleItemRef");
        let allowed: Vec<_> = refs.iter().map(|r| r.attribute("AllowedInSlots").unwrap()).collect();
        assert_eq!(allowed, vec!["1", "2", "3", "4"]);
        // 16 REAL -> 16 Float32 DataItems; 32 BOOL -> 4 Unsigned8 UseAsBits with 8 BitDataItems each
        let items = find("DataItem");
        assert_eq!(items.iter().filter(|i| i.attribute("DataType") == Some("Float32")).count(), 32);
        let bit_items: Vec<_> = items.iter().filter(|i| i.attribute("UseAsBits") == Some("true")).collect();
        assert_eq!(bit_items.len(), 8);
        for b in &bit_items {
            let offsets: Vec<_> = b.children().filter(|c| c.has_tag_name("BitDataItem")).map(|c| c.attribute("BitOffset").unwrap().to_string()).collect();
            assert_eq!(offsets, ["0", "1", "2", "3", "4", "5", "6", "7"]);
        }
        // every TextId is defined
        let defined: std::collections::HashSet<_> = find("Text").iter().map(|t| t.attribute("TextId").unwrap().to_string()).collect();
        for n in doc.descendants() {
            if let Some(id) = n.attribute("TextId") {
                if !n.has_tag_name("Text") {
                    assert!(defined.contains(id), "TextId {id} undefined");
                }
            }
        }
        let timing = find("TimingProperties")[0];
        assert_eq!(timing.attribute("SendClock"), Some("32"));
    }

    #[test]
    fn interval_16_declares_both_send_clocks() {
        let cfg = DeviceConfig::builder("a").min_device_interval(16).input(Slot(1), &[Int]).build().unwrap();
        let xml = render(&cfg, &meta());
        assert!(xml.contains("MinDeviceInterval=\"16\""));
        assert!(xml.contains("SendClock=\"16 32\""));
    }

    #[test]
    fn mixed_submodule_renders_input_and_output_lists_and_a_partial_bit_group() {
        let cfg = DeviceConfig::builder("a")
            .submodule(Slot(5), "mixed", &[Int, Bool, Bool, Bool], &[Dint])
            .build()
            .unwrap();
        let xml = render(&cfg, &meta());
        let doc = roxmltree::Document::parse(&xml).unwrap();
        let input = doc.descendants().find(|n| n.has_tag_name("Input")).unwrap();
        let items: Vec<_> = input.children().filter(|c| c.has_tag_name("DataItem")).collect();
        assert_eq!(items[0].attribute("DataType"), Some("Integer16"));
        assert_eq!(items[1].attribute("DataType"), Some("Unsigned8"));
        assert_eq!(items[1].children().filter(|c| c.has_tag_name("BitDataItem")).count(), 3);
        let output = doc.descendants().find(|n| n.has_tag_name("Output")).unwrap();
        assert_eq!(output.children().filter(|c| c.has_tag_name("DataItem")).next().unwrap().attribute("DataType"), Some("Integer32"));
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `. "$HOME/.cargo/env" && cargo test -p pnio gsdml:: 2>&1 | tail -5`
Expected: compile error.

- [ ] **Step 3: Implement `gsdml.rs`**

```rust
//! GSDML V2.4 rendering from a [`DeviceConfig`]: same object, same idents, same field
//! order as the code, so the file cannot drift from what the device answers (spec §5).
//! Text template; no XML library in the crate.

use crate::config::{DeviceConfig, Direction, FieldRef};
use crate::data::FieldType;
use std::fmt::Write;

/// Vendor/product texts and the date that go into the file (and its name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GsdmlMeta {
    pub vendor_name: String,
    pub product_family: String,
    pub info_text: String,
    pub order_number: String,
    /// (year, month, day) — the GSDML release date, also the file-name suffix.
    pub date: (u16, u8, u8),
}

/// `GSDML-V2.4-<Vendor>-<Product>-<YYYYMMDD>.xml`, names stripped to `[A-Za-z0-9]`
/// (TIA rejects files that do not match this pattern).
pub fn file_name(meta: &GsdmlMeta) -> String {
    let clean = |s: &str| s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>();
    format!(
        "GSDML-V2.4-{}-{}-{:04}{:02}{:02}.xml",
        clean(&meta.vendor_name),
        clean(&meta.product_family),
        meta.date.0,
        meta.date.1,
        meta.date.2
    )
}

/// Escape the five XML specials for attribute/text content.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// One `DataItem` (byte type) or one `Unsigned8 UseAsBits` group (≤ 8 BOOLs of the
/// same byte), in declaration order.
enum Item<'a> {
    Scalar(FieldType, usize),
    Bits(Vec<(usize, &'a FieldRef)>),
}

fn items(fields: &[FieldRef]) -> Vec<Item<'_>> {
    let mut out = Vec::new();
    for (i, f) in fields.iter().enumerate() {
        match f.ty {
            FieldType::Bool => match out.last_mut() {
                Some(Item::Bits(g)) if g.last().map(|(_, r)| r.byte) == Some(f.byte) => g.push((i, f)),
                _ => out.push(Item::Bits(vec![(i, f)])),
            },
            ty => out.push(Item::Scalar(ty, i)),
        }
    }
    out
}

fn data_type(ty: FieldType) -> &'static str {
    match ty {
        FieldType::Bool => "Unsigned8",
        FieldType::Int => "Integer16",
        FieldType::Word => "Unsigned16",
        FieldType::Dint => "Integer32",
        FieldType::Real => "Float32",
    }
}

/// Render the GSDML document for `cfg`.
pub fn render(cfg: &DeviceConfig, meta: &GsdmlMeta) -> String {
    let mut x = String::with_capacity(16 * 1024);
    let mut texts: Vec<(String, String)> = Vec::new(); // (TextId, Value)
    let n_slots = cfg.submodules().last().map(|s| s.slot.0).unwrap_or(0);
    let (max_in, max_out) = cfg.submodules().iter().fold((0u32, 0u32), |(i, o), s| {
        (i + cfg.input_len(s.slot).unwrap_or(0) as u32, o + cfg.output_len(s.slot).unwrap_or(0) as u32)
    });
    let send_clock = if cfg.min_device_interval() < 32 { format!("{} 32", cfg.min_device_interval()) } else { "32".to_string() };
    let (y, m, d) = meta.date;

    let _ = write!(x, r#"<?xml version="1.0" encoding="utf-8"?>
<ISO15745Profile xmlns="http://www.profibus.com/GSDML/2003/11/DeviceProfile" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:schemaLocation="http://www.profibus.com/GSDML/2003/11/DeviceProfile ..\xsd\GSDML-DeviceProfile-V2.4.xsd">
  <ProfileHeader>
    <ProfileIdentification>PROFINET Device Profile</ProfileIdentification>
    <ProfileRevision>1.00</ProfileRevision>
    <ProfileName>Device Profile for PROFINET Devices</ProfileName>
    <ProfileSource>PROFIBUS Nutzerorganisation e. V. (PNO)</ProfileSource>
    <ProfileClassID>Device</ProfileClassID>
    <ISO15745Reference>
      <ISO15745Part>4</ISO15745Part>
      <ISO15745Edition>1</ISO15745Edition>
      <ProfileTechnology>GSDML</ProfileTechnology>
    </ISO15745Reference>
  </ProfileHeader>
  <ProfileBody>
    <DeviceIdentity VendorID="0x{vid:04X}" DeviceID="0x{did:04X}">
      <InfoText TextId="T_InfoText"/>
      <VendorName Value="{vendor}"/>
    </DeviceIdentity>
    <DeviceFunction>
      <Family MainFamily="I/O" ProductFamily="{family}"/>
    </DeviceFunction>
    <ApplicationProcess>
      <DeviceAccessPointList>
        <DeviceAccessPointItem ID="DAP1" PhysicalSlots="0..{n_slots}" ModuleIdentNumber="0x00000001" MinDeviceInterval="{mdi}" DNS_CompatibleName="{station}" FixedInSlots="0" ObjectUUID_LocalIndex="1" MultipleWriteSupported="true" DeviceAccessSupported="false">
          <ModuleInfo>
            <Name TextId="T_DAP_Name"/>
            <InfoText TextId="T_DAP_Info"/>
            <VendorName Value="{vendor}"/>
            <OrderNumber Value="{order}"/>
            <HardwareRelease Value="1.0"/>
            <SoftwareRelease Value="V0.0.0"/>
          </ModuleInfo>
          <SubslotList>
            <SubslotItem SubslotNumber="32768" TextId="T_Interface"/>
            <SubslotItem SubslotNumber="32769" TextId="T_Port1"/>
          </SubslotList>
          <IOConfigData MaxInputLength="{max_in}" MaxOutputLength="{max_out}"/>
          <UseableModules>
"#,
        vid = cfg.vendor_id(), did = cfg.device_id(), vendor = escape(&meta.vendor_name),
        family = escape(&meta.product_family), n_slots = n_slots, mdi = cfg.min_device_interval(),
        station = escape(cfg.station_name()), order = escape(&meta.order_number),
        max_in = max_in, max_out = max_out);
    for s in cfg.submodules() {
        let _ = writeln!(x, r#"            <ModuleItemRef ModuleItemTarget="M{n}" AllowedInSlots="{n}"/>"#, n = s.slot.0);
    }
    let _ = write!(x, r#"          </UseableModules>
          <VirtualSubmoduleList>
            <VirtualSubmoduleItem ID="DAP1_SM" SubmoduleIdentNumber="0x00000001" MayIssueProcessAlarm="false">
              <IOData/>
              <ModuleInfo>
                <Name TextId="T_DAP_Name"/>
                <InfoText TextId="T_DAP_Info"/>
              </ModuleInfo>
            </VirtualSubmoduleItem>
          </VirtualSubmoduleList>
          <SystemDefinedSubmoduleList>
            <InterfaceSubmoduleItem ID="DAP1_IF" SubslotNumber="32768" TextId="T_Interface" SubmoduleIdentNumber="0x00008000" SupportedRT_Classes="RT_CLASS_1" SupportedProtocols="SNMP;LLDP" DCP_HelloSupported="false" PTP_BoundarySupported="false" DCP_BoundarySupported="false" DelayMeasurementSupported="false">
              <ApplicationRelations StartupMode="Advanced">
                <TimingProperties SendClock="{send_clock}" ReductionRatio="1 2 4 8 16 32 64 128 256 512"/>
              </ApplicationRelations>
            </InterfaceSubmoduleItem>
            <PortSubmoduleItem ID="DAP1_P1" SubslotNumber="32769" TextId="T_Port1" SubmoduleIdentNumber="0x00008001" MAUTypes="16" MaxPortTxDelay="160" MaxPortRxDelay="350"/>
          </SystemDefinedSubmoduleList>
        </DeviceAccessPointItem>
      </DeviceAccessPointList>
      <ModuleList>
"#, send_clock = send_clock);
    texts.push(("T_InfoText".into(), escape(&meta.info_text)));
    texts.push(("T_DAP_Name".into(), format!("{} DAP", escape(&meta.product_family))));
    texts.push(("T_DAP_Info".into(), "Device access point".into()));
    texts.push(("T_Interface".into(), "PROFINET interface".into()));
    texts.push(("T_Port1".into(), "Port 1".into()));

    for s in cfg.submodules() {
        let n = s.slot.0;
        let _ = write!(x, r#"        <ModuleItem ID="M{n}" ModuleIdentNumber="0x{ident:08X}">
          <ModuleInfo>
            <Name TextId="M{n}_Name"/>
            <InfoText TextId="M{n}_Info"/>
            <OrderNumber Value="{order}-M{n}"/>
          </ModuleInfo>
          <VirtualSubmoduleList>
            <VirtualSubmoduleItem ID="M{n}_SM" SubmoduleIdentNumber="0x00000001" MayIssueProcessAlarm="false">
              <IOData>
"#, ident = 0x100u32 + n as u32, order = escape(&meta.order_number));
        texts.push((format!("M{n}_Name"), escape(&s.name)));
        texts.push((format!("M{n}_Info"), format!("Slot {n}: {} input bytes, {} output bytes", cfg.input_len(s.slot).unwrap_or(0), cfg.output_len(s.slot).unwrap_or(0))));
        for (dir, tag, prefix) in [(Direction::Input, "Input", "In"), (Direction::Output, "Output", "Out")] {
            let Some(fields) = cfg.fields(s.slot, dir) else { continue };
            let _ = writeln!(x, "                <{tag}>");
            for item in items(fields) {
                match item {
                    Item::Scalar(ty, i) => {
                        let id = format!("M{n}_{prefix}{i}");
                        let _ = writeln!(x, r#"                  <DataItem DataType="{}" TextId="{id}"/>"#, data_type(ty));
                        texts.push((id, format!("{prefix} {i} ({ty:?})")));
                    }
                    Item::Bits(group) => {
                        let first = group[0].0;
                        let id = format!("M{n}_{prefix}{first}_bits");
                        let _ = writeln!(x, r#"                  <DataItem DataType="Unsigned8" UseAsBits="true" TextId="{id}">"#);
                        texts.push((id, format!("{prefix} bits {}..{}", first, group.last().unwrap().0)));
                        for (i, f) in &group {
                            let bid = format!("M{n}_{prefix}{i}_b");
                            let _ = writeln!(x, r#"                    <BitDataItem BitOffset="{}" TextId="{bid}"/>"#, f.bit);
                            texts.push((bid, format!("{prefix} {i} (Bool)")));
                        }
                        let _ = writeln!(x, "                  </DataItem>");
                    }
                }
            }
            let _ = writeln!(x, "                </{tag}>");
        }
        let _ = write!(x, r#"              </IOData>
              <ModuleInfo>
                <Name TextId="M{n}_Name"/>
                <InfoText TextId="M{n}_Info"/>
              </ModuleInfo>
            </VirtualSubmoduleItem>
          </VirtualSubmoduleList>
        </ModuleItem>
"#);
    }
    let _ = write!(x, r#"      </ModuleList>
      <ExternalTextList>
        <PrimaryLanguage>
"#);
    for (id, value) in &texts {
        let _ = writeln!(x, r#"          <Text TextId="{id}" Value="{value}"/>"#);
    }
    let _ = write!(x, r#"        </PrimaryLanguage>
      </ExternalTextList>
    </ApplicationProcess>
  </ProfileBody>
</ISO15745Profile>
"#);
    let _ = (y, m, d); // the date lives in the file name; GSDML V2.4 has no release-date attribute we need
    x
}
```
Note the `{ty:?}` in `format!` for `FieldType` (derives `Debug`). The text values are already escaped where user-provided; generated ones contain no specials.

Golden: run `render(&sample(), &meta())` once (a tiny `#[test] #[ignore] fn dump_golden()` that writes the file is acceptable, then delete it, or use `cargo run --example gen_gsdml` after Task 6 — for this task write it from a unit test guarded by `#[ignore]` and run it with `--ignored` once), open it, check it by eye against the structure above (DAP, 4 modules, 32 `Float32`, 8 bit groups, all texts), and commit it as `crates/pnio/testdata/gsdml/sample-16real-32bool.xml` with LF endings.

`Cargo.toml`: add `roxmltree = "0.20"` under `[dev-dependencies]`. `lib.rs`: `pub mod gsdml;`.

- [ ] **Step 4: Run, fmt, clippy**

Run: `. "$HOME/.cargo/env" && cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test -p pnio gsdml:: 2>&1 | grep -E "^test result|FAILED|panicked" && cargo package -q --allow-dirty -p pnio && echo PACKAGE_OK`
Expected: 6 tests pass; package OK (the golden is under `testdata/`, included).

- [ ] **Step 5: Commit**

```bash
git add crates/pnio/src/gsdml.rs crates/pnio/src/lib.rs crates/pnio/Cargo.toml crates/pnio/testdata/gsdml
git commit -m "feat(gsdml): render a GSDML V2.4 from DeviceConfig (golden + structural tests)"
```

---

### Task 5: `api::IoDevice` — facade over `Device` + `IoImage`

**Files:**
- Create: `crates/pnio/src/api.rs`
- Modify: `crates/pnio/src/lib.rs` (`pub mod api;`)

**Interfaces:**
- Produces: `api::{IoDevice, StartOptions, SlotSnapshot, SlotWriter, ApiError}` per spec §6 (signatures below).
- Consumes: `config::{DeviceConfig, Slot, Direction, FieldRef}`, `data::Value`, `device::{Device, DeviceSetup, DeviceError, RtOptions}`, `rt::{IoImage, Validity, Freshness, RtStats, StatsSnapshot, RtConfig, RtHandle, RtError}`, `cm::{ArState, AbortReason}`, `eth::{AfPacketTransport, EthTransport, MacAddr, bpf::acyclic_filter}`, `rpc::{UdpRpcTransport, RpcTransport, PNIO_UDP_PORT}`, `rt::sched::set_affinity`, `testutil::synthetic_connect_req` (tests).

- [ ] **Step 1: Write the failing tests** (bottom of `api.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cm::ArState;
    use crate::config::{DeviceConfig, Slot};
    use crate::data::FieldType::*;
    use crate::eth::{MacAddr, MockTransport};
    use crate::rpc::MockRpcTransport;
    use crate::rt::RtRunner;
    use crate::testutil::{golden, synthetic_connect_req, RPC_OFF};
    use std::time::Duration;

    const DEV: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);

    fn sample() -> DeviceConfig {
        DeviceConfig::builder("pnio-dev")
            .input(Slot(1), &[Real; 16])
            .input(Slot(2), &[Bool; 32])
            .output(Slot(3), &[Real; 16])
            .output(Slot(4), &[Bool; 32])
            .build()
            .unwrap()
    }

    /// Start on mocks with the AR driven to Data by a synthetic Connect + the golden
    /// Write/PrmEnd/AppReady exchange (their bodies do not depend on the model).
    fn started() -> (IoDevice, std::sync::Arc<MockTransport>) {
        let cfg = sample();
        let model = cfg.model(DEV);
        let rpc = MockRpcTransport::new();
        let cpu = "172.16.2.100:54766".parse().unwrap();
        let cpu_cm = "172.16.2.100:34964".parse().unwrap();
        rpc.push_rx(synthetic_connect_req(&model), cpu);
        rpc.push_rx(golden("write_req")[RPC_OFF..].to_vec(), cpu);
        rpc.push_rx(golden("prmend_req")[RPC_OFF..].to_vec(), cpu);
        rpc.push_rx(golden("appready_res")[RPC_OFF..].to_vec(), cpu_cm);
        let eth = std::sync::Arc::new(MockTransport::new());
        let eth_for_runner = eth.clone();
        let dev = IoDevice::start_with(cfg, DEV, [172, 16, 2, 10], None, SharedMock(eth.clone()), rpc, move |rt_cfg| {
            RtRunner::spawn_with_transport(rt_cfg, SharedMock(eth_for_runner.clone()))
        })
        .unwrap();
        (dev, eth)
    }

    /// `Arc<MockTransport>` adapter so the acyclic loop and the RT thread share one mock.
    struct SharedMock(std::sync::Arc<MockTransport>);
    impl crate::eth::EthTransport for SharedMock {
        fn send(&self, f: &[u8]) -> Result<(), crate::eth::TransportError> { self.0.send(f) }
        fn recv_into(&self, b: &mut [u8], t: Option<Duration>) -> Result<Option<usize>, crate::eth::TransportError> { self.0.recv_into(b, t) }
    }

    fn wait_for(dev: &IoDevice, st: ArState) {
        let t0 = std::time::Instant::now();
        while dev.ar_state() != st {
            assert!(t0.elapsed() < Duration::from_secs(2), "AR stuck in {:?}", dev.ar_state());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn reads_before_data_are_no_layout_yet() {
        let cfg = sample();
        let dev = IoDevice::start_with(cfg, DEV, [172, 16, 2, 10], None, MockTransport::new(), MockRpcTransport::new(), |c| RtRunner::spawn_with_transport(c, MockTransport::new())).unwrap();
        assert_eq!(dev.ar_state(), ArState::Idle);
        assert_eq!(dev.read_real(Slot(3), 0).unwrap_err(), ApiError::NoLayoutYet);
        assert_eq!(dev.write_real(Slot(1), 0, 1.0).unwrap_err(), ApiError::NoLayoutYet);
        dev.stop().unwrap();
    }

    #[test]
    fn typed_errors_come_from_the_config_table() {
        let (dev, _eth) = started();
        wait_for(&dev, ArState::Data);
        assert_eq!(dev.read_real(Slot(9), 0).unwrap_err(), ApiError::UnknownSlot(9));
        assert_eq!(dev.read_real(Slot(3), 16).unwrap_err(), ApiError::IndexOutOfRange { slot: 3, index: 16, len: 16 });
        assert_eq!(dev.read_bool(Slot(3), 0).unwrap_err(), ApiError::TypeMismatch { slot: 3, index: 0, expected: Real, got: Bool });
        assert_eq!(dev.write_real(Slot(3), 0, 1.0).unwrap_err(), ApiError::WrongDirection { slot: 3, expected: Direction::Output });
        assert_eq!(dev.read_real(Slot(1), 0).unwrap_err(), ApiError::WrongDirection { slot: 1, expected: Direction::Input });
        dev.stop().unwrap();
    }

    #[test]
    fn writes_publish_the_whole_submodule_and_reads_decode_cpu_frames() {
        let (dev, eth) = started();
        wait_for(&dev, ArState::Data);
        // Group write: two fields of slot 1, one publish.
        dev.with_inputs(Slot(1), |w| { w.real(0, 1.0)?; w.real(15, -2.5) }).unwrap();
        dev.write_bool(Slot(2), 31, true).unwrap();
        let image = dev.image();
        let s1 = image.read_inputs_for_test(1); // see step 3: test-only accessor via rt_snapshot_inputs
        assert_eq!(&s1[..4], &[0x3F, 0x80, 0, 0]);
        assert_eq!(&s1[60..64], &[0xC0, 0x20, 0, 0]);
        // Inject a CPU frame carrying REAL 1.0 at slot 3 index 0 and bit 7 of slot 4 byte 3.
        let frame = cpu_frame_for(&dev, |csdu| {
            csdu[3 + 64 + 1 + 4 + 1 + 0 ..][..4].copy_from_slice(&[0x3F, 0x80, 0, 0]); // slot 3 data
            // layout of the Output CR: IOCS DAP ×3 at 0,1,2 ... use the runner's layout instead:
        });
        eth.push_rx(frame);
        std::thread::sleep(Duration::from_millis(30));
        let snap = dev.outputs(Slot(3)).unwrap();
        assert_eq!(snap.real(0).unwrap(), 1.0);
        assert_eq!(dev.read_real(Slot(3), 0).unwrap(), 1.0);
        assert_eq!(dev.freshness(), crate::rt::Freshness::Fresh);
        dev.stop().unwrap();
    }

    #[test]
    fn drop_without_stop_does_not_panic() {
        let (dev, _eth) = started();
        wait_for(&dev, ArState::Data);
        drop(dev);
    }
}
```
The frame-injection helper `cpu_frame_for` must build a valid `0x8001` RTC1 frame for the *actual* layout: read `dev.ar_params()` (expose `IoDevice::ar_params()` for tests, `#[doc(hidden)]`), compute `Layout::from_ar`, fill a C-SDU of `output_cr.data_length` bytes with IOPS/IOCS `0x80` at every object's `iops_off`/`iocs_off`, write the field bytes at `data_off + field.byte`, then `RtFrame { frame_id: 0x8001, cycle_counter: 1024, data_status: DataStatus(0x35), transfer_status: 0, csdu }.write(&mut buf, dst = DEV, src = CPU MAC from params.initiator_mac)`. Write the helper in the test module; the version sketched above with hard-coded offsets is **not** acceptable — derive the offsets from the layout. Also replace `image.read_inputs_for_test` by `let mut buf = vec![0u8; 80]; assert!(image.rt_snapshot_inputs(&mut buf));` and index with the layout's `input_cr` objects (`data_off` of slot 1).

- [ ] **Step 2: Run to verify they fail**

Run: `. "$HOME/.cargo/env" && cargo test -p pnio api:: 2>&1 | tail -5`
Expected: compile error.

- [ ] **Step 3: Implement `api.rs`**

```rust
//! Typed facade over `device::Device` + `rt::IoImage` (spec §6): start the device from
//! a [`DeviceConfig`] in one call, read the controller's outputs and write our inputs
//! by (slot, index) with the config's field table. The RT path is untouched.

use crate::cm::{AbortReason, ArParams, ArState};
use crate::config::{DeviceConfig, Direction, FieldRef, Slot};
use crate::data::{CodecError, FieldType, Value};
use crate::device::{Device, DeviceError, RtOptions};
use crate::eth::{bpf::acyclic_filter, AfPacketTransport, EthTransport, MacAddr};
use crate::rpc::{RpcTransport, UdpRpcTransport, PNIO_UDP_PORT};
use crate::rt::{Freshness, ImageError, IoImage, RtConfig, RtError, RtHandle, RtStats, StatsSnapshot, Validity};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartOptions {
    pub iface: String,
    pub ip: [u8; 4],
    pub rt: Option<RtOptions>,
    /// CPUs for the acyclic thread (and anything the application spawns from it).
    pub app_cpus: Option<Vec<usize>>,
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("slot {0} is not declared")]
    UnknownSlot(u16),
    #[error("slot {slot} index {index} out of range (len {len})")]
    IndexOutOfRange { slot: u16, index: usize, len: usize },
    #[error("slot {slot} index {index} is {expected:?}, not {got:?}")]
    TypeMismatch { slot: u16, index: usize, expected: FieldType, got: FieldType },
    #[error("slot {slot} has no {expected:?} data")]
    WrongDirection { slot: u16, expected: Direction },
    #[error("no I/O layout yet: the AR has not reached Data")]
    NoLayoutYet,
    #[error(transparent)]
    Image(ImageError),
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error("device error: {0}")]
    Device(#[from] DeviceError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
impl PartialEq for ApiError { /* compare Debug strings; Io/Device carry non-PartialEq payloads */
    fn eq(&self, o: &Self) -> bool { format!("{self:?}") == format!("{o:?}") }
}
impl From<ImageError> for ApiError {
    fn from(e: ImageError) -> Self {
        match e {
            ImageError::UnknownSubmodule { .. } => ApiError::NoLayoutYet,
            e => ApiError::Image(e),
        }
    }
}

struct Shared {
    state: Mutex<(ArState, Option<AbortReason>)>,
}

pub struct IoDevice {
    cfg: Arc<DeviceConfig>,
    image: Arc<IoImage>,
    stats: Arc<RtStats>,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    /// Working copy of each input submodule's bytes, index = position in `cfg.submodules()`.
    inputs: Vec<Mutex<Vec<u8>>>,
    thread: Mutex<Option<JoinHandle<Result<(), DeviceError>>>>,
    params: Arc<Mutex<Option<ArParams>>>,
}

impl IoDevice {
    pub fn start(cfg: DeviceConfig, opts: StartOptions) -> Result<IoDevice, ApiError> {
        let mac = read_mac(&opts.iface)?;
        let eth = AfPacketTransport::open(&opts.iface).map_err(io_err)?;
        eth.attach_filter(&acyclic_filter()).map_err(io_err)?;
        let rpc = UdpRpcTransport::bind(std::net::SocketAddr::from(([0, 0, 0, 0], PNIO_UDP_PORT)))
            .map_err(|e| ApiError::Io(std::io::Error::other(e.to_string())))?;
        let app_cpus = opts.app_cpus.clone();
        Self::start_inner(cfg, mac, opts.ip, opts.rt, eth, rpc, crate::rt::RtRunner::spawn, app_cpus)
    }

    /// Test/embedding hook: any transports, any runner factory.
    #[doc(hidden)]
    pub fn start_with<E, R>(cfg: DeviceConfig, mac: MacAddr, ip: [u8; 4], rt: Option<RtOptions>, eth: E, rpc: R,
        runner: impl Fn(RtConfig) -> Result<RtHandle, RtError> + Send + 'static) -> Result<IoDevice, ApiError>
    where E: EthTransport + 'static, R: RpcTransport + 'static {
        Self::start_inner(cfg, mac, ip, rt, eth, rpc, runner, None)
    }

    fn start_inner<E, R>(cfg: DeviceConfig, mac: MacAddr, ip: [u8; 4], rt: Option<RtOptions>, eth: E, rpc: R,
        runner: impl Fn(RtConfig) -> Result<RtHandle, RtError> + Send + 'static, app_cpus: Option<Vec<usize>>) -> Result<IoDevice, ApiError>
    where E: EthTransport + 'static, R: RpcTransport + 'static {
        let cfg = Arc::new(cfg);
        let mut dev = Device::new(cfg.setup(mac, ip, rt), eth, rpc);
        dev.with_runner_factory(runner);
        let shared = Arc::new(Shared { state: Mutex::new((ArState::Idle, None)) });
        let params = Arc::new(Mutex::new(None));
        {
            let shared = shared.clone();
            dev.on_state_change(move |st, why| {
                *shared.state.lock().unwrap_or_else(|e| e.into_inner()) = (st, why);
            });
        }
        let image = dev.image();
        let stats = dev.rt_stats();
        let stop = Arc::new(AtomicBool::new(false));
        let inputs = cfg.submodules().iter().map(|s| Mutex::new(vec![0u8; cfg.input_len(s.slot).unwrap_or(0) as usize])).collect();
        let thread = {
            let stop = stop.clone();
            let params = params.clone();
            std::thread::Builder::new().name("pnio-acyclic".into()).spawn(move || {
                if let Some(cpus) = app_cpus {
                    if let Err(e) = crate::rt::sched::set_affinity(&cpus) {
                        log::warn!("acyclic affinity {cpus:?}: {e}");
                    }
                }
                let r = run_publishing_params(&mut dev, &stop, &params);
                drop(dev); // stops and joins the RT runner before the thread ends
                r
            })?
        };
        Ok(IoDevice { cfg, image, stats, shared, stop, inputs, thread: Mutex::new(Some(thread)), params })
    }

    pub fn config(&self) -> &DeviceConfig { &self.cfg }
    pub fn image(&self) -> Arc<IoImage> { self.image.clone() }
    pub fn ar_state(&self) -> ArState { self.shared.state.lock().unwrap_or_else(|e| e.into_inner()).0 }
    pub fn last_abort(&self) -> Option<AbortReason> { self.shared.state.lock().unwrap_or_else(|e| e.into_inner()).1.clone() }
    #[doc(hidden)]
    pub fn ar_params(&self) -> Option<ArParams> { self.params.lock().unwrap_or_else(|e| e.into_inner()).clone() }
    pub fn validity(&self) -> Validity { self.image.validity() }
    pub fn freshness(&self) -> Freshness { self.image.validity().freshness() }
    pub fn stats(&self) -> StatsSnapshot { self.stats.snapshot() }
    pub fn rt_stats(&self) -> Arc<RtStats> { self.stats.clone() }

    // ----- lookups -----
    fn index_of(&self, slot: Slot) -> Result<usize, ApiError> {
        self.cfg.submodules().iter().position(|s| s.slot == slot).ok_or(ApiError::UnknownSlot(slot.0))
    }
    fn field(&self, slot: Slot, dir: Direction, index: usize, want: Option<FieldType>) -> Result<FieldRef, ApiError> {
        self.index_of(slot)?;
        let fields = self.cfg.fields(slot, dir).ok_or(ApiError::WrongDirection { slot: slot.0, expected: dir })?;
        let f = *fields.get(index).ok_or(ApiError::IndexOutOfRange { slot: slot.0, index, len: fields.len() })?;
        if let Some(w) = want { if w != f.ty { return Err(ApiError::TypeMismatch { slot: slot.0, index, expected: f.ty, got: w }); } }
        Ok(f)
    }

    // ----- controller -> device -----
    pub fn read(&self, slot: Slot, index: usize) -> Result<Value, ApiError> {
        let f = self.field(slot, Direction::Output, index, None)?;
        self.image.read_outputs(slot.0, 1, |b, _| Value::decode(f.ty, &b[f.byte as usize..], f.bit as usize))??.pipe_ok()
    }
    pub fn read_bool(&self, s: Slot, i: usize) -> Result<bool, ApiError> { self.typed(s, i, FieldType::Bool).map(|v| match v { Value::Bool(b) => b, _ => unreachable!() }) }
    pub fn read_int(&self, s: Slot, i: usize) -> Result<i16, ApiError> { self.typed(s, i, FieldType::Int).map(|v| match v { Value::Int(x) => x, _ => unreachable!() }) }
    pub fn read_word(&self, s: Slot, i: usize) -> Result<u16, ApiError> { self.typed(s, i, FieldType::Word).map(|v| match v { Value::Word(x) => x, _ => unreachable!() }) }
    pub fn read_dint(&self, s: Slot, i: usize) -> Result<i32, ApiError> { self.typed(s, i, FieldType::Dint).map(|v| match v { Value::Dint(x) => x, _ => unreachable!() }) }
    pub fn read_real(&self, s: Slot, i: usize) -> Result<f32, ApiError> { self.typed(s, i, FieldType::Real).map(|v| match v { Value::Real(x) => x, _ => unreachable!() }) }
    fn typed(&self, slot: Slot, index: usize, want: FieldType) -> Result<Value, ApiError> {
        let f = self.field(slot, Direction::Output, index, Some(want))?;
        let r = self.image.read_outputs(slot.0, 1, |b, _| Value::decode(f.ty, &b[f.byte as usize..], f.bit as usize))?;
        Ok(r?)
    }
    /// A consistent copy of one slot's output bytes plus the validity of that cycle.
    pub fn outputs(&self, slot: Slot) -> Result<SlotSnapshot, ApiError> {
        self.index_of(slot)?;
        let fields: Arc<[FieldRef]> = self.cfg.fields(slot, Direction::Output).ok_or(ApiError::WrongDirection { slot: slot.0, expected: Direction::Output })?.into();
        let (bytes, validity) = self.image.read_outputs(slot.0, 1, |b, v| (b.to_vec(), *v))?;
        Ok(SlotSnapshot { slot, bytes, validity, fields })
    }

    // ----- device -> controller -----
    pub fn write(&self, slot: Slot, index: usize, v: Value) -> Result<(), ApiError> {
        self.with_inputs(slot, |w| w.set(index, v))
    }
    pub fn write_bool(&self, s: Slot, i: usize, v: bool) -> Result<(), ApiError> { self.write(s, i, Value::Bool(v)) }
    pub fn write_int(&self, s: Slot, i: usize, v: i16) -> Result<(), ApiError> { self.write(s, i, Value::Int(v)) }
    pub fn write_word(&self, s: Slot, i: usize, v: u16) -> Result<(), ApiError> { self.write(s, i, Value::Word(v)) }
    pub fn write_dint(&self, s: Slot, i: usize, v: i32) -> Result<(), ApiError> { self.write(s, i, Value::Dint(v)) }
    pub fn write_real(&self, s: Slot, i: usize, v: f32) -> Result<(), ApiError> { self.write(s, i, Value::Real(v)) }
    /// Modify several fields of one input slot and publish them in one go (same frame).
    pub fn with_inputs<T>(&self, slot: Slot, f: impl FnOnce(&mut SlotWriter<'_>) -> Result<T, ApiError>) -> Result<T, ApiError> {
        let i = self.index_of(slot)?;
        let fields = self.cfg.fields(slot, Direction::Input).ok_or(ApiError::WrongDirection { slot: slot.0, expected: Direction::Input })?;
        let mut buf = self.inputs[i].lock().unwrap_or_else(|e| e.into_inner());
        let mut w = SlotWriter { slot, fields, bytes: &mut buf, dirty: false };
        let out = f(&mut w)?;
        self.image.write_inputs(slot.0, 1, &buf)?;
        Ok(out)
    }

    pub fn stop(self) -> Result<(), DeviceError> {
        self.stop.store(true, Ordering::Relaxed);
        let h = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        match h { Some(h) => h.join().unwrap_or(Ok(())), None => Ok(()) }
    }
}

impl Drop for IoDevice {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() { let _ = h.join(); }
    }
}

/// `Device::run` with the AR parameters mirrored into `params` on every state change.
fn run_publishing_params<E: EthTransport, R: RpcTransport>(dev: &mut Device<E, R>, stop: &AtomicBool, params: &Mutex<Option<ArParams>>) -> Result<(), DeviceError> {
    // Same loop as Device::run (200 ms poll), stepping so we can observe ar_params():
    use std::time::{Duration, Instant};
    let mut last = None;
    while !stop.load(Ordering::Relaxed) {
        dev.step(Instant::now(), Some(Duration::from_millis(200)))?;
        let p = dev.ar_params();
        if p != last { *params.lock().unwrap_or_else(|e| e.into_inner()) = p.clone(); last = p; }
    }
    Ok(())
}

pub struct SlotSnapshot { pub slot: Slot, bytes: Vec<u8>, pub validity: Validity, fields: Arc<[FieldRef]> }
impl SlotSnapshot {
    pub fn get(&self, index: usize) -> Result<Value, ApiError> {
        let f = *self.fields.get(index).ok_or(ApiError::IndexOutOfRange { slot: self.slot.0, index, len: self.fields.len() })?;
        Ok(Value::decode(f.ty, &self.bytes[f.byte as usize..], f.bit as usize)?)
    }
    pub fn real(&self, i: usize) -> Result<f32, ApiError> { match self.get(i)? { Value::Real(v) => Ok(v), v => Err(self.mismatch(i, FieldType::Real, v)) } }
    pub fn bool(&self, i: usize) -> Result<bool, ApiError> { match self.get(i)? { Value::Bool(v) => Ok(v), v => Err(self.mismatch(i, FieldType::Bool, v)) } }
    pub fn int(&self, i: usize) -> Result<i16, ApiError> { match self.get(i)? { Value::Int(v) => Ok(v), v => Err(self.mismatch(i, FieldType::Int, v)) } }
    pub fn word(&self, i: usize) -> Result<u16, ApiError> { match self.get(i)? { Value::Word(v) => Ok(v), v => Err(self.mismatch(i, FieldType::Word, v)) } }
    pub fn dint(&self, i: usize) -> Result<i32, ApiError> { match self.get(i)? { Value::Dint(v) => Ok(v), v => Err(self.mismatch(i, FieldType::Dint, v)) } }
    pub fn bytes(&self) -> &[u8] { &self.bytes }
    fn mismatch(&self, index: usize, got: FieldType, v: Value) -> ApiError { ApiError::TypeMismatch { slot: self.slot.0, index, expected: v.field_type(), got } }
}

pub struct SlotWriter<'a> { slot: Slot, fields: &'a [FieldRef], bytes: &'a mut Vec<u8>, dirty: bool }
impl SlotWriter<'_> {
    pub fn set(&mut self, index: usize, v: Value) -> Result<(), ApiError> {
        let f = *self.fields.get(index).ok_or(ApiError::IndexOutOfRange { slot: self.slot.0, index, len: self.fields.len() })?;
        if f.ty != v.field_type() { return Err(ApiError::TypeMismatch { slot: self.slot.0, index, expected: f.ty, got: v.field_type() }); }
        v.encode(&mut self.bytes[f.byte as usize..], f.bit as usize)?;
        self.dirty = true;
        Ok(())
    }
    pub fn bool(&mut self, i: usize, v: bool) -> Result<(), ApiError> { self.set(i, Value::Bool(v)) }
    pub fn int(&mut self, i: usize, v: i16) -> Result<(), ApiError> { self.set(i, Value::Int(v)) }
    pub fn word(&mut self, i: usize, v: u16) -> Result<(), ApiError> { self.set(i, Value::Word(v)) }
    pub fn dint(&mut self, i: usize, v: i32) -> Result<(), ApiError> { self.set(i, Value::Dint(v)) }
    pub fn real(&mut self, i: usize, v: f32) -> Result<(), ApiError> { self.set(i, Value::Real(v)) }
}

fn read_mac(iface: &str) -> Result<MacAddr, ApiError> {
    let s = std::fs::read_to_string(format!("/sys/class/net/{iface}/address"))?;
    let mut m = [0u8; 6];
    for (i, p) in s.trim().split(':').enumerate().take(6) {
        m[i] = u8::from_str_radix(p, 16).map_err(|_| std::io::Error::other(format!("bad mac {s:?}")))?;
    }
    Ok(MacAddr(m))
}
fn io_err(e: crate::eth::TransportError) -> ApiError { ApiError::Io(std::io::Error::other(e.to_string())) }
```
Notes for the implementer: (1) `read` above has a bogus `.pipe_ok()` — write it like `typed` (`let r = self.image.read_outputs(..)?; Ok(r?)`). (2) `Device::step`'s signature is `step(&mut self, now: Instant, timeout: Option<Duration>) -> Result<StepReport, DeviceError>` — check and adapt; if `Device::run` cannot be reproduced with `step` (e.g. `run` does extra work), add a `#[doc(hidden)] pub fn run_with(&mut self, stop, on_step: impl FnMut(&Self))` to `device` instead — the smallest change, documented in the report. (3) `ArParams: PartialEq + Clone` (verify; add derives if missing — logic-free change in `cm`). (4) `Device` is `Send` when `E: Send + Sync`, `R: Send` — the mocks are; if the compiler disagrees, box the runner factory as `Device` already does. (5) The `SharedMock` in the tests must be the same type used by `spawn_with_transport` — the `EthTransport` impl above forwards both methods. `lib.rs`: `pub mod api;`.

- [ ] **Step 4: Run, fmt, clippy**

Run: `. "$HOME/.cargo/env" && cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test -p pnio 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: green; the 4 api tests pass, everything else unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/pnio/src/api.rs crates/pnio/src/lib.rs crates/pnio/src/device crates/pnio/src/cm
git commit -m "feat(api): IoDevice facade — typed reads/writes and per-slot consistent snapshots over Device + IoImage"
```

---

### Task 6: `typed_replay` integration test + `gen_gsdml` and `typed_bringup` examples

**Files:**
- Create: `crates/pnio/tests/typed_replay.rs`, `crates/pnio/examples/gen_gsdml.rs`, `crates/pnio/examples/typed_bringup.rs`
- Modify: `crates/pnio/Cargo.toml` (`[[example]] name = "typed_bringup" test = true`)

**Interfaces:**
- Consumes: everything from Tasks 2-5; `tests/common::{golden, synthetic_connect_req, RPC_OFF}`; `rt::{RtFrame, DataStatus, Layout}`; `rt_bringup.rs` as the model for flags, CSV, verdict (copy the `Args` fields `--iface --ip --rt-priority --cpu --app-cpus --lock-memory --duration --stats-every --csv --max-lateness-us --p9999-lateness-us --max-rx-interval-us`, `parse_cpu_list`, `verdict`, `write_hist_csv`, signal handling — duplicated on purpose, each example stays standalone).

- [ ] **Step 1: `tests/typed_replay.rs`**

```rust
//! End-to-end with the typed config: synthetic Connect → Data, a fabricated CPU frame for
//! the 16 REAL + 32 BOOL layout decoded through IoDevice, our inputs published and
//! visible in the produced frame.
mod common;
use common::{golden, synthetic_connect_req, RPC_OFF};
use pnio::api::IoDevice;
use pnio::cm::ArState;
use pnio::config::{DeviceConfig, Slot};
use pnio::data::FieldType::*;
use pnio::eth::{EthTransport, MacAddr, MockTransport, TransportError};
use pnio::rpc::MockRpcTransport;
use pnio::rt::{DataStatus, Layout, RtFrame, RtRunner};
use std::sync::Arc;
use std::time::Duration;

const DEV: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);
const CPU: MacAddr = MacAddr([0xec, 0x1c, 0x5d, 0x61, 0xe7, 0x3f]);

struct SharedMock(Arc<MockTransport>);
impl EthTransport for SharedMock {
    fn send(&self, f: &[u8]) -> Result<(), TransportError> { self.0.send(f) }
    fn recv_into(&self, b: &mut [u8], t: Option<Duration>) -> Result<Option<usize>, TransportError> { self.0.recv_into(b, t) }
}

fn sample() -> DeviceConfig {
    DeviceConfig::builder("pnio-dev")
        .input(Slot(1), &[Real; 16]).input(Slot(2), &[Bool; 32])
        .output(Slot(3), &[Real; 16]).output(Slot(4), &[Bool; 32])
        .build().unwrap()
}

#[test]
fn typed_round_trip_with_the_sample_config() {
    let cfg = sample();
    let model = cfg.model(DEV);
    let rpc = MockRpcTransport::new();
    let cpu = "172.16.2.100:54766".parse().unwrap();
    let cpu_cm = "172.16.2.100:34964".parse().unwrap();
    rpc.push_rx(synthetic_connect_req(&model), cpu);
    rpc.push_rx(golden("write_req")[RPC_OFF..].to_vec(), cpu);
    rpc.push_rx(golden("prmend_req")[RPC_OFF..].to_vec(), cpu);
    rpc.push_rx(golden("appready_res")[RPC_OFF..].to_vec(), cpu_cm);
    let eth = Arc::new(MockTransport::new());
    let eth2 = eth.clone();
    let dev = IoDevice::start_with(cfg.clone(), DEV, [172, 16, 2, 10], None, SharedMock(eth.clone()), rpc,
        move |c| RtRunner::spawn_with_transport(c, SharedMock(eth2.clone()))).unwrap();
    let t0 = std::time::Instant::now();
    while dev.ar_state() != ArState::Data { assert!(t0.elapsed() < Duration::from_secs(2)); std::thread::sleep(Duration::from_millis(5)); }

    // Our inputs: REAL 1.0 at slot 1 index 0, -2.5 at index 15, bits 0 and 31 of slot 2.
    dev.with_inputs(Slot(1), |w| { w.real(0, 1.0)?; w.real(15, -2.5) }).unwrap();
    dev.with_inputs(Slot(2), |w| { w.bool(0, true)?; w.bool(31, true) }).unwrap();

    // A CPU frame: REAL 1.0 at slot 3 index 0, -2.5 at index 15, bit 7 of slot 4 byte 3 (index 31).
    let params = dev.ar_params().unwrap();
    let layout = Layout::from_ar(&params, &model).unwrap();
    let mut csdu = vec![0u8; layout.output_cr.data_length as usize];
    for o in &layout.output_cr.objects { csdu[o.iops_off as usize] = 0x80; }
    for c in &layout.output_cr.iocs { csdu[c.iocs_off as usize] = 0x80; }
    let s3 = layout.output_cr.objects.iter().find(|o| o.slot == 3).unwrap().data_off as usize;
    csdu[s3..s3 + 4].copy_from_slice(&[0x3F, 0x80, 0, 0]);
    csdu[s3 + 60..s3 + 64].copy_from_slice(&[0xC0, 0x20, 0, 0]);
    let s4 = layout.output_cr.objects.iter().find(|o| o.slot == 4).unwrap().data_off as usize;
    csdu[s4 + 3] = 0x80;
    let mut buf = vec![0u8; 1522];
    let n = RtFrame { frame_id: 0x8001, cycle_counter: 1024, data_status: DataStatus(0x35), transfer_status: 0, csdu: &csdu }
        .write(&mut buf, DEV, CPU).unwrap();
    eth.push_rx(buf[..n].to_vec());
    std::thread::sleep(Duration::from_millis(50));

    let snap = dev.outputs(Slot(3)).unwrap();
    assert_eq!(snap.real(0).unwrap(), 1.0);
    assert_eq!(snap.real(15).unwrap(), -2.5);
    assert!(dev.read_bool(Slot(4), 31).unwrap());
    assert!(!dev.read_bool(Slot(4), 30).unwrap());

    // The frames we produced carry our inputs at the Input CR offsets.
    let sent = eth.sent();
    let frame = sent.iter().rev().find(|f| f.len() > 20 && f[16..18] == [0x88, 0x92] || f[12..14] == [0x88, 0x92]).unwrap();
    let csdu_off = if frame[12..14] == [0x81, 0x00] { 20 } else { 16 };
    let s1 = layout.input_cr.objects.iter().find(|o| o.slot == 1).unwrap().data_off as usize;
    assert_eq!(&frame[csdu_off + s1..csdu_off + s1 + 4], &[0x3F, 0x80, 0, 0]);
    let s2 = layout.input_cr.objects.iter().find(|o| o.slot == 2).unwrap().data_off as usize;
    assert_eq!(frame[csdu_off + s2], 0x01);
    assert_eq!(frame[csdu_off + s2 + 3], 0x80);
    dev.stop().unwrap();
}
```
(Check `RtFrame::write`'s exact signature and the field names of `CrLayout`/`IoObject`/`CsObject` in `rt/layout.rs` — `data_off`, `data_len`, `iops_off`, `iocs_off` — and adapt.)

- [ ] **Step 2: `examples/gen_gsdml.rs`**

```rust
//! Write the GSDML of the sample configuration (16 REAL + 32 BOOL per direction) and print
//! the resulting controller address map.
use clap::Parser;
use pnio::config::{DeviceConfig, Direction, Slot};
use pnio::data::FieldType::*;
use pnio::gsdml::{file_name, render, GsdmlMeta};

#[derive(Parser)]
struct Args {
    /// Output directory
    #[arg(long, default_value = ".")]
    out: std::path::PathBuf,
    /// Station name
    #[arg(long, default_value = "pnio-dev")]
    station: String,
    /// Vendor ID (development default, not a PI-assigned ID)
    #[arg(long, default_value_t = 0xFFFF)]
    vendor_id: u16,
    #[arg(long, default_value_t = 0x0001)]
    device_id: u16,
    /// MinDeviceInterval in 31.25 µs units: 32 = 1 ms, 16 = 500 µs
    #[arg(long, default_value_t = 32)]
    interval: u16,
}

pub fn sample_config(a: &Args) -> DeviceConfig {
    DeviceConfig::builder(&a.station)
        .station_type("pnio sample device")
        .identity(a.vendor_id, a.device_id)
        .min_device_interval(a.interval)
        .input(Slot(1), &[Real; 16])
        .input(Slot(2), &[Bool; 32])
        .output(Slot(3), &[Real; 16])
        .output(Slot(4), &[Bool; 32])
        .build()
        .expect("sample config is valid")
}

fn main() {
    let a = Args::parse();
    let cfg = sample_config(&a);
    let meta = GsdmlMeta {
        vendor_name: "Core Engineering".into(),
        product_family: "pnio".into(),
        info_text: "pnio sample device: 16 REAL + 32 BOOL per direction (development identity)".into(),
        order_number: "PNIO-SAMPLE".into(),
        date: (2026, 8, 29),
    };
    let path = a.out.join(file_name(&meta));
    std::fs::write(&path, render(&cfg, &meta)).expect("write gsdml");
    println!("wrote {}", path.display());
    println!("slot  dir     bytes  fields");
    let (mut ib, mut qb) = (0u32, 0u32);
    for s in cfg.submodules() {
        for (dir, len, base, tag) in [
            (Direction::Input, cfg.input_len(s.slot).unwrap_or(0), &mut ib, "%IB"),
            (Direction::Output, cfg.output_len(s.slot).unwrap_or(0), &mut qb, "%QB"),
        ] {
            if len == 0 { continue; }
            let n = cfg.fields(s.slot, dir).map(|f| f.len()).unwrap_or(0);
            println!("{:<5} {:<7} {:<6} {} fields -> {tag}{}..{}", s.slot.0, format!("{dir:?}"), len, n, *base, *base + len as u32 - 1);
            *base += len as u32;
        }
    }
    println!("(controller addresses assume TIA packs the modules in slot order from 0; check the device view)");
}
```
Add `[[example]] name = "gen_gsdml"` is not needed (no tests inside).

- [ ] **Step 3: `examples/typed_bringup.rs`**

Copy `examples/rt_bringup.rs` and change: the doc comment (typed sample config, our GSDML, station `pnio-dev`); replace the hand-built `DeviceSetup`/`AfPacketTransport`/`UdpRpcTransport`/`Device::new`/`on_state_change`/app-thread plumbing by:
```rust
    let cfg = sample_config(&a.station);             // same builder as gen_gsdml (inline copy)
    let dev = IoDevice::start(cfg, StartOptions {
        iface: a.iface.clone(), ip: a.ip.octets(),
        rt: Some(RtOptions { iface: a.iface.clone(), cpu_pin: a.cpu, rt_priority: a.rt_priority, lock_memory: a.lock_memory }),
        app_cpus: app_cpus.clone(),
    }).expect("start (need cap_net_raw/cap_net_admin/cap_sys_nice/cap_ipc_lock)");
```
and the application loop (1 ms) becomes the typed mirror:
```rust
fn run_app_cycle(dev: &IoDevice) -> Result<(), ApiError> {
    match dev.outputs(Slot(3)) {
        Ok(snap) => dev.with_inputs(Slot(1), |w| { for i in 0..16 { w.real(i, snap.real(i)?)?; } Ok(()) })?,
        Err(ApiError::NoLayoutYet) => return Ok(()),
        Err(e) => return Err(e),
    }
    let bits = dev.outputs(Slot(4))?;
    dev.with_inputs(Slot(2), |w| { for i in 0..32 { w.bool(i, bits.bool(i)?)?; } Ok(()) })
}
```
Stats/CSV/verdict read `dev.stats()` / `dev.rt_stats()` / `dev.freshness()`; the AR state log comes from polling `dev.ar_state()` every loop iteration and logging on change. Add `--station` (default `pnio-dev`). At the end: `let stats = dev.rt_stats(); let r = dev.stop(); … verdict(&stats, …)`. Keep `parse_cpu_list` and its test (`[[example]] name = "typed_bringup" test = true`).

- [ ] **Step 4: Run everything**

Run: `. "$HOME/.cargo/env" && cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all 2>&1 | grep -E "^test result|FAILED|panicked" && cargo build -q --release --target x86_64-unknown-linux-musl --example typed_bringup --example gen_gsdml && cargo run -q --example gen_gsdml -- --out /tmp/claude-1000 2>&1 | head -8 && cargo package -q --allow-dirty -p pnio && echo PACKAGE_OK`
Expected: green; `typed_replay` passes; both examples build for musl; `gen_gsdml` prints the map (`1 Input 64 16 fields -> %IB0..63`, `2 Input 4 32 fields -> %IB64..67`, `3 Output 64 … -> %QB0..63`, `4 Output 4 … -> %QB64..67`).

- [ ] **Step 5: Commit**

```bash
git add crates/pnio/tests/typed_replay.rs crates/pnio/examples/gen_gsdml.rs crates/pnio/examples/typed_bringup.rs crates/pnio/Cargo.toml
git commit -m "test(api): typed end-to-end replay; examples gen_gsdml and typed_bringup"
```

---

### Task 7: HIL with our GSDML, docs (controller + user)

**Files:**
- Create: `docs/gsdml.md`
- Modify: `docs/bench-pnet-device.md` (§6g before §7; §7 updated), `README.md`, `FOLLOWUPS.md`, the spec status line

- [ ] **Step 1: Generate + deploy**: `cargo run --example gen_gsdml -- --out captures/ --interval 16` (declares 500 µs for the bonus; 1 ms stays the criterion); musl build of `typed_bringup`, `scp` to `~/bench/`, user runs `setcap cap_net_raw,cap_net_admin,cap_sys_nice,cap_ipc_lock+eip ~/bench/typed_bringup`.
- [ ] **Step 2: TIA (user)**: import `captures/GSDML-V2.4-CoreEngineering-pnio-20260829.xml`; add the device from *Other field devices → PROFINET IO → I/O → Core Engineering → pnio*; name `pnio-dev`, IP 172.16.2.10; modules `in1`..`out4` land in slots 1-4 (fixed); update time 1 ms; check the device view addresses against `gen_gsdml`'s map (`%IB0..63`, `%IB64..67`, `%QB0..63`, `%QB64..67`); download; keep the rt-labs device object in the project (disabled or on a second device) for `rt_bringup` regression.
- [ ] **Step 3: Runs**: `typed_bringup --iface eno2 --ip 172.16.2.10 --rt-priority 80 --cpu 3 --app-cpus 0-1 --lock-memory --duration 60` smoke → `Data`, device green; watch table `%QD0 := 1.0 → %ID0`, `%QD60 := -2.5 → %ID60`, `%Q64.0 → %I64.0`, `%Q67.7 → %I67.7`; then `--duration 600 --csv logs/plan6-1ms.csv` → `VERDICT: PASS`; STOP→RUN; diagnostic buffer; bonus: TIA update time 0.5 ms, `--duration 300 --csv logs/plan6-500us.csv` (record the numbers whatever the verdict; not a criterion).
- [ ] **Step 4: Docs**: `docs/gsdml.md` (what the generator emits, the layout rule with the worked example, declaration → TIA address, import steps, the identity caveat, `--interval 16`); bench §6g (GSDML import, addresses, watch table, 1 ms run table, STOP→RUN, bonus 500 µs, lessons); README (Status: `config`/`gsdml`/`api` ✅ with the HIL date; Quick Start rewritten on `DeviceConfig::builder` + `IoDevice::start` + `gen_gsdml`; identity warning); FOLLOWUPS (Plan 6 items ✅; new: GSDML alarms/I&M when Plan 5 lands, `IsochroneMode`, application config file, official Vendor ID, `ProfinetDevice` facade); spec status line.
- [ ] **Step 5: Commit**: `docs: Plan 6 HIL — our GSDML on the S7-1500, typed round trip at 1 ms; gsdml.md; README quick start`.

---

## Self-review

- **Spec coverage**: §4 → Tasks 2-3; §5 → Task 4; §6 → Task 5; §7 → Tasks 6-7; §8 tests → each task's Step 1 + Task 6; §9 errors → `ConfigError` (Task 2), `ApiError`/`NoLayoutYet` (Task 5); §10 docs → Task 7; §11 deps → Task 4 (`roxmltree` dev only); §12 roles → Task 7.
- **Type consistency**: `layout(&[FieldType]) -> (Vec<FieldRef>, u16)` used by Tasks 2 and 4 (`items()` consumes `&[FieldRef]`); `DeviceConfig::{fields, field, input_len, output_len, submodules, model, setup}` used by Tasks 4-6 with the same signatures; `synthetic_connect_req(&DeviceModel) -> Vec<u8>` (Task 3) consumed by Tasks 3, 5, 6; `IoDevice::start_with(cfg, mac, ip, rt, eth, rpc, runner)` identical in Task 5 tests and Task 6; `SlotSnapshot::{real,bool,int,word,dint,get}` and `SlotWriter::{real,bool,int,word,dint,set}` used by Task 6's mirror.
- **Placeholders**: none; the two spots in Task 5's test sketch explicitly flagged as "not acceptable as written" carry the replacement instruction (derive offsets from the layout; use `rt_snapshot_inputs`).

//! Typed device configuration: the single source from which the device model, the
//! per-field byte/bit table, the DCP identity and the GSDML are derived (spec §4).

use crate::cm::{DeviceModel, SlotModel, SubmoduleModel};
use crate::data::FieldType;
use crate::dcp::{DcpConfig, DeviceProperties};
use crate::device::{DeviceSetup, RtOptions};
use crate::eth::MacAddr;
use crate::im::{Im0, ImError};
use crate::rpc::Uuid;
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
    /// Device → controller (the controller's `%I`).
    Input,
    /// Controller → device (the controller's `%Q`).
    Output,
    /// Both: the submodule declares fields in each direction.
    InputOutput,
}

/// One submodule: an ordered list of input fields and/or output fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmoduleSpec {
    /// Slot this submodule is plugged into.
    pub slot: Slot,
    /// Name reported in the model/GSDML; auto-generated (`in<slot>`/`out<slot>`) by
    /// [`DeviceConfigBuilder::input`]/[`DeviceConfigBuilder::output`], or given
    /// explicitly to [`DeviceConfigBuilder::submodule`].
    pub name: String,
    /// Input fields (device → controller), in declaration order.
    pub inputs: Vec<FieldType>,
    /// Output fields (controller → device), in declaration order.
    pub outputs: Vec<FieldType>,
}

impl SubmoduleSpec {
    /// [`Direction::Input`] if only `inputs` is non-empty, [`Direction::Output`] if
    /// only `outputs` is, [`Direction::InputOutput`] otherwise (including the
    /// pathological case where both are empty).
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
    /// Byte offset within the submodule's data.
    pub byte: u16,
    /// Bit offset within `byte`, LSB-first; always `0` for byte-typed fields.
    pub bit: u8,
    /// The field's type.
    pub ty: FieldType,
}

/// Errors from [`DeviceConfigBuilder::build`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    /// A submodule was declared at slot 0, which is reserved for the DAP.
    #[error("slot 0 is the DAP and cannot carry process data")]
    SlotZeroReserved,
    /// Two submodules were declared at the same slot.
    #[error("slot {0} declared twice")]
    DuplicateSlot(u16),
    /// A submodule was declared with no input fields and no output fields.
    #[error("slot {0} has neither inputs nor outputs")]
    EmptySubmodule(u16),
    /// The builder has no submodule at all.
    #[error("no submodule declared")]
    NoSubmodule,
    /// One submodule's byte layout, in one direction, exceeds
    /// [`MAX_SUBMODULE_BYTES`].
    #[error("slot {slot}: {bytes} bytes exceed the {max}-byte submodule limit")]
    TooLong {
        /// Slot that overflowed.
        slot: u16,
        /// Its byte length in that direction.
        bytes: u32,
        /// The limit, i.e. [`MAX_SUBMODULE_BYTES`].
        max: u16,
    },
    /// The station name fails the PROFINET name-of-station rule — see
    /// `normalize_station_name`'s doc for the exact grammar.
    #[error("station name {0:?} is not a valid PROFINET name of station")]
    BadStationName(String),
    /// [`DeviceConfigBuilder::min_device_interval`] was given a value other than 16
    /// or 32.
    #[error("min device interval {0} is not one of 16, 32")]
    BadInterval(u16),
    /// [`DeviceConfigBuilder::identity`] was given a zero `vendor_id`.
    #[error("vendor id must be non-zero")]
    BadIdentity,
    /// The sum of all submodules' C-SDU, in one direction (including the DAP/IOPS/IOCS
    /// overhead — see `cr_lengths`), exceeds the 1440-byte RT frame budget even though
    /// every individual submodule fit under [`MAX_SUBMODULE_BYTES`].
    #[error(
        "total {direction:?} C-SDU is {bytes} bytes, exceeding the {max}-byte RT frame budget"
    )]
    TooLongTotal {
        /// The direction that overflowed.
        direction: Direction,
        /// Its total C-SDU length.
        bytes: u32,
        /// The limit, i.e. [`MAX_SUBMODULE_BYTES`].
        max: u16,
    },
    /// The declared or defaulted I&M0 identity failed [`Im0::validate`].
    #[error("invalid I&M0 identity: {0}")]
    Im(#[from] ImError),
}

/// Lay out `fields` per the declaration-order rule: `Bool`s pack 8 per byte
/// (LSB-first), a `Bool` after a byte-typed field opens a new byte, byte types are
/// placed back-to-back big-endian with no padding. Returns the refs and the byte
/// length.
///
/// This is the raw packing rule with no size limit of its own — meaningful as a
/// submodule's actual on-wire layout only once its byte length has been checked
/// against [`MAX_SUBMODULE_BYTES`], which [`DeviceConfigBuilder::build`] does via
/// `checked_layout` before ever storing a [`FieldRef`] table. Called directly (as
/// the tests below do) it will happily lay out an oversized field list.
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
                refs.push(FieldRef {
                    byte: byte as u16,
                    bit,
                    ty,
                });
            }
            Some(n) => {
                bit_byte = None;
                refs.push(FieldRef {
                    byte: next_byte as u16,
                    bit: 0,
                    ty,
                });
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
    im0: Im0,
}

/// Accumulates a device declaration (station name, identity, interval, submodules)
/// for [`DeviceConfigBuilder::build`] to validate and derive into a [`DeviceConfig`].
/// Nothing here is checked until `build()` runs — a builder can be composed freely
/// (any order, any number of `input`/`output`/`submodule` calls) and only fails, all
/// at once, on `build()`.
pub struct DeviceConfigBuilder {
    station_name: String,
    station_type: String,
    vendor_id: u16,
    device_id: u16,
    min_device_interval: u16,
    submodules: Vec<SubmoduleSpec>,
    /// `None` until [`DeviceConfigBuilder::im0`] is called: `build()` then derives
    /// `order_id` from `station_type` (spec §5.4) instead of taking
    /// [`Im0::default`]'s placeholder.
    im0: Option<Im0>,
}

impl DeviceConfig {
    /// Starts a builder with the given station name and every other field at its
    /// default (station type `"pnio device"`, vendor/device id `0xFFFF`/`0x0001`,
    /// 1 ms `min_device_interval`, no submodules).
    pub fn builder(station_name: &str) -> DeviceConfigBuilder {
        DeviceConfigBuilder {
            station_name: station_name.to_string(),
            station_type: "pnio device".to_string(),
            vendor_id: 0xFFFF,
            device_id: 0x0001,
            min_device_interval: 32,
            submodules: Vec::new(),
            im0: None,
        }
    }

    /// The normalized station name (see `normalize_station_name`), as answered on the
    /// wire by DCP.
    pub fn station_name(&self) -> &str {
        &self.station_name
    }
    /// The station type ([`DeviceConfigBuilder::station_type`]).
    pub fn station_type(&self) -> &str {
        &self.station_type
    }
    /// The PROFINET vendor ID.
    pub fn vendor_id(&self) -> u16 {
        self.vendor_id
    }
    /// The PROFINET device ID.
    pub fn device_id(&self) -> u16 {
        self.device_id
    }
    /// The AR's cyclic update time, in units of 31.25 µs — see
    /// [`DeviceConfigBuilder::min_device_interval`].
    pub fn min_device_interval(&self) -> u16 {
        self.min_device_interval
    }
    /// Every declared submodule, sorted by slot.
    pub fn submodules(&self) -> &[SubmoduleSpec] {
        &self.submodules
    }
    /// The device's I&M0 identity.
    pub fn im0(&self) -> &Im0 {
        &self.im0
    }

    fn index_of(&self, slot: Slot) -> Option<usize> {
        self.submodules.iter().position(|s| s.slot == slot)
    }

    /// The declared submodule at `slot`, if any.
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

    /// The [`FieldRef`] at `index` in one direction of a slot, or `None` if the slot,
    /// direction or index doesn't resolve — see [`DeviceConfig::fields`].
    pub fn field(&self, slot: Slot, dir: Direction, index: usize) -> Option<FieldRef> {
        self.fields(slot, dir)?.get(index).copied()
    }

    /// The slot's input byte length (device → controller), or `None` if the slot
    /// isn't declared. `0` if declared with no input fields.
    pub fn input_len(&self, slot: Slot) -> Option<u16> {
        self.index_of(slot).map(|i| self.derived[i].input_len)
    }

    /// The slot's output byte length (controller → device), or `None` if the slot
    /// isn't declared. `0` if declared with no output fields.
    pub fn output_len(&self, slot: Slot) -> Option<u16> {
        self.index_of(slot).map(|i| self.derived[i].output_len)
    }

    /// Input CR C-SDU length including the IOPS/IOCS bytes, as the controller will
    /// compute it; ≤ 1440 by construction.
    pub fn input_cr_len(&self) -> u16 {
        cr_lengths(&self.submodules, &self.derived).0 as u16
    }

    /// Output CR C-SDU length including the IOPS/IOCS bytes, as the controller will
    /// compute it; ≤ 1440 by construction.
    pub fn output_cr_len(&self) -> u16 {
        cr_lengths(&self.submodules, &self.derived).1 as u16
    }

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
            submodules: vec![
                sm(1, 0x1, 0, 0),
                sm(0x8000, 0x8000, 0, 0),
                sm(0x8001, 0x8001, 0, 0),
            ],
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
        let mut b = [
            0x14, 0xaf, 0x19, 0x8a, 0x12, 0x34, 0x10, 0x56, 0x80, 0x79, 0, 0, 0, 0, 0, 0,
        ];
        b[10..].copy_from_slice(&mac.0);
        Uuid(b)
    }

    /// Everything `device::Device::new` needs.
    ///
    /// `im0.serial_number` is filled from `mac`'s last three octets
    /// (`PNIO-<XXYYZZ>`) when the config leaves it blank, so a device configured
    /// without an explicit serial still answers I&M0 with something MAC-derived
    /// rather than an empty field.
    pub fn setup(&self, mac: MacAddr, ip: [u8; 4], rt: Option<RtOptions>) -> DeviceSetup {
        let mut im0 = self.im0.clone();
        if im0.serial_number.is_empty() {
            im0.serial_number = format!("PNIO-{:02X}{:02X}{:02X}", mac.0[3], mac.0[4], mac.0[5]);
        }
        DeviceSetup {
            dcp: DcpConfig {
                mac,
                properties: self.dcp_properties(ip),
            },
            model: self.model(mac),
            activity_seed: Self::activity_seed(mac),
            rt,
            im0,
            im_store: None,
        }
    }
}

impl DeviceConfigBuilder {
    /// The `TypeOfStation` reported by DCP (default `"pnio device"`); also the source
    /// of the default I&M0 `OrderID` — see [`DeviceConfigBuilder::im0`].
    pub fn station_type(mut self, s: &str) -> Self {
        self.station_type = s.to_string();
        self
    }
    /// PROFINET vendor/device ID pair (default `0xFFFF`/`0x0001`). `build()` rejects a
    /// zero `vendor_id` with [`ConfigError::BadIdentity`].
    pub fn identity(mut self, vendor_id: u16, device_id: u16) -> Self {
        self.vendor_id = vendor_id;
        self.device_id = device_id;
        self
    }
    /// The AR's cyclic update time, in units of 31.25 µs: `16` = 500 µs, `32` = 1 ms
    /// (the default). No other value is accepted — `8` would need a busy-poll device
    /// we don't implement, and `64`/`128` are not send clocks this crate tests or
    /// declares in the GSDML.
    pub fn min_device_interval(mut self, v: u16) -> Self {
        self.min_device_interval = v;
        self
    }
    /// I&M0 device identity: order number, serial, hardware/software revision.
    /// Validated by [`ImError`] on `build()`.
    ///
    /// Left unset, `build()` takes [`Im0::default`] but replaces its placeholder
    /// `order_id` with [`DeviceConfigBuilder::station_type`] truncated to 20 ASCII
    /// bytes (spec §5.4), so a device that declares only its type still reports that
    /// type as its I&M0 `OrderID` and in the GSDML's `ModuleInfo`. [`Im0::default`]
    /// itself is unchanged for callers who build an `Im0` directly.
    pub fn im0(mut self, im0: Im0) -> Self {
        self.im0 = Some(im0);
        self
    }
    /// Device → controller data in `slot` (the controller's inputs).
    pub fn input(self, slot: Slot, fields: &[FieldType]) -> Self {
        self.submodule(slot, &format!("in{}", slot.0), fields, &[])
    }
    /// Controller → device data in `slot` (the controller's outputs).
    pub fn output(self, slot: Slot, fields: &[FieldType]) -> Self {
        self.submodule(slot, &format!("out{}", slot.0), &[], fields)
    }
    /// Declares one submodule at `slot` with both input and output fields (the
    /// general form of [`DeviceConfigBuilder::input`]/[`DeviceConfigBuilder::output`],
    /// for a mixed-direction submodule).
    pub fn submodule(
        mut self,
        slot: Slot,
        name: &str,
        inputs: &[FieldType],
        outputs: &[FieldType],
    ) -> Self {
        self.submodules.push(SubmoduleSpec {
            slot,
            name: name.to_string(),
            inputs: inputs.to_vec(),
            outputs: outputs.to_vec(),
        });
        self
    }

    /// Validates the declaration and derives the per-submodule field tables. Runs, in
    /// order: the station-name rule, `min_device_interval`, `vendor_id != 0`, at
    /// least one submodule, then per submodule (in ascending slot order) slot != 0,
    /// no duplicate slot, not empty, each direction's byte layout within
    /// [`MAX_SUBMODULE_BYTES`] — and finally, once every submodule's layout is known,
    /// the *total* C-SDU guard ([`ConfigError::TooLongTotal`]) against the same
    /// 1440-byte RT frame budget, per direction across all submodules.
    pub fn build(self) -> Result<DeviceConfig, ConfigError> {
        let station_name = normalize_station_name(&self.station_name)
            .ok_or_else(|| ConfigError::BadStationName(self.station_name.clone()))?;
        if !matches!(self.min_device_interval, 16 | 32) {
            return Err(ConfigError::BadInterval(self.min_device_interval));
        }
        if self.vendor_id == 0 {
            return Err(ConfigError::BadIdentity);
        }
        let im0 = self.im0.unwrap_or_else(|| Im0 {
            order_id: order_id_from_station_type(&self.station_type),
            ..Im0::default()
        });
        im0.validate()?;
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
            derived.push(Derived {
                inputs,
                input_len,
                outputs,
                output_len,
            });
        }
        check_total_csdu(&submodules, &derived)?;
        Ok(DeviceConfig {
            station_name,
            station_type: self.station_type,
            vendor_id: self.vendor_id,
            device_id: self.device_id,
            min_device_interval: self.min_device_interval,
            submodules,
            derived,
            im0,
        })
    }
}

/// The I&M0 `OrderID` a device gets when the builder was given no [`Im0`]: its
/// `station_type`, kept to ASCII (the record's charset) and to the field's 20 bytes.
fn order_id_from_station_type(station_type: &str) -> String {
    let id: String = station_type
        .chars()
        .filter(char::is_ascii)
        .take(20)
        .collect();
    if id.trim().is_empty() {
        // An empty or entirely non-ASCII station type would put an all-spaces OrderID on the
        // wire and in the GSDML; fall back to the identity `Im0::default()` carries.
        Im0::default().order_id
    } else {
        id
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
        return Err(ConfigError::TooLong {
            slot: slot.0,
            bytes,
            max: MAX_SUBMODULE_BYTES,
        });
    }
    Ok(layout(fields))
}

/// True if `s` is one of the DCP-reserved auto-generated port-name forms
/// `port-xyz` or `port-xyz-abcde` (`x`,`y`,`z`,`a`..`e` decimal digits) — those are
/// assigned automatically to a device's ports and are never valid station names.
fn is_reserved_port_name(s: &str) -> bool {
    fn all_digits(s: &str, len: usize) -> bool {
        s.len() == len && s.bytes().all(|b| b.is_ascii_digit())
    }
    let Some(rest) = s.strip_prefix("port-") else {
        return false;
    };
    match rest.split_once('-') {
        None => all_digits(rest, 3),
        Some((xyz, abcde)) => all_digits(xyz, 3) && all_digits(abcde, 5),
    }
}

/// The Input and Output CR C-SDU lengths the way the controller (and `rt::Layout`)
/// compute them: 3 bytes of DAP IOPS/IOCS, then one `(data_len + 1)` per submodule
/// that has data in that direction plus one IOCS byte per submodule that has data
/// only in the other direction.
fn cr_lengths(submodules: &[SubmoduleSpec], derived: &[Derived]) -> (u32, u32) {
    let (mut input_bytes, mut output_bytes): (u32, u32) = (3, 3);
    for (sm, d) in submodules.iter().zip(derived) {
        let has_in = !sm.inputs.is_empty();
        let has_out = !sm.outputs.is_empty();
        if has_in {
            input_bytes += d.input_len as u32 + 1;
        }
        if has_out {
            output_bytes += d.output_len as u32 + 1;
        }
        if has_out {
            input_bytes += 1; // IOCS point in the CR that doesn't carry this submodule's data
        }
        if has_in {
            output_bytes += 1;
        }
    }
    (input_bytes, output_bytes)
}

/// Reject a total C-SDU (either direction) over the 1440-byte RT frame budget — the
/// same bound `rt::Layout` builds against. Checked once here so `Layout::from_ar` on
/// a config-derived model can never itself fail with `OutOfBounds`.
fn check_total_csdu(submodules: &[SubmoduleSpec], derived: &[Derived]) -> Result<(), ConfigError> {
    let (input_bytes, output_bytes) = cr_lengths(submodules, derived);
    if input_bytes > MAX_SUBMODULE_BYTES as u32 {
        return Err(ConfigError::TooLongTotal {
            direction: Direction::Input,
            bytes: input_bytes,
            max: MAX_SUBMODULE_BYTES,
        });
    }
    if output_bytes > MAX_SUBMODULE_BYTES as u32 {
        return Err(ConfigError::TooLongTotal {
            direction: Direction::Output,
            bytes: output_bytes,
            max: MAX_SUBMODULE_BYTES,
        });
    }
    Ok(())
}

/// PROFINET name-of-station rule (DCP): 1..=240 bytes, labels of `[a-z0-9-]`
/// separated by `.`, no label empty, no label starting/ending with `-`, at least one
/// label not all digits (a pure number would look like an IP), not a reserved
/// `port-xyz`/`port-xyz-abcde` auto-port name. Uppercase is lowercased (TIA does the
/// same).
fn normalize_station_name(s: &str) -> Option<String> {
    let s = s.to_ascii_lowercase();
    if s.is_empty() || s.len() > 240 || !s.is_ascii() || is_reserved_port_name(&s) {
        return None;
    }
    let mut any_non_numeric = false;
    for label in s.split('.') {
        if label.is_empty() || label.starts_with('-') || label.ends_with('-') {
            return None;
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return None;
        }
        if !label.bytes().all(|b| b.is_ascii_digit()) {
            any_non_numeric = true;
        }
    }
    any_non_numeric.then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use FieldType::*;

    fn refs(v: &[(u16, u8, FieldType)]) -> Vec<FieldRef> {
        v.iter()
            .map(|&(byte, bit, ty)| FieldRef { byte, bit, ty })
            .collect()
    }

    #[test]
    fn layout_mixes_bools_and_byte_types_in_declaration_order() {
        let (f, len) = layout(&[Real, Bool, Bool, Int, Bool]);
        assert_eq!(
            f,
            refs(&[
                (0, 0, Real),
                (4, 0, Bool),
                (4, 1, Bool),
                (5, 0, Int),
                (7, 0, Bool)
            ])
        );
        assert_eq!(len, 8);
    }

    #[test]
    fn layout_packs_bools_eight_per_byte() {
        assert_eq!(layout(&[Bool; 32]).1, 4);
        let (f, len) = layout(&[Bool; 9]);
        assert_eq!(len, 2);
        assert_eq!(
            f[8],
            FieldRef {
                byte: 1,
                bit: 0,
                ty: Bool
            }
        );
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
        assert_eq!(
            cfg.submodule(Slot(2)).unwrap().direction(),
            Direction::Input
        );
        assert_eq!(
            cfg.field(Slot(2), Direction::Input, 31),
            Some(FieldRef {
                byte: 3,
                bit: 7,
                ty: Bool
            })
        );
        assert_eq!(cfg.field(Slot(2), Direction::Input, 32), None);
        assert_eq!(cfg.field(Slot(2), Direction::Output, 0), None);
        assert_eq!(cfg.fields(Slot(3), Direction::Output).unwrap().len(), 16);
        assert_eq!(cfg.fields(Slot(9), Direction::Output), None);
    }

    #[test]
    fn cr_lengths_include_the_iops_iocs_bytes() {
        let cfg = sample();
        assert_eq!((cfg.input_cr_len(), cfg.output_cr_len()), (75, 75));

        let one_bool = DeviceConfig::builder("a")
            .input(Slot(1), &[Bool])
            .build()
            .unwrap();
        assert_eq!((one_bool.input_cr_len(), one_bool.output_cr_len()), (5, 4));
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
        assert_eq!(
            (cfg.input_len(Slot(5)), cfg.output_len(Slot(5))),
            (Some(3), Some(4))
        );
        assert_eq!(sm.name, "mixed");
    }

    #[test]
    fn every_config_error_is_reported() {
        let e = |b: DeviceConfigBuilder| b.build().unwrap_err();
        assert_eq!(
            e(DeviceConfig::builder("a").input(Slot(0), &[Bool])),
            ConfigError::SlotZeroReserved
        );
        assert_eq!(
            e(DeviceConfig::builder("a")
                .input(Slot(1), &[Bool])
                .output(Slot(1), &[Bool])),
            ConfigError::DuplicateSlot(1)
        );
        assert_eq!(
            e(DeviceConfig::builder("a").input(Slot(1), &[])),
            ConfigError::EmptySubmodule(1)
        );
        assert_eq!(e(DeviceConfig::builder("a")), ConfigError::NoSubmodule);
        assert_eq!(
            e(DeviceConfig::builder("a").input(Slot(1), &[Real; 361])),
            ConfigError::TooLong {
                slot: 1,
                bytes: 1444,
                max: MAX_SUBMODULE_BYTES
            }
        );
        for bad in [
            "Edge_01",
            "-edge",
            "edge-",
            "123",
            "",
            "a..b",
            "édge",
            "123.456",
            "port-001",
            "port-001-12345",
        ] {
            assert_eq!(
                e(DeviceConfig::builder(bad).input(Slot(1), &[Bool])),
                ConfigError::BadStationName(bad.to_string()),
                "{bad}"
            );
        }
        assert!(DeviceConfig::builder("edge-reg-01.plant2")
            .input(Slot(1), &[Bool])
            .build()
            .is_ok());
        assert!(DeviceConfig::builder("port-a")
            .input(Slot(1), &[Bool])
            .build()
            .is_ok());
        assert_eq!(
            e(DeviceConfig::builder("a")
                .min_device_interval(24)
                .input(Slot(1), &[Bool])),
            ConfigError::BadInterval(24)
        );
        assert_eq!(
            e(DeviceConfig::builder("a")
                .min_device_interval(64)
                .input(Slot(1), &[Bool])),
            ConfigError::BadInterval(64)
        );
        assert_eq!(
            e(DeviceConfig::builder("a")
                .identity(0, 1)
                .input(Slot(1), &[Bool])),
            ConfigError::BadIdentity
        );
    }

    #[test]
    fn total_csdu_over_1440_bytes_is_rejected() {
        // Two 720-byte input slots: 3 (DAP IOPS) + (720+1)*2 = 1445 > 1440.
        let e = DeviceConfig::builder("a")
            .input(Slot(1), &[Real; 180])
            .input(Slot(2), &[Real; 180])
            .build()
            .unwrap_err();
        assert_eq!(
            e,
            ConfigError::TooLongTotal {
                direction: Direction::Input,
                bytes: 1445,
                max: MAX_SUBMODULE_BYTES,
            }
        );
        // The sample config (75/75 bytes, see layout_from_ar_accepts_the_derived_model)
        // stays well under the guard.
        assert!(sample().input_len(Slot(1)).is_some());
    }

    #[test]
    fn station_name_is_lowercased_on_input() {
        // TIA lowercases; we accept mixed case and normalize so the DCP answer matches.
        let cfg = DeviceConfig::builder("Pnio-Dev")
            .input(Slot(1), &[Bool])
            .build()
            .unwrap();
        assert_eq!(cfg.station_name(), "pnio-dev");
    }

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
        let idents: Vec<(u16, u32)> = m.slots[1..]
            .iter()
            .map(|s| (s.slot, s.module_ident))
            .collect();
        assert_eq!(idents, vec![(1, 0x101), (2, 0x102), (3, 0x103), (4, 0x104)]);
        assert_eq!(
            m.slots[1].submodules,
            vec![SubmoduleModel {
                subslot: 1,
                submodule_ident: 0x1,
                input_len: 64,
                output_len: 0
            }]
        );
        assert_eq!(m.find(4, 1).unwrap().output_len, 4);
    }

    #[test]
    fn dcp_properties_and_setup_carry_the_identity() {
        let mac = crate::eth::MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);
        let cfg = sample();
        let p = cfg.dcp_properties([172, 16, 2, 10]);
        assert_eq!(
            (p.vendor_id, p.device_id, p.device_role, p.device_instance),
            (0xFFFF, 1, 0x0100, 1)
        );
        assert_eq!(p.name_of_station, "pnio-dev");
        assert_eq!(p.type_of_station, "pnio device");
        assert_eq!(
            (p.ip, p.subnet, p.gateway, p.ip_block_info),
            ([172, 16, 2, 10], [255, 255, 255, 0], [172, 16, 2, 10], 1)
        );
        assert_eq!(p.device_options, vec![1, 2, 2, 2, 2, 3]);
        let s = cfg.setup(mac, [172, 16, 2, 10], None);
        assert_eq!(s.dcp.mac, mac);
        assert_eq!(s.model, cfg.model(mac));
        assert_eq!(s.activity_seed.0[10..], mac.0);
        assert!(s.rt.is_none());
    }

    #[test]
    fn builder_accepts_im0_and_rejects_bad_ones() {
        use crate::im::ImError;
        let e = DeviceConfig::builder("a")
            .input(Slot(1), &[Bool])
            .im0(Im0 {
                order_id: "x".repeat(21),
                ..Im0::default()
            })
            .build()
            .unwrap_err();
        assert_eq!(
            e,
            ConfigError::Im(ImError::TooLong {
                field: "order_id",
                max: 20
            })
        );

        let custom = Im0 {
            order_id: "test order".into(),
            ..Im0::default()
        };
        let cfg = DeviceConfig::builder("a")
            .input(Slot(1), &[Bool])
            .im0(custom.clone())
            .build()
            .unwrap();
        assert_eq!(cfg.im0(), &custom);

        // Default serial number is derived from the MAC when left blank.
        let mac = crate::eth::MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);
        let s = sample().setup(mac, [172, 16, 2, 10], None);
        assert_eq!(s.im0.serial_number, "PNIO-CD19F8");
    }

    #[test]
    fn default_im0_takes_its_order_id_from_the_station_type() {
        let cfg = DeviceConfig::builder("a")
            .station_type("Foo")
            .input(Slot(1), &[Bool])
            .build()
            .unwrap();
        assert_eq!(cfg.im0().order_id, "Foo");
        // Everything else stays `Im0::default()`.
        assert_eq!(
            cfg.im0(),
            &Im0 {
                order_id: "Foo".into(),
                ..Im0::default()
            }
        );

        // Longer than the 20-byte OrderID field: truncated, not a build error.
        let cfg = DeviceConfig::builder("a")
            .station_type("a station type far longer than twenty bytes")
            .input(Slot(1), &[Bool])
            .build()
            .unwrap();
        assert_eq!(cfg.im0().order_id, "a station type far l");

        // An explicit `im0` still wins, whatever the station type.
        let cfg = DeviceConfig::builder("a")
            .station_type("Foo")
            .input(Slot(1), &[Bool])
            .im0(Im0 {
                order_id: "Bar".into(),
                ..Im0::default()
            })
            .build()
            .unwrap();
        assert_eq!(cfg.im0().order_id, "Bar");

        // `Im0::default()` itself is untouched for direct users.
        assert_eq!(Im0::default().order_id, "pnio device");
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
        let s1 = layout
            .input_cr
            .objects
            .iter()
            .find(|o| o.slot == 1)
            .unwrap();
        assert_eq!((s1.data_off, s1.data_len, s1.iops_off), (3, 64, 67));
    }
}

#[cfg(test)]
mod order_id_fallback_tests {
    use super::*;

    #[test]
    fn empty_or_non_ascii_station_type_falls_back_to_the_default_order_id() {
        assert_eq!(order_id_from_station_type(""), Im0::default().order_id);
        assert_eq!(order_id_from_station_type("   "), Im0::default().order_id);
        assert_eq!(order_id_from_station_type("éèà"), Im0::default().order_id);
        assert_eq!(order_id_from_station_type("Foo"), "Foo");
        assert_eq!(order_id_from_station_type(&"x".repeat(30)), "x".repeat(20));
    }
}

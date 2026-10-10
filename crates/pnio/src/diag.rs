//! Channel-diagnosis store: tracks the active `ChannelDiagnosis` state per
//! submodule and turns raise/clear events into `AlarmNotification`s (Diagnosis /
//! DiagnosisDisappears) plus the aggregate problem indicator. Pure: no I/O, no
//! clock — the device loop drives it.
//!
//! Besides the application `ChannelDiagnosis` entries it holds one port-level
//! entry, the DAP port's remote mismatch ([`DiagStore::set_port_mismatch`]),
//! sent as an `ExtChannelDiagnosis` (USI `0x8002`).

use crate::alarm::{
    AlarmNotification, AlarmSpecifier, AlarmType, ChannelDiagnosis, ChannelProperties,
    ExtChannelDiagnosis, Maintenance, Specifier, UserData, USI_CHANNEL_DIAG, USI_EXT_CHANNEL_DIAG,
};
use crate::cm::DeviceModel;
use crate::config::{Direction, Slot};
use crate::pdev::Mismatch;
use std::collections::BTreeMap;

/// `ChannelDiagnosis.error_type` codes this store knows how to raise/clear
/// (PROFINET's "standard" channel error type range).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u16)]
pub enum ChannelError {
    /// `ChannelErrorType` 0x0001.
    ShortCircuit = 0x0001,
    /// `ChannelErrorType` 0x0002.
    Undervoltage = 0x0002,
    /// `ChannelErrorType` 0x0003.
    Overvoltage = 0x0003,
    /// `ChannelErrorType` 0x0004.
    Overload = 0x0004,
    /// `ChannelErrorType` 0x0005.
    Overtemperature = 0x0005,
    /// `ChannelErrorType` 0x0006.
    LineBreak = 0x0006,
    /// `ChannelErrorType` 0x0007.
    UpperLimitExceeded = 0x0007,
    /// `ChannelErrorType` 0x0008.
    LowerLimitExceeded = 0x0008,
    /// `ChannelErrorType` 0x0009.
    Error = 0x0009,
}

impl ChannelError {
    /// The wire `ChannelErrorType` code.
    pub fn code(self) -> u16 {
        self as u16
    }

    /// Inverse of [`ChannelError::code`]; `None` if `c` is not one of the codes this
    /// store knows.
    pub fn from_code(c: u16) -> Option<ChannelError> {
        match c {
            0x0001 => Some(ChannelError::ShortCircuit),
            0x0002 => Some(ChannelError::Undervoltage),
            0x0003 => Some(ChannelError::Overvoltage),
            0x0004 => Some(ChannelError::Overload),
            0x0005 => Some(ChannelError::Overtemperature),
            0x0006 => Some(ChannelError::LineBreak),
            0x0007 => Some(ChannelError::UpperLimitExceeded),
            0x0008 => Some(ChannelError::LowerLimitExceeded),
            0x0009 => Some(ChannelError::Error),
            _ => None,
        }
    }

    /// Every name [`ChannelError::from_name`] accepts, in the same order as
    /// [`ChannelError::from_code`]'s codes (1..=9) — the single source of truth for
    /// CLI help/error text that needs to list them (e.g. `typed_bringup`'s `--diag`).
    pub const fn names() -> &'static [&'static str] {
        &[
            "short-circuit",
            "undervoltage",
            "overvoltage",
            "overload",
            "overtemperature",
            "line-break",
            "upper-limit",
            "lower-limit",
            "error",
        ]
    }

    /// Inverse of `names()[error.code() as usize - 1] == name`: looks `s` up in
    /// [`ChannelError::names`] and resolves it through [`ChannelError::from_code`],
    /// so the two never drift apart.
    pub fn from_name(s: &str) -> Option<ChannelError> {
        let idx = Self::names().iter().position(|&n| n == s)?;
        Self::from_code(idx as u16 + 1)
    }
}

/// How severe a raised diagnosis is; maps to `ChannelProperties.maintenance`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Maps to [`Maintenance::Fault`]; the only severity that sets
    /// [`DiagStore::problem_indicator`].
    Fault,
    /// Maps to [`Maintenance::Required`].
    MaintenanceRequired,
    /// Maps to [`Maintenance::Demanded`].
    MaintenanceDemanded,
}

/// `ChannelDiagnosis.channel` value meaning "the whole submodule", not one channel.
pub const WHOLE_SUBMODULE: u16 = 0x8000;

/// `ChannelErrorType` for the port's remote-mismatch diagnoses ("Remote mismatch",
/// pinned in docs/pdev-golden-frames.md §2.10).
pub const PORT_ERROR_TYPE: u16 = 0x8001;

/// Slot of the DAP port submodule [`DiagStore::with_port`] looks up.
const PORT_SLOT: u16 = 0;
/// Subslot of the DAP port submodule [`DiagStore::with_port`] looks up.
const PORT_SUBSLOT: u16 = 0x8001;

/// One channel diagnosis, as raised or cleared by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnosis {
    /// Slot the diagnosis applies to.
    pub slot: Slot,
    /// Channel number, or [`WHOLE_SUBMODULE`] for the whole submodule.
    pub channel: u16,
    /// Which error is being raised or cleared.
    pub error: ChannelError,
    /// How severe it is.
    pub severity: Severity,
    /// Not meaningful as input: [`DiagStore::raise`] overwrites it from the
    /// submodule's own declared direction.
    pub direction: Direction,
}

/// What the store needs to know about one submodule to build notifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmoduleInfo {
    /// Slot the submodule is plugged into.
    pub slot: Slot,
    /// Subslot (always `1` for the submodules [`DiagStore::from_model`] derives).
    pub subslot: u16,
    /// The slot's module identity number.
    pub module_ident: u32,
    /// The submodule's identity number.
    pub submodule_ident: u32,
    /// Its data direction, used to fill `ChannelProperties.direction` on every
    /// notification built for it.
    pub direction: Direction,
}

/// Tracks the active channel diagnoses across a device's submodules and produces
/// the `AlarmNotification`s (and the aggregate problem indicator) they imply.
pub struct DiagStore {
    submodules: Vec<SubmoduleInfo>,
    active: BTreeMap<(u16, u16, u16), Diagnosis>,
    /// The DAP port submodule and its current remote mismatch, once registered
    /// by [`DiagStore::with_port`].
    port: Option<(SubmoduleInfo, Option<Mismatch>)>,
}

impl DiagStore {
    /// An empty store (no active diagnoses) that knows about `submodules`.
    pub fn new(submodules: Vec<SubmoduleInfo>) -> Self {
        DiagStore {
            submodules,
            active: BTreeMap::new(),
            port: None,
        }
    }

    /// Register the DAP port submodule so port diagnoses can be built
    /// (module/submodule idents from the model's slot 0 / subslot 0x8001). If the
    /// model has no such submodule the store stays port-less and
    /// [`DiagStore::set_port_mismatch`] produces nothing.
    pub fn with_port(mut self, model: &DeviceModel) -> Self {
        self.port = model
            .slots
            .iter()
            .find(|slot| slot.slot == PORT_SLOT)
            .and_then(|slot| {
                let sm = slot
                    .submodules
                    .iter()
                    .find(|sm| sm.subslot == PORT_SUBSLOT)?;
                Some(SubmoduleInfo {
                    slot: Slot(PORT_SLOT),
                    subslot: PORT_SUBSLOT,
                    module_ident: slot.module_ident,
                    submodule_ident: sm.submodule_ident,
                    direction: Direction::InputOutput,
                })
            })
            .map(|info| (info, None));
        self
    }

    /// Set the port's mismatch state; returns the notification(s) implied:
    /// Appears on a new mismatch, Disappears on `None`, Disappears (old) then
    /// Appears (new) on a changed one, nothing when unchanged. Without a port
    /// registered by [`DiagStore::with_port`] it returns nothing (and logs).
    pub fn set_port_mismatch(&mut self, m: Option<Mismatch>) -> Vec<AlarmNotification> {
        let Some((info, current)) = self.port.clone() else {
            if let Some(m) = m {
                log::warn!(
                    "port mismatch {m:?} ignored: no DAP port submodule \
                     (slot 0 / subslot 0x8001) registered"
                );
            }
            return Vec::new();
        };
        if current == m {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(2);
        if let Some(old) = current {
            self.set_port_state(None);
            out.push(self.build_port_notification(
                &info,
                old,
                AlarmType::DiagnosisDisappears,
                Specifier::Disappears,
            ));
        }
        if let Some(new) = m {
            self.set_port_state(Some(new));
            out.push(self.build_port_notification(
                &info,
                new,
                AlarmType::Diagnosis,
                Specifier::Appears,
            ));
        }
        out
    }

    /// The port's current remote mismatch (`None` also when no port is registered).
    pub fn port_mismatch(&self) -> Option<Mismatch> {
        self.port.as_ref().and_then(|(_, m)| *m)
    }

    fn set_port_state(&mut self, m: Option<Mismatch>) {
        if let Some((_, current)) = self.port.as_mut() {
            *current = m;
        }
    }

    /// Builds the store from a device's plug-and-play model: every slot `> 0`
    /// and its subslot-1 submodule, direction derived from that submodule's
    /// input/output lengths.
    pub fn from_model(model: &DeviceModel) -> Self {
        let submodules = model
            .slots
            .iter()
            .filter(|slot| slot.slot > 0)
            .filter_map(|slot| {
                let sm = slot.submodules.iter().find(|sm| sm.subslot == 1)?;
                let direction = if sm.output_len == 0 && sm.input_len > 0 {
                    Direction::Input
                } else if sm.input_len == 0 && sm.output_len > 0 {
                    Direction::Output
                } else {
                    Direction::InputOutput
                };
                Some(SubmoduleInfo {
                    slot: Slot(slot.slot),
                    subslot: 1,
                    module_ident: slot.module_ident,
                    submodule_ident: sm.submodule_ident,
                    direction,
                })
            })
            .collect();
        DiagStore::new(submodules)
    }

    fn info(&self, slot: Slot) -> Option<&SubmoduleInfo> {
        self.submodules.iter().find(|s| s.slot == slot)
    }

    /// Whether `slot` is one of the submodules this store was built with.
    pub fn knows(&self, slot: Slot) -> bool {
        self.info(slot).is_some()
    }

    /// Raises (or updates the severity of) one channel diagnosis. `None` if an
    /// identical diagnosis (same slot/channel/error/severity) is already active,
    /// or if `d.slot` is not a submodule this store knows about.
    pub fn raise(&mut self, mut d: Diagnosis) -> Option<AlarmNotification> {
        let info = self.info(d.slot)?.clone();
        d.direction = info.direction;
        let key = (d.slot.0, d.channel, d.error.code());
        if let Some(existing) = self.active.get(&key) {
            if existing.severity == d.severity {
                return None;
            }
        }
        self.active.insert(key, d.clone());
        Some(self.build_notification(&info, &d, AlarmType::Diagnosis, Specifier::Appears))
    }

    /// Clears one active channel diagnosis. `None` if it was not active.
    pub fn clear(
        &mut self,
        slot: Slot,
        channel: u16,
        error: ChannelError,
    ) -> Option<AlarmNotification> {
        let key = (slot.0, channel, error.code());
        let removed = self.active.remove(&key)?;
        let info = self.info(slot)?.clone();
        let others_on_channel = self
            .active
            .keys()
            .any(|&(s, ch, _)| s == slot.0 && ch == channel);
        let specifier = if others_on_channel {
            Specifier::DisappearsOthersRemain
        } else {
            Specifier::Disappears
        };
        Some(self.build_notification(&info, &removed, AlarmType::DiagnosisDisappears, specifier))
    }

    /// True if any active diagnosis has `Severity::Fault`, or a port mismatch is
    /// set (always a fault).
    pub fn problem_indicator(&self) -> bool {
        self.active.values().any(|d| d.severity == Severity::Fault)
            || self.port_mismatch().is_some()
    }

    /// Every currently active application diagnosis (the port's remote mismatch
    /// is not a [`ChannelError`]; see [`DiagStore::port_mismatch`]).
    pub fn active(&self) -> Vec<Diagnosis> {
        self.active.values().cloned().collect()
    }

    /// One `Appears` notification per currently active diagnosis (the port's
    /// remote mismatch included, last), for replaying state to a newly connected
    /// controller.
    pub fn replay(&self) -> Vec<AlarmNotification> {
        let mut out: Vec<AlarmNotification> = self
            .active
            .values()
            .filter_map(|d| {
                let info = self.info(d.slot)?;
                Some(self.build_notification(info, d, AlarmType::Diagnosis, Specifier::Appears))
            })
            .collect();
        if let Some((info, Some(m))) = &self.port {
            out.push(self.build_port_notification(
                info,
                *m,
                AlarmType::Diagnosis,
                Specifier::Appears,
            ));
        }
        out
    }

    /// Whether any diagnosis — application or port — is active on the AR.
    fn ar_diag(&self) -> bool {
        !self.active.is_empty() || self.port_mismatch().is_some()
    }

    /// The port's remote-mismatch `ExtChannelDiagnosis` (docs/pdev-golden-frames.md
    /// §2.10): whole submodule, `ChannelErrorType` [`PORT_ERROR_TYPE`],
    /// `ExtChannelErrorType` from `m`, direction 0 (manufacturer specific), Fault.
    /// The `AlarmSpecifier` flags reflect the store's state as already updated.
    fn build_port_notification(
        &self,
        info: &SubmoduleInfo,
        m: Mismatch,
        alarm_type: AlarmType,
        specifier: Specifier,
    ) -> AlarmNotification {
        let port_diag = self.port_mismatch().is_some();
        AlarmNotification {
            alarm_type,
            api: 0,
            slot: info.slot.0,
            subslot: info.subslot,
            module_ident: info.module_ident,
            submodule_ident: info.submodule_ident,
            specifier: AlarmSpecifier {
                sequence: 0,
                channel_diag: port_diag,
                manufacturer_diag: false,
                submodule_diag: port_diag,
                ar_diag: self.ar_diag(),
            },
            usi: USI_EXT_CHANNEL_DIAG,
            data: UserData::ExtChannel(ExtChannelDiagnosis {
                channel: WHOLE_SUBMODULE,
                properties: ChannelProperties {
                    type_: 0,
                    accumulative: false,
                    maintenance: Maintenance::Fault,
                    specifier,
                    direction: 0,
                },
                error_type: PORT_ERROR_TYPE,
                ext_error_type: m.ext_error_type(),
                ext_add_value: 0,
            }),
        }
    }

    fn build_notification(
        &self,
        info: &SubmoduleInfo,
        d: &Diagnosis,
        alarm_type: AlarmType,
        specifier: Specifier,
    ) -> AlarmNotification {
        let channel_diag = self.active.keys().any(|&(slot, _, _)| slot == d.slot.0);
        let ar_diag = self.ar_diag();
        let maintenance = match d.severity {
            Severity::Fault => Maintenance::Fault,
            Severity::MaintenanceRequired => Maintenance::Required,
            Severity::MaintenanceDemanded => Maintenance::Demanded,
        };
        let direction = match info.direction {
            Direction::Input => 1,
            Direction::Output => 2,
            Direction::InputOutput => 3,
        };
        AlarmNotification {
            alarm_type,
            api: 0,
            slot: info.slot.0,
            subslot: info.subslot,
            module_ident: info.module_ident,
            submodule_ident: info.submodule_ident,
            specifier: AlarmSpecifier {
                sequence: 0,
                channel_diag,
                manufacturer_diag: false,
                submodule_diag: channel_diag,
                ar_diag,
            },
            usi: USI_CHANNEL_DIAG,
            data: UserData::Channel(ChannelDiagnosis {
                channel: d.channel,
                properties: ChannelProperties {
                    type_: 0,
                    accumulative: false,
                    maintenance,
                    specifier,
                    direction,
                },
                error_type: d.error.code(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alarm::*;
    use crate::config::{Direction, Slot};
    use crate::eth::MacAddr;

    fn store() -> DiagStore {
        DiagStore::new(vec![
            SubmoduleInfo {
                slot: Slot(1),
                subslot: 1,
                module_ident: 0x101,
                submodule_ident: 1,
                direction: Direction::Input,
            },
            SubmoduleInfo {
                slot: Slot(3),
                subslot: 1,
                module_ident: 0x103,
                submodule_ident: 1,
                direction: Direction::Output,
            },
        ])
    }
    fn d(slot: u16, ch: u16, e: ChannelError, s: Severity) -> Diagnosis {
        Diagnosis {
            slot: Slot(slot),
            channel: ch,
            error: e,
            severity: s,
            direction: Direction::Input,
        }
    }

    #[test]
    fn every_name_round_trips_through_from_name() {
        assert!(ChannelError::names()
            .iter()
            .all(|&n| ChannelError::from_name(n).is_some()));
    }
    #[allow(clippy::clone_on_copy)] // brief's test helper, kept verbatim
    fn chan(n: &AlarmNotification) -> ChannelDiagnosis {
        match &n.data {
            UserData::Channel(c) => c.clone(),
            _ => panic!(),
        }
    }

    #[test]
    fn raise_builds_a_channel_diagnosis_appears() {
        let mut s = store();
        let n = s
            .raise(d(1, 0, ChannelError::LineBreak, Severity::Fault))
            .unwrap();
        assert_eq!(n.alarm_type, AlarmType::Diagnosis);
        assert_eq!(
            (n.slot, n.subslot, n.module_ident, n.submodule_ident),
            (1, 1, 0x101, 1)
        );
        assert_eq!(n.usi, USI_CHANNEL_DIAG);
        let c = chan(&n);
        assert_eq!(c.error_type, 0x0006);
        assert_eq!(
            c.properties,
            ChannelProperties {
                type_: 0,
                accumulative: false,
                maintenance: Maintenance::Fault,
                specifier: Specifier::Appears,
                direction: 1
            }
        );
        assert!(n.specifier.channel_diag && n.specifier.submodule_diag && n.specifier.ar_diag);
        assert!(s.problem_indicator());
    }

    #[test]
    fn identical_raise_is_noop_and_severity_change_is_update() {
        let mut s = store();
        s.raise(d(1, 0, ChannelError::LineBreak, Severity::Fault));
        assert!(s
            .raise(d(1, 0, ChannelError::LineBreak, Severity::Fault))
            .is_none());
        let n = s
            .raise(d(
                1,
                0,
                ChannelError::LineBreak,
                Severity::MaintenanceRequired,
            ))
            .unwrap();
        assert_eq!(chan(&n).properties.maintenance, Maintenance::Required);
        assert!(
            !s.problem_indicator(),
            "maintenance-required is not a fault"
        );
        assert_eq!(s.active().len(), 1);
    }

    #[test]
    fn clear_builds_disappears_and_clears_flags_when_last() {
        let mut s = store();
        s.raise(d(1, 0, ChannelError::LineBreak, Severity::Fault));
        s.raise(d(1, 0, ChannelError::Overload, Severity::Fault));
        let n = s.clear(Slot(1), 0, ChannelError::LineBreak).unwrap();
        assert_eq!(n.alarm_type, AlarmType::DiagnosisDisappears);
        assert_eq!(
            chan(&n).properties.specifier,
            Specifier::DisappearsOthersRemain
        );
        assert!(n.specifier.channel_diag);
        let n = s.clear(Slot(1), 0, ChannelError::Overload).unwrap();
        assert_eq!(chan(&n).properties.specifier, Specifier::Disappears);
        assert!(!n.specifier.channel_diag && !n.specifier.ar_diag);
        assert!(!s.problem_indicator());
        assert!(s.clear(Slot(1), 0, ChannelError::Overload).is_none());
    }

    #[test]
    fn output_submodule_direction_and_replay() {
        let mut s = store();
        s.raise(d(3, WHOLE_SUBMODULE, ChannelError::Error, Severity::Fault));
        s.raise(d(
            1,
            2,
            ChannelError::ShortCircuit,
            Severity::MaintenanceDemanded,
        ));
        let r = s.replay();
        assert_eq!(r.len(), 2);
        let out = r.iter().find(|n| n.slot == 3).unwrap();
        assert_eq!(chan(out).properties.direction, 2);
        assert_eq!(chan(out).channel, WHOLE_SUBMODULE);
        assert!(r
            .iter()
            .all(|n| chan(n).properties.specifier == Specifier::Appears && n.specifier.ar_diag));
    }

    #[test]
    fn from_model_derives_directions() {
        use crate::cm::{DeviceModel, SlotModel, SubmoduleModel};
        let sm = |i, o| SubmoduleModel {
            subslot: 1,
            submodule_ident: 1,
            input_len: i,
            output_len: o,
        };
        let m = DeviceModel {
            vendor_id: 0xFFFF,
            device_id: 1,
            instance: 1,
            station_name: "x".into(),
            mac: crate::eth::MacAddr([0; 6]),
            max_alarm_data_length: 200,
            slots: vec![
                SlotModel {
                    slot: 0,
                    module_ident: 1,
                    submodules: vec![],
                },
                SlotModel {
                    slot: 1,
                    module_ident: 0x101,
                    submodules: vec![sm(4, 0)],
                },
                SlotModel {
                    slot: 2,
                    module_ident: 0x102,
                    submodules: vec![sm(0, 4)],
                },
                SlotModel {
                    slot: 3,
                    module_ident: 0x103,
                    submodules: vec![sm(2, 2)],
                },
            ],
        };
        let s = DiagStore::from_model(&m);
        assert!(s.knows(Slot(1)) && s.knows(Slot(3)) && !s.knows(Slot(0)) && !s.knows(Slot(9)));
        let mut s = s;
        let n1 = s
            .raise(d(1, 0, ChannelError::Error, Severity::Fault))
            .unwrap();
        let n2 = s
            .raise(d(2, 0, ChannelError::Error, Severity::Fault))
            .unwrap();
        let n3 = s
            .raise(d(3, 0, ChannelError::Error, Severity::Fault))
            .unwrap();
        assert_eq!(
            (
                chan(&n1).properties.direction,
                chan(&n2).properties.direction,
                chan(&n3).properties.direction
            ),
            (1, 2, 3)
        );
    }

    #[test]
    fn port_mismatch_appears_changes_and_disappears_as_ext_channel_diag() {
        use crate::pdev::Mismatch;
        let model = DeviceModel::pnet_sample(MacAddr([0; 6]));
        let mut s = DiagStore::from_model(&model).with_port(&model);
        assert!(s.set_port_mismatch(None).is_empty());
        let n = s.set_port_mismatch(Some(Mismatch::Port));
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].alarm_type, AlarmType::Diagnosis);
        assert_eq!((n[0].slot, n[0].subslot), (0, 0x8001));
        assert_eq!(n[0].usi, USI_EXT_CHANNEL_DIAG);
        let UserData::ExtChannel(d) = &n[0].data else {
            panic!()
        };
        assert_eq!(d.channel, WHOLE_SUBMODULE);
        assert_eq!(d.error_type, PORT_ERROR_TYPE);
        assert_eq!(d.ext_error_type, Mismatch::Port.ext_error_type());
        assert!(s.problem_indicator());
        assert!(
            s.set_port_mismatch(Some(Mismatch::Port)).is_empty(),
            "no churn"
        );
        let n = s.set_port_mismatch(Some(Mismatch::Chassis));
        assert_eq!(n.len(), 2, "old disappears, new appears");
        assert_eq!(n[0].alarm_type, AlarmType::DiagnosisDisappears);
        assert_eq!(n[1].alarm_type, AlarmType::Diagnosis);
        assert_eq!(s.replay().len(), 1);
        let n = s.set_port_mismatch(None);
        assert_eq!(n[0].alarm_type, AlarmType::DiagnosisDisappears);
        assert!(!s.problem_indicator());
        assert!(s.replay().is_empty());
        assert!(
            s.active().is_empty(),
            "the port entry is not an application ChannelError"
        );
    }

    #[test]
    fn port_mismatch_without_with_port_is_ignored() {
        let model = DeviceModel::pnet_sample(MacAddr([0; 6]));
        let mut s = DiagStore::from_model(&model);
        assert!(s
            .set_port_mismatch(Some(crate::pdev::Mismatch::NoPeer))
            .is_empty());
    }

    /// The port diagnosis on the wire: the full AlarmNotification block body as
    /// `build_frame` encodes it (docs/pdev-golden-frames.md §2.10), and back.
    #[test]
    fn port_mismatch_notification_encodes_ext_channel_diag_bytes() {
        use crate::pdev::Mismatch;
        let model = DeviceModel::pnet_sample(MacAddr([0; 6]));
        let mut s = DiagStore::from_model(&model).with_port(&model);
        let frame_of = |n: &AlarmNotification| {
            let pdu = RtaPdu {
                priority: Priority::Low,
                header: RtaHeader {
                    dst_ref: 1,
                    src_ref: 2,
                    pdu_type: PduType::Data,
                    tack: true,
                    send_seq: 0,
                    ack_seq: SEQ_NONE,
                },
                body: RtaBody::Data(RtaData::Notification(n.clone())),
            };
            let f = build_frame(MacAddr([1; 6]), MacAddr([2; 6]), &pdu);
            assert_eq!(parse_frame(&f).unwrap(), pdu, "round-trips");
            f
        };
        // BlockHeader (0x0002 low, BlockLength 36, v1.0) + AlarmNotification body.
        let header = |alarm_type: u8, alarm_spec: [u8; 2]| {
            let mut v = vec![0x00, 0x02, 0x00, 0x24, 0x01, 0x00];
            v.extend_from_slice(&[0x00, alarm_type]); // AlarmType
            v.extend_from_slice(&[0, 0, 0, 0]); // API 0
            v.extend_from_slice(&[0x00, 0x00, 0x80, 0x01]); // slot 0, subslot 0x8001
            v.extend_from_slice(&[0, 0, 0, 0x01, 0, 0, 0x80, 0x01]); // module 0x1, submodule 0x8001
            v.extend_from_slice(&alarm_spec);
            v.extend_from_slice(&[0x80, 0x02]); // USI ExtChannelDiagnosis
            v
        };

        let n = s.set_port_mismatch(Some(Mismatch::NoPeer));
        let mut want = header(0x01, [0xA8, 0x00]); // channel+submodule+AR diag
        want.extend_from_slice(&[
            0x80, 0x00, // ChannelNumber: whole submodule
            0x08, 0x00, // ChannelProperties: type 0, Fault, Appears, direction 0
            0x80, 0x01, // ChannelErrorType: Remote mismatch
            0x80, 0x05, // ExtChannelErrorType: no peer detected
            0x00, 0x00, 0x00, 0x00, // ExtChannelAddValue
        ]);
        let f = frame_of(&n[0]);
        assert_eq!(&f[f.len() - want.len()..], &want[..]);

        let n = s.set_port_mismatch(None);
        let mut want = header(0x0C, [0x00, 0x00]); // DiagnosisDisappears, nothing left
        want.extend_from_slice(&[
            0x80, 0x00, //
            0x10, 0x00, // ChannelProperties: Fault, Disappears, direction 0
            0x80, 0x01, //
            0x80, 0x05, //
            0x00, 0x00, 0x00, 0x00,
        ]);
        let f = frame_of(&n[0]);
        assert_eq!(&f[f.len() - want.len()..], &want[..]);
    }

    #[test]
    fn channel_error_names_and_codes() {
        assert_eq!(
            ChannelError::from_name("line-break"),
            Some(ChannelError::LineBreak)
        );
        assert_eq!(ChannelError::from_code(0x0009), Some(ChannelError::Error));
        assert_eq!(ChannelError::from_code(0x0100), None);
    }
}

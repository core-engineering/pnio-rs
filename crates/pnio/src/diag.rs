//! Channel-diagnosis store: tracks the active `ChannelDiagnosis` state per
//! submodule and turns raise/clear events into `AlarmNotification`s (Diagnosis /
//! DiagnosisDisappears) plus the aggregate problem indicator. Pure: no I/O, no
//! clock — the device loop drives it.

use crate::alarm::{
    AlarmNotification, AlarmSpecifier, AlarmType, ChannelDiagnosis, ChannelProperties, Maintenance,
    Specifier, UserData, USI_CHANNEL_DIAG,
};
use crate::cm::DeviceModel;
use crate::config::{Direction, Slot};
use std::collections::BTreeMap;

/// `ChannelDiagnosis.error_type` codes this store knows how to raise/clear
/// (PROFINET's "standard" channel error type range).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u16)]
pub enum ChannelError {
    ShortCircuit = 0x0001,
    Undervoltage = 0x0002,
    Overvoltage = 0x0003,
    Overload = 0x0004,
    Overtemperature = 0x0005,
    LineBreak = 0x0006,
    UpperLimitExceeded = 0x0007,
    LowerLimitExceeded = 0x0008,
    Error = 0x0009,
}

impl ChannelError {
    pub fn code(self) -> u16 {
        self as u16
    }

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
    Fault,
    MaintenanceRequired,
    MaintenanceDemanded,
}

/// `ChannelDiagnosis.channel` value meaning "the whole submodule", not one channel.
pub const WHOLE_SUBMODULE: u16 = 0x8000;

/// One channel diagnosis, as raised or cleared by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnosis {
    pub slot: Slot,
    pub channel: u16,
    pub error: ChannelError,
    pub severity: Severity,
    pub direction: Direction,
}

/// What the store needs to know about one submodule to build notifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmoduleInfo {
    pub slot: Slot,
    pub subslot: u16,
    pub module_ident: u32,
    pub submodule_ident: u32,
    pub direction: Direction,
}

/// Tracks the active channel diagnoses across a device's submodules and produces
/// the `AlarmNotification`s (and the aggregate problem indicator) they imply.
pub struct DiagStore {
    submodules: Vec<SubmoduleInfo>,
    active: BTreeMap<(u16, u16, u16), Diagnosis>,
}

impl DiagStore {
    pub fn new(submodules: Vec<SubmoduleInfo>) -> Self {
        DiagStore {
            submodules,
            active: BTreeMap::new(),
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

    /// True if any active diagnosis has `Severity::Fault`.
    pub fn problem_indicator(&self) -> bool {
        self.active.values().any(|d| d.severity == Severity::Fault)
    }

    pub fn active(&self) -> Vec<Diagnosis> {
        self.active.values().cloned().collect()
    }

    /// One `Appears` notification per currently active diagnosis, for replaying
    /// state to a newly connected controller.
    pub fn replay(&self) -> Vec<AlarmNotification> {
        self.active
            .values()
            .filter_map(|d| {
                let info = self.info(d.slot)?;
                Some(self.build_notification(info, d, AlarmType::Diagnosis, Specifier::Appears))
            })
            .collect()
    }

    fn build_notification(
        &self,
        info: &SubmoduleInfo,
        d: &Diagnosis,
        alarm_type: AlarmType,
        specifier: Specifier,
    ) -> AlarmNotification {
        let channel_diag = self.active.keys().any(|&(slot, _, _)| slot == d.slot.0);
        let ar_diag = !self.active.is_empty();
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
    fn channel_error_names_and_codes() {
        assert_eq!(
            ChannelError::from_name("line-break"),
            Some(ChannelError::LineBreak)
        );
        assert_eq!(ChannelError::from_code(0x0009), Some(ChannelError::Error));
        assert_eq!(ChannelError::from_code(0x0100), None);
    }
}

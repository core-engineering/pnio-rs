//! Device model: the slot/submodule plug-and-play structure this device instance
//! offers, used to validate a Connect request against what is actually plugged.
//!
//! `DeviceModel::pnet_sample` mirrors the identity of the p-net "rt-labs-dev" sample
//! application so the TIA project on the hardware-in-the-loop bench (engineered
//! against the real p-net device) keeps working unmodified against this stack.

use crate::eth::MacAddr;
use crate::rpc::Uuid;

/// A device's slot/submodule structure plus the identity fields needed to answer
/// Connect (station name, MAC, alarm sizing) and to build the PNIO object UUID.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceModel {
    /// `VendorID`, part of the PNIO object UUID and I&M0's identity.
    pub vendor_id: u16,
    /// `DeviceID`, part of the PNIO object UUID and I&M0's identity.
    pub device_id: u16,
    /// `DeviceInstance`, part of the PNIO object UUID.
    pub instance: u16,
    /// `NameOfStation`, matched against the Connect request's station name.
    pub station_name: String,
    /// This device's Ethernet MAC address.
    pub mac: MacAddr,
    /// Value this device answers with in the AlarmCR negotiation
    /// (`AlarmCRBlockRes.MaxAlarmDataLength`); the actual cap applied to outgoing
    /// alarms is the controller's own requested value, not this one (see
    /// [`ArParams::max_alarm_data_length_remote`](crate::cm::connect::ArParams::max_alarm_data_length_remote)).
    pub max_alarm_data_length: u16,
    /// The device's slots, in any order; slot `0` is the DAP.
    pub slots: Vec<SlotModel>,
}

/// One slot's plugged module and its submodules.
#[derive(Debug, Clone, PartialEq)]
pub struct SlotModel {
    /// Slot number.
    pub slot: u16,
    /// The plugged module's identity number (`ModuleIdentNumber`).
    pub module_ident: u32,
    /// The module's submodules, in any order.
    pub submodules: Vec<SubmoduleModel>,
}

/// One submodule's identity and IO data lengths.
#[derive(Debug, Clone, PartialEq)]
pub struct SubmoduleModel {
    /// Subslot number.
    pub subslot: u16,
    /// The plugged submodule's identity number (`SubmoduleIdentNumber`).
    pub submodule_ident: u32,
    /// Input (device-to-controller) data length in bytes; `0` if this submodule has no input data.
    pub input_len: u16,
    /// Output (controller-to-device) data length in bytes; `0` if this submodule has no output data.
    pub output_len: u16,
}

impl DeviceModel {
    /// Look up the submodule plugged at `(slot, subslot)`, if any.
    pub fn find(&self, slot: u16, subslot: u16) -> Option<&SubmoduleModel> {
        self.slots
            .iter()
            .find(|s| s.slot == slot)?
            .submodules
            .iter()
            .find(|sm| sm.subslot == subslot)
    }

    /// The PNIO object UUID for this device instance:
    /// `dea00000-6c97-11d1-8271-{instance}{device_id}{vendor_id}`.
    pub fn object_uuid(&self) -> Uuid {
        Uuid::pnio_object(self.instance, self.device_id, self.vendor_id)
    }

    /// The p-net "rt-labs-dev" sample application identity, cloned so the bench's
    /// TIA project (engineered against the real p-net device) stays unchanged.
    pub fn pnet_sample(mac: MacAddr) -> DeviceModel {
        fn sm(
            subslot: u16,
            submodule_ident: u32,
            input_len: u16,
            output_len: u16,
        ) -> SubmoduleModel {
            SubmoduleModel {
                subslot,
                submodule_ident,
                input_len,
                output_len,
            }
        }
        DeviceModel {
            vendor_id: 0x0493,
            device_id: 0x0002,
            instance: 1,
            station_name: "rt-labs-dev".to_string(),
            mac,
            max_alarm_data_length: 200,
            slots: vec![
                SlotModel {
                    slot: 0,
                    module_ident: 0x1,
                    submodules: vec![
                        sm(1, 0x1, 0, 0),
                        sm(0x8000, 0x8000, 0, 0),
                        sm(0x8001, 0x8001, 0, 0),
                    ],
                },
                SlotModel {
                    slot: 1,
                    module_ident: 0x30,
                    submodules: vec![sm(1, 0x130, 1, 0)],
                },
                SlotModel {
                    slot: 2,
                    module_ident: 0x31,
                    submodules: vec![sm(1, 0x131, 0, 1)],
                },
                SlotModel {
                    slot: 3,
                    module_ident: 0x32,
                    submodules: vec![sm(1, 0x132, 1, 1)],
                },
                SlotModel {
                    slot: 4,
                    module_ident: 0x40,
                    submodules: vec![sm(1, 0x140, 8, 8)],
                },
            ],
        }
    }
}

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
        assert_eq!(
            m.object_uuid().to_string(),
            "dea00000-6c97-11d1-8271-000100020493"
        );
    }
}

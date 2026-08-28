//! C-SDU layout: turns the AR parameters negotiated at Connect (`ArParams`) plus the
//! `DeviceModel` into flat serialization tables for the cyclic engine and the I/O
//! image — one table per Communication Relationship (input, output), plus a
//! per-submodule `Cell` index the I/O image walks in model order.

use std::time::Duration;

use thiserror::Error;

use super::CYCLE_UNIT;
use crate::cm::{ArParams, DeviceModel, IocrParams};

/// One IO data object inside a CR's C-SDU: payload bytes plus its trailing IOPS byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoObject {
    pub slot: u16,
    pub subslot: u16,
    pub data_off: usize,
    pub data_len: usize,
    pub iops_off: usize,
}

/// One IOCS object inside a CR's C-SDU (a submodule with no data in this direction,
/// or an extra consumer-status point such as the DAP's slot/subslot entries).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsObject {
    pub slot: u16,
    pub subslot: u16,
    pub iocs_off: usize,
}

/// The flat serialization table for one Communication Relationship (input or output).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrLayout {
    pub frame_id: u16,
    pub data_length: usize,
    pub cycle_step: u16,
    pub watchdog: Duration,
    pub objects: Vec<IoObject>,
    pub iocs: Vec<CsObject>,
}

impl CrLayout {
    /// The send cycle period: `cycle_step * CYCLE_UNIT` (CYCLE_UNIT = 31.25 us).
    pub fn period(&self) -> Duration {
        CYCLE_UNIT * self.cycle_step as u32
    }
}

/// One model submodule's offsets in each direction's CR, if it carries data there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub slot: u16,
    pub subslot: u16,
    pub input_len: usize,
    pub output_len: usize,
    /// `data_off` in the input CR, if this submodule has an IO data object there.
    pub input_off: Option<usize>,
    /// `data_off` in the output CR, if this submodule has an IO data object there.
    pub output_off: Option<usize>,
}

/// The C-SDU plan for both CRs of an AR, plus the per-submodule `Cell` index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub input_cr: CrLayout,
    pub output_cr: CrLayout,
    pub cells: Vec<Cell>,
}

/// Errors building a `Layout` from an AR's parameters against a `DeviceModel`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LayoutError {
    #[error("unknown submodule at slot {slot}, subslot {subslot:#06x}")]
    UnknownSubmodule { slot: u16, subslot: u16 },
    #[error(
        "object at slot {slot}, subslot {subslot:#06x} does not fit: end {end} > data_length {data_length}"
    )]
    OutOfBounds {
        slot: u16,
        subslot: u16,
        end: usize,
        data_length: usize,
    },
    #[error("overlapping object at slot {slot}, subslot {subslot:#06x}")]
    Overlap { slot: u16, subslot: u16 },
}

/// Which side of the AR a CR carries data for: which model length (`input_len` vs
/// `output_len`) its IO data objects use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dir {
    Input,
    Output,
}

impl Layout {
    /// Build the C-SDU plan for both CRs of `params` against `model`.
    pub fn from_ar(params: &ArParams, model: &DeviceModel) -> Result<Layout, LayoutError> {
        let input_cr = build_cr(&params.input_cr, model, Dir::Input)?;
        let output_cr = build_cr(&params.output_cr, model, Dir::Output)?;

        let mut cells = Vec::new();
        for slot in &model.slots {
            for sm in &slot.submodules {
                let find_off = |cr: &CrLayout| {
                    cr.objects
                        .iter()
                        .find(|o| o.slot == slot.slot && o.subslot == sm.subslot)
                        .map(|o| o.data_off)
                };
                cells.push(Cell {
                    slot: slot.slot,
                    subslot: sm.subslot,
                    input_len: sm.input_len as usize,
                    output_len: sm.output_len as usize,
                    input_off: find_off(&input_cr),
                    output_off: find_off(&output_cr),
                });
            }
        }

        Ok(Layout {
            input_cr,
            output_cr,
            cells,
        })
    }
}

/// Mark `[off, off + len)` as occupied in `occupied`, or fail if any of those bytes
/// are already taken by another object.
fn mark(
    occupied: &mut [bool],
    off: usize,
    len: usize,
    slot: u16,
    subslot: u16,
) -> Result<(), LayoutError> {
    for b in &mut occupied[off..off + len] {
        if *b {
            return Err(LayoutError::Overlap { slot, subslot });
        }
        *b = true;
    }
    Ok(())
}

/// Build one CR's `CrLayout`: cycle timing plus its IO data and IOCS objects, each
/// checked to fit inside `data_length` and not overlap another object.
fn build_cr(cr: &IocrParams, model: &DeviceModel, dir: Dir) -> Result<CrLayout, LayoutError> {
    let data_length = cr.data_length as usize;
    let cycle_step =
        cr.send_clock_factor
            .checked_mul(cr.reduction_ratio)
            .ok_or(LayoutError::OutOfBounds {
                slot: 0,
                subslot: 0,
                end: u16::MAX as usize,
                data_length,
            })?;
    let watchdog = Duration::from_nanos(
        cr.data_hold_factor as u64 * cycle_step as u64 * CYCLE_UNIT.as_nanos() as u64,
    );

    let mut occupied = vec![false; data_length];
    let mut objects = Vec::with_capacity(cr.io_data.len());
    for obj in &cr.io_data {
        let sm = model
            .find(obj.slot, obj.subslot)
            .ok_or(LayoutError::UnknownSubmodule {
                slot: obj.slot,
                subslot: obj.subslot,
            })?;
        let len = match dir {
            Dir::Input => sm.input_len,
            Dir::Output => sm.output_len,
        } as usize;
        let data_off = obj.frame_offset as usize;
        let iops_off = data_off + len;
        let end = iops_off + 1;
        if end > data_length {
            return Err(LayoutError::OutOfBounds {
                slot: obj.slot,
                subslot: obj.subslot,
                end,
                data_length,
            });
        }
        mark(&mut occupied, data_off, len, obj.slot, obj.subslot)?;
        mark(&mut occupied, iops_off, 1, obj.slot, obj.subslot)?;
        objects.push(IoObject {
            slot: obj.slot,
            subslot: obj.subslot,
            data_off,
            data_len: len,
            iops_off,
        });
    }

    let mut iocs = Vec::with_capacity(cr.iocs.len());
    for obj in &cr.iocs {
        let iocs_off = obj.frame_offset as usize;
        let end = iocs_off + 1;
        if end > data_length {
            return Err(LayoutError::OutOfBounds {
                slot: obj.slot,
                subslot: obj.subslot,
                end,
                data_length,
            });
        }
        mark(&mut occupied, iocs_off, 1, obj.slot, obj.subslot)?;
        iocs.push(CsObject {
            slot: obj.slot,
            subslot: obj.subslot,
            iocs_off,
        });
    }

    Ok(CrLayout {
        frame_id: cr.frame_id,
        data_length,
        cycle_step,
        watchdog,
        objects,
        iocs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cm::{validate, ConnectReq, DeviceModel};
    use crate::eth::MacAddr;
    use crate::testutil::golden;
    use std::time::Duration;

    const MAC: MacAddr = MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]);

    fn layout() -> Layout {
        let model = DeviceModel::pnet_sample(MAC);
        let req = ConnectReq::parse(&golden("connect_req")[142..]).unwrap();
        let params = validate(&req, &model).unwrap();
        Layout::from_ar(&params, &model).unwrap()
    }

    #[test]
    fn input_cr_matches_bench_table() {
        let l = layout();
        let cr = &l.input_cr;
        assert_eq!(
            (cr.frame_id, cr.data_length, cr.cycle_step),
            (0x8000, 40, 1024)
        );
        assert_eq!(cr.watchdog, Duration::from_millis(96));
        assert_eq!(cr.period(), Duration::from_millis(32));
        let o = |slot, subslot| {
            cr.objects
                .iter()
                .find(|o| o.slot == slot && o.subslot == subslot)
                .unwrap()
        };
        assert_eq!(
            (o(0, 1).data_off, o(0, 1).data_len, o(0, 1).iops_off),
            (0, 0, 0)
        );
        assert_eq!((o(0, 0x8000).iops_off, o(0, 0x8001).iops_off), (1, 2));
        assert_eq!(
            (o(1, 1).data_off, o(1, 1).data_len, o(1, 1).iops_off),
            (3, 1, 4)
        );
        assert_eq!((o(3, 1).data_off, o(3, 1).iops_off), (6, 7));
        assert_eq!(
            (o(4, 1).data_off, o(4, 1).data_len, o(4, 1).iops_off),
            (9, 8, 17)
        );
        let c = |slot| cr.iocs.iter().find(|c| c.slot == slot).unwrap().iocs_off;
        assert_eq!((c(2), c(3), c(4)), (5, 8, 18));
    }

    #[test]
    fn output_cr_matches_bench_table() {
        let l = layout();
        let cr = &l.output_cr;
        assert_eq!(cr.frame_id, 0x8001);
        let o = |slot| cr.objects.iter().find(|o| o.slot == slot).unwrap();
        assert_eq!((o(2).data_off, o(2).data_len, o(2).iops_off), (4, 1, 5));
        assert_eq!((o(3).data_off, o(3).iops_off), (7, 8));
        assert_eq!((o(4).data_off, o(4).data_len, o(4).iops_off), (10, 8, 18));
        let c = |slot, subslot| {
            cr.iocs
                .iter()
                .find(|c| c.slot == slot && c.subslot == subslot)
                .unwrap()
                .iocs_off
        };
        assert_eq!(
            (
                c(0, 1),
                c(0, 0x8000),
                c(0, 0x8001),
                c(1, 1),
                c(3, 1),
                c(4, 1)
            ),
            (0, 1, 2, 3, 6, 9)
        );
    }

    #[test]
    fn cells_follow_the_model() {
        let l = layout();
        assert_eq!(l.cells.len(), 7);
        let echo = l.cells.iter().find(|c| c.slot == 4).unwrap();
        assert_eq!(
            (
                echo.input_len,
                echo.output_len,
                echo.input_off,
                echo.output_off
            ),
            (8, 8, Some(9), Some(10))
        );
        let di = l.cells.iter().find(|c| c.slot == 1).unwrap();
        assert_eq!((di.input_off, di.output_off), (Some(3), None));
    }

    #[test]
    fn out_of_bounds_and_unknown_are_errors() {
        let model = DeviceModel::pnet_sample(MAC);
        let req = ConnectReq::parse(&golden("connect_req")[142..]).unwrap();
        let mut params = validate(&req, &model).unwrap();
        params.input_cr.data_length = 10;
        assert!(matches!(
            Layout::from_ar(&params, &model),
            Err(LayoutError::OutOfBounds { .. })
        ));
        let mut model2 = model.clone();
        model2.slots.pop();
        let params = validate(&req, &model).unwrap();
        assert!(matches!(
            Layout::from_ar(&params, &model2),
            Err(LayoutError::UnknownSubmodule { slot: 4, .. })
        ));
    }
}

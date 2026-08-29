//! Shared I/O image: the buffer the application and the RT thread exchange each cycle.
//!
//! `inputs` (app → CPU) is one flat buffer laid out like the input C-SDU, filled by
//! the application via [`IoImage::write_inputs`] and read by the RT thread via
//! [`IoImage::rt_snapshot_inputs`]. `outputs` (CPU → app) holds the last accepted
//! output C-SDU verbatim plus its [`Validity`], written by the RT thread via
//! [`IoImage::rt_publish`] / [`IoImage::rt_set_validity`] (and, on the stop path, by
//! the application via the blocking [`IoImage::set_validity`]) and read by the
//! application via [`IoImage::read_outputs`] / [`IoImage::snapshot_outputs`].
//!
//! The application side may block briefly (`Mutex::lock`, poison-tolerant: a
//! panicking application thread must not brick the image). The RT side never blocks
//! (`try_lock`; any error — contention or poisoning — yields `false` and the caller
//! reuses its previous snapshot or defers the publish to the next cycle).

use std::sync::Mutex;
use std::time::Duration;

use thiserror::Error;

use super::layout::{Cell, Layout};

/// Consumer watchdog state as tracked by the RT engine, mirrored here for the
/// application to read alongside the data it gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WatchdogState {
    /// No frame has been accepted yet; the watchdog has nothing to measure against.
    #[default]
    NotArmed,
    /// Within the watchdog window since the last accepted frame.
    Ok,
    /// The watchdog window has been exceeded.
    Expired,
}

/// How the application should treat the current output image: a coarser summary of
/// [`Validity`] for the common case of "can I use this data".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// The watchdog has never been armed: no output data has ever been accepted.
    NoData,
    /// Recent, provider running: safe to use.
    Fresh,
    /// The provider is not (or no longer) running; the data may be stale garbage.
    Stopped,
    /// The consumer watchdog has expired: no accepted frame within the window.
    Stale,
}

/// Per-cycle metadata attached to the output image: what the last accepted (or
/// timed-out) frame told us.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Validity {
    /// The provider's `DataStatus.Provider_State` bit from the last accepted frame.
    pub provider_run: bool,
    /// The provider's `DataStatus.State_Primary` bit from the last accepted frame.
    pub primary: bool,
    /// Consumer watchdog state.
    pub watchdog: WatchdogState,
    /// Age of the data at the moment it was last accepted, if any frame ever was.
    pub last_rx_age: Option<Duration>,
    /// Engine cycle counter at the moment this validity was recorded.
    pub cycle: u64,
}

impl Validity {
    /// Summarize this validity into a [`Freshness`] the application can switch on.
    pub fn freshness(&self) -> Freshness {
        if self.watchdog == WatchdogState::NotArmed {
            Freshness::NoData
        } else if self.watchdog == WatchdogState::Expired {
            Freshness::Stale
        } else if !self.provider_run {
            Freshness::Stopped
        } else {
            Freshness::Fresh
        }
    }
}

/// Errors from the application-side accessors of [`IoImage`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ImageError {
    #[error("unknown submodule at slot {slot}, subslot {subslot:#06x}")]
    UnknownSubmodule { slot: u16, subslot: u16 },
    #[error("length mismatch: expected {expected}, got {got}")]
    LengthMismatch { expected: usize, got: usize },
    #[error("no input data at slot {slot}, subslot {subslot:#06x}")]
    NoInput { slot: u16, subslot: u16 },
    #[error("no output data at slot {slot}, subslot {subslot:#06x}")]
    NoOutput { slot: u16, subslot: u16 },
}

/// The output C-SDU held verbatim plus the validity of the frame it came from.
pub(crate) struct Outputs {
    csdu: Vec<u8>,
    validity: Validity,
}

/// The shared I/O image: one input buffer, one output buffer, and the per-submodule
/// cell index used to translate `(slot, subslot)` into offsets in each.
///
/// Cheap to share behind an `Arc`: all accessors take `&self`.
pub struct IoImage {
    cells: Mutex<Vec<Cell>>,
    /// `pub(crate)` so tests (and nothing else) can lock it directly to simulate
    /// contention with the RT thread.
    pub(crate) inputs: Mutex<Vec<u8>>,
    /// `pub(crate)` so tests (and nothing else) can lock it directly to simulate
    /// contention with the RT thread.
    pub(crate) outputs: Mutex<Outputs>,
}

impl IoImage {
    /// Build an image sized and indexed from `layout`: `inputs` zeroed to the input
    /// CR's `data_length`, `outputs.csdu` zeroed to the output CR's `data_length`.
    pub fn new(layout: &Layout) -> IoImage {
        IoImage {
            cells: Mutex::new(layout.cells.clone()),
            inputs: Mutex::new(vec![0u8; layout.input_cr.data_length]),
            outputs: Mutex::new(Outputs {
                csdu: vec![0u8; layout.output_cr.data_length],
                validity: Validity::default(),
            }),
        }
    }

    /// An image with no cells and empty buffers: every accessor returns
    /// [`ImageError::UnknownSubmodule`] until [`IoImage::rebuild`] gives it a layout.
    pub fn empty() -> IoImage {
        IoImage {
            cells: Mutex::new(Vec::new()),
            inputs: Mutex::new(Vec::new()),
            outputs: Mutex::new(Outputs {
                csdu: Vec::new(),
                validity: Validity::default(),
            }),
        }
    }

    /// Replace the cell index and both buffers under their locks, as if freshly built
    /// from `layout` via [`IoImage::new`]. Used by `device` on each new `Data` (AR
    /// re-negotiation).
    pub fn rebuild(&self, layout: &Layout) {
        let mut cells = self.cells.lock().unwrap_or_else(|e| e.into_inner());
        let mut inputs = self.inputs.lock().unwrap_or_else(|e| e.into_inner());
        let mut outputs = self.outputs.lock().unwrap_or_else(|e| e.into_inner());
        *cells = layout.cells.clone();
        *inputs = vec![0u8; layout.input_cr.data_length];
        *outputs = Outputs {
            csdu: vec![0u8; layout.output_cr.data_length],
            validity: Validity::default(),
        };
    }

    /// Drop the layout: called when the RT runner stops. The cell index and both
    /// buffers are emptied — every accessor keyed by `(slot, subslot)` reports
    /// [`ImageError::UnknownSubmodule`] (the `NoLayoutYet`-class error at the
    /// `api::IoDevice` facade) until the next [`IoImage::rebuild`] — but the
    /// validity is left untouched: the caller (`device::Device::stop_runner`) sets it
    /// to reflect the stopped runner (watchdog `Expired`, `provider_run = false`)
    /// just before calling this, and that must survive.
    ///
    /// Same lock order as [`IoImage::rebuild`] (`cells` before `inputs` before
    /// `outputs`), for the same TOCTOU-vs-concurrent-app-access reasoning.
    pub fn clear(&self) {
        let mut cells = self.cells.lock().unwrap_or_else(|e| e.into_inner());
        let mut inputs = self.inputs.lock().unwrap_or_else(|e| e.into_inner());
        let mut outputs = self.outputs.lock().unwrap_or_else(|e| e.into_inner());
        cells.clear();
        inputs.clear();
        outputs.csdu.clear();
    }

    /// A clone of the current cell index, in model order.
    pub fn cells(&self) -> Vec<Cell> {
        self.cells.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Write `bytes` into the input image at `(slot, subslot)`'s offset. `bytes.len()`
    /// must exactly match the submodule's input length.
    ///
    /// Holds the `cells` lock for the whole operation (same acquisition order as
    /// [`IoImage::rebuild`]: `cells` before `inputs`) so a concurrent `rebuild` can
    /// never swap the buffer out from under an offset looked up against the old cell
    /// index — the TOCTOU a lookup-then-release-then-lock sequence would allow.
    pub fn write_inputs(&self, slot: u16, subslot: u16, bytes: &[u8]) -> Result<(), ImageError> {
        let cells = self.cells.lock().unwrap_or_else(|e| e.into_inner());
        let cell = cells
            .iter()
            .find(|c| c.slot == slot && c.subslot == subslot)
            .ok_or(ImageError::UnknownSubmodule { slot, subslot })?;
        let off = cell
            .input_off
            .ok_or(ImageError::NoInput { slot, subslot })?;
        let len = cell.input_len;
        if bytes.len() != len {
            return Err(ImageError::LengthMismatch {
                expected: len,
                got: bytes.len(),
            });
        }
        let mut inputs = self.inputs.lock().unwrap_or_else(|e| e.into_inner());
        if off + len > inputs.len() {
            return Err(ImageError::LengthMismatch {
                expected: len,
                got: inputs.len().saturating_sub(off),
            });
        }
        inputs[off..off + len].copy_from_slice(bytes);
        Ok(())
    }

    /// Call `f` with the current output image slice for `(slot, subslot)` and the
    /// validity of the frame it was published from.
    ///
    /// The cell lookup and bounds check happen under the `cells`/`outputs` locks (same
    /// TOCTOU-vs-`rebuild` reasoning as [`IoImage::write_inputs`]), but both locks are
    /// released *before* `f` runs: `f` executes with no `IoImage` lock held, so it may
    /// freely call back into this image (e.g. `write_inputs` to mirror the read data
    /// into the input image) without deadlocking. `std::sync::Mutex` is not reentrant,
    /// so this matters even for callbacks that stay on the calling thread.
    pub fn read_outputs<T>(
        &self,
        slot: u16,
        subslot: u16,
        f: impl FnOnce(&[u8], &Validity) -> T,
    ) -> Result<T, ImageError> {
        let (bytes, validity) = {
            let cells = self.cells.lock().unwrap_or_else(|e| e.into_inner());
            let cell = cells
                .iter()
                .find(|c| c.slot == slot && c.subslot == subslot)
                .ok_or(ImageError::UnknownSubmodule { slot, subslot })?;
            let off = cell
                .output_off
                .ok_or(ImageError::NoOutput { slot, subslot })?;
            let len = cell.output_len;
            let outputs = self.outputs.lock().unwrap_or_else(|e| e.into_inner());
            if off + len > outputs.csdu.len() {
                return Err(ImageError::LengthMismatch {
                    expected: len,
                    got: outputs.csdu.len().saturating_sub(off),
                });
            }
            (outputs.csdu[off..off + len].to_vec(), outputs.validity)
        };
        Ok(f(&bytes, &validity))
    }

    /// Copy the whole output C-SDU (`min(dst.len(), csdu.len())` bytes) into `dst` and
    /// return the current validity.
    pub fn snapshot_outputs(&self, dst: &mut [u8]) -> Validity {
        let outputs = self.outputs.lock().unwrap_or_else(|e| e.into_inner());
        let n = dst.len().min(outputs.csdu.len());
        dst[..n].copy_from_slice(&outputs.csdu[..n]);
        outputs.validity
    }

    /// The current output validity, without touching the data.
    pub fn validity(&self) -> Validity {
        self.outputs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .validity
    }

    /// Blocking: store `validity` directly, without touching the output data — the
    /// application-side counterpart of [`IoImage::rt_set_validity`]. Used by `device`
    /// to mark the image stale once its RT runner has stopped; poison-tolerant like
    /// every other app-side accessor (a panicking application thread must not brick
    /// the image).
    pub fn set_validity(&self, validity: Validity) {
        let mut outputs = self.outputs.lock().unwrap_or_else(|e| e.into_inner());
        outputs.validity = validity;
    }

    /// Non-blocking: copy the whole input C-SDU (`min(dst.len(), inputs.len())` bytes)
    /// into `dst`. Returns `false` (nothing copied) if the application currently holds
    /// the lock; the RT thread should reuse its previous snapshot for this cycle.
    pub fn rt_snapshot_inputs(&self, dst: &mut [u8]) -> bool {
        match self.inputs.try_lock() {
            Ok(inputs) => {
                let n = dst.len().min(inputs.len());
                dst[..n].copy_from_slice(&inputs[..n]);
                true
            }
            Err(_) => false,
        }
    }

    /// Non-blocking: store `csdu` (`min(csdu.len(), outputs.csdu.len())` bytes) and
    /// `validity` as the current output image. Returns `false` (deferred) if the
    /// application currently holds the lock; the RT thread should retry next cycle.
    pub fn rt_publish(&self, csdu: &[u8], validity: Validity) -> bool {
        match self.outputs.try_lock() {
            Ok(mut outputs) => {
                let n = csdu.len().min(outputs.csdu.len());
                outputs.csdu[..n].copy_from_slice(&csdu[..n]);
                outputs.validity = validity;
                true
            }
            Err(_) => false,
        }
    }

    /// Non-blocking: store `validity` without touching the output data — used after a
    /// watchdog verdict that didn't accept new data. Returns `false` (deferred) if the
    /// application currently holds the lock.
    pub fn rt_set_validity(&self, validity: Validity) -> bool {
        match self.outputs.try_lock() {
            Ok(mut outputs) => {
                outputs.validity = validity;
                true
            }
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cm::{validate, ConnectReq, DeviceModel};
    use crate::eth::MacAddr;
    use crate::rt::layout::Layout;
    use crate::testutil::golden;
    use std::time::Duration;

    fn layout() -> Layout {
        let model = DeviceModel::pnet_sample(MacAddr([0x8c, 0xf3, 0x19, 0xcd, 0x19, 0xf8]));
        let req = ConnectReq::parse(&golden("connect_req")[142..]).unwrap();
        Layout::from_ar(&validate(&req, &model).unwrap(), &model).unwrap()
    }
    fn fresh() -> Validity {
        Validity {
            provider_run: true,
            primary: true,
            watchdog: WatchdogState::Ok,
            last_rx_age: Some(Duration::from_millis(1)),
            cycle: 7,
        }
    }

    #[test]
    fn app_writes_land_in_the_rt_snapshot_at_layout_offsets() {
        let img = IoImage::new(&layout());
        img.write_inputs(1, 1, &[0xa5]).unwrap();
        img.write_inputs(4, 1, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        let mut snap = vec![0u8; 40];
        assert!(img.rt_snapshot_inputs(&mut snap));
        assert_eq!(snap[3], 0xa5);
        assert_eq!(&snap[9..17], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            img.write_inputs(1, 1, &[1, 2]).unwrap_err(),
            ImageError::LengthMismatch {
                expected: 1,
                got: 2
            }
        );
        assert_eq!(
            img.write_inputs(2, 1, &[1]).unwrap_err(),
            ImageError::NoInput {
                slot: 2,
                subslot: 1
            }
        );
        assert_eq!(
            img.write_inputs(9, 1, &[1]).unwrap_err(),
            ImageError::UnknownSubmodule {
                slot: 9,
                subslot: 1
            }
        );
    }

    #[test]
    fn published_outputs_are_readable_per_cell_with_validity() {
        let img = IoImage::new(&layout());
        let mut csdu = vec![0u8; 40];
        csdu[4] = 0x01;
        csdu[10..18].copy_from_slice(&[0x12, 0x34, 0x56, 0x78, 0x3f, 0xc0, 0x00, 0x00]);
        assert!(img.rt_publish(&csdu, fresh()));
        let (qb0, v) = img.read_outputs(2, 1, |b, v| (b.to_vec(), *v)).unwrap();
        assert_eq!(qb0, vec![0x01]);
        assert_eq!(v.freshness(), Freshness::Fresh);
        let echo = img
            .read_outputs(4, 1, |b, _| crate::data::decode_f32(&b[4..8]).unwrap())
            .unwrap();
        assert_eq!(echo, 1.5);
        assert_eq!(
            img.read_outputs(1, 1, |_, _| ()).unwrap_err(),
            ImageError::NoOutput {
                slot: 1,
                subslot: 1
            }
        );
    }

    #[test]
    fn freshness_states() {
        let img = IoImage::new(&layout());
        assert_eq!(img.validity().freshness(), Freshness::NoData);
        let mut v = fresh();
        v.provider_run = false;
        assert!(img.rt_set_validity(v));
        assert_eq!(img.validity().freshness(), Freshness::Stopped);
        v.provider_run = true;
        v.watchdog = WatchdogState::Expired;
        assert!(img.rt_set_validity(v));
        assert_eq!(img.validity().freshness(), Freshness::Stale);
    }

    #[test]
    fn set_validity_is_the_blocking_app_side_counterpart() {
        let img = IoImage::new(&layout());
        assert_eq!(img.validity().freshness(), Freshness::NoData);
        // Blocks (rather than deferring like `rt_set_validity`) and is poison-tolerant.
        img.set_validity(fresh());
        assert_eq!(img.validity().freshness(), Freshness::Fresh);
        let mut stale = fresh();
        stale.watchdog = WatchdogState::Expired;
        stale.provider_run = false;
        img.set_validity(stale);
        assert_eq!(img.validity(), stale);
        assert_eq!(img.validity().freshness(), Freshness::Stale);

        // Poisoning the lock (a panic while held) must not brick later access.
        use std::sync::Arc;
        let img = Arc::new(IoImage::new(&layout()));
        let img2 = Arc::clone(&img);
        let _ = std::thread::spawn(move || {
            let _guard = img2.outputs.lock().unwrap();
            panic!("simulated app-side panic while holding the outputs lock");
        })
        .join();
        img.set_validity(fresh());
        assert_eq!(img.validity().freshness(), Freshness::Fresh);
    }

    #[test]
    fn rt_side_never_blocks_under_contention() {
        let img = IoImage::new(&layout());
        let guard = img.inputs.lock().unwrap(); // application holds the lock
        let mut snap = vec![0u8; 40];
        assert!(!img.rt_snapshot_inputs(&mut snap));
        drop(guard);
        assert!(img.rt_snapshot_inputs(&mut snap));
        let guard = img.outputs.lock().unwrap();
        assert!(!img.rt_publish(&[0u8; 40], fresh()));
        drop(guard);
        assert!(img.rt_publish(&[0u8; 40], fresh()));
    }

    #[test]
    fn rebuild_during_app_access_cannot_index_out_of_bounds() {
        use std::sync::Arc;
        use std::thread;

        let full = layout();
        let mut small = full.clone();
        small.cells = Vec::new();
        small.input_cr.data_length = 12;
        small.output_cr.data_length = 12;
        let full_for_thread = full.clone();

        let img = Arc::new(IoImage::new(&full));
        let img2 = Arc::clone(&img);
        let handle = thread::spawn(move || {
            for _ in 0..200 {
                img2.rebuild(&small);
                img2.rebuild(&full_for_thread);
            }
        });

        for _ in 0..200 {
            match img.write_inputs(4, 1, &[0; 8]) {
                Ok(())
                | Err(ImageError::UnknownSubmodule { .. })
                | Err(ImageError::LengthMismatch { .. }) => {}
                Err(e) => panic!("unexpected error: {e:?}"),
            }
            match img.read_outputs(4, 1, |b, _| b.len()) {
                Ok(_)
                | Err(ImageError::UnknownSubmodule { .. })
                | Err(ImageError::LengthMismatch { .. }) => {}
                Err(e) => panic!("unexpected error: {e:?}"),
            }
        }

        handle.join().unwrap();
    }

    #[test]
    fn read_outputs_closure_may_write_inputs() {
        let img = IoImage::new(&layout());
        let mut csdu = vec![0u8; 40];
        csdu[4] = 0x5a;
        assert!(img.rt_publish(&csdu, fresh()));
        img.read_outputs(2, 1, |b, _| img.write_inputs(1, 1, b).unwrap())
            .unwrap();
        let mut snap = vec![0u8; 40];
        assert!(img.rt_snapshot_inputs(&mut snap));
        assert_eq!(snap[3], 0x5a);
    }

    #[test]
    fn empty_then_rebuild() {
        let img = IoImage::empty();
        assert_eq!(
            img.write_inputs(1, 1, &[1]).unwrap_err(),
            ImageError::UnknownSubmodule {
                slot: 1,
                subslot: 1
            }
        );
        img.rebuild(&layout());
        img.write_inputs(1, 1, &[1]).unwrap();
        assert_eq!(img.cells().len(), 7);
    }

    #[test]
    fn clear_drops_the_layout_but_not_the_validity() {
        let img = IoImage::new(&layout());
        img.rebuild(&layout());
        assert_eq!(img.cells().len(), 7);
        // The validity `device::Device::stop_runner` just set (watchdog Expired,
        // provider_run false) must survive `clear`.
        let mut v = fresh();
        v.watchdog = WatchdogState::Expired;
        v.provider_run = false;
        img.set_validity(v);

        img.clear();

        assert!(img.cells().is_empty());
        assert_eq!(
            img.write_inputs(1, 1, &[1]).unwrap_err(),
            ImageError::UnknownSubmodule {
                slot: 1,
                subslot: 1
            }
        );
        assert_eq!(
            img.read_outputs(1, 1, |_, _| ()).unwrap_err(),
            ImageError::UnknownSubmodule {
                slot: 1,
                subslot: 1
            }
        );
        assert_eq!(img.validity(), v);
        assert_eq!(img.validity().freshness(), Freshness::Stale);
    }
}

//! Typed facade over `device::Device` + `rt::IoImage` (spec §6): start the device from
//! a [`DeviceConfig`] in one call, read the controller's outputs and write our inputs
//! by (slot, index) with the config's field table. The RT path is untouched.

use crate::cm::{AbortReason, ArParams, ArState};
use crate::config::{DeviceConfig, Direction, FieldRef, Slot};
use crate::data::{CodecError, FieldType, Value};
use crate::device::{Device, DeviceError, RtOptions};
use crate::eth::{bpf::acyclic_filter, AfPacketTransport, EthTransport, MacAddr};
use crate::rpc::{RpcTransport, UdpRpcTransport, PNIO_UDP_PORT};
use crate::rt::{
    Freshness, ImageError, IoImage, RtConfig, RtError, RtHandle, RtStats, StatsSnapshot, Validity,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use thiserror::Error;

/// Options for [`IoDevice::start`]: the real interface to open and how the RT thread
/// (if any) should be scheduled.
#[derive(Debug, Clone)]
pub struct StartOptions {
    pub iface: String,
    pub ip: [u8; 4],
    pub rt: Option<RtOptions>,
    /// CPUs for the acyclic thread (and anything the application spawns from it).
    pub app_cpus: Option<Vec<usize>>,
}

/// Errors from the [`IoDevice`] facade: config-table lookups (slot/index/type/direction),
/// the image not being laid out yet, and the lower layers' own errors wrapped through.
#[derive(Debug, Error)]
pub enum ApiError {
    #[error("slot {0} is not declared")]
    UnknownSlot(u16),
    #[error("slot {slot} index {index} out of range (len {len})")]
    IndexOutOfRange { slot: u16, index: usize, len: usize },
    #[error("slot {slot} index {index} is {expected:?}, not {got:?}")]
    TypeMismatch {
        slot: u16,
        index: usize,
        expected: FieldType,
        got: FieldType,
    },
    #[error("slot {slot} has no {expected:?} data")]
    WrongDirection { slot: u16, expected: Direction },
    /// Can still occur for a few microseconds after [`IoDevice::ar_state`] first
    /// reports [`ArState::Data`] — see that method's doc. Poll [`IoDevice::ready`]
    /// instead of `ar_state() == Data` to avoid it.
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

/// Hand-implemented rather than derived: `Device` and `Io` wrap error types with no
/// `PartialEq` of their own (`std::io::Error` has none, and `DeviceError` wraps it
/// transitively through `TransportError`), so they — and, for consistency,
/// `Image`/`Codec`, which do have one — are compared by their `Debug` rendering
/// instead. Every other variant carries only `Copy` data and is compared structurally.
impl PartialEq for ApiError {
    fn eq(&self, other: &Self) -> bool {
        use ApiError::*;
        match (self, other) {
            (UnknownSlot(a), UnknownSlot(b)) => a == b,
            (
                IndexOutOfRange {
                    slot: s1,
                    index: i1,
                    len: l1,
                },
                IndexOutOfRange {
                    slot: s2,
                    index: i2,
                    len: l2,
                },
            ) => (s1, i1, l1) == (s2, i2, l2),
            (
                TypeMismatch {
                    slot: s1,
                    index: i1,
                    expected: e1,
                    got: g1,
                },
                TypeMismatch {
                    slot: s2,
                    index: i2,
                    expected: e2,
                    got: g2,
                },
            ) => (s1, i1, e1, g1) == (s2, i2, e2, g2),
            (
                WrongDirection {
                    slot: s1,
                    expected: e1,
                },
                WrongDirection {
                    slot: s2,
                    expected: e2,
                },
            ) => s1 == s2 && e1 == e2,
            (NoLayoutYet, NoLayoutYet) => true,
            (Image(_), Image(_))
            | (Codec(_), Codec(_))
            | (Device(_), Device(_))
            | (Io(_), Io(_)) => format!("{self:?}") == format!("{other:?}"),
            _ => false,
        }
    }
}

impl From<ImageError> for ApiError {
    /// An `UnknownSubmodule` from the image means the AR has never reached `Data`
    /// (the empty image) or has since dropped back to `Idle` (a stale one) — either
    /// way there is no layout right now, which is a distinct, more actionable error
    /// than "this slot doesn't exist" (already caught earlier by the config lookup).
    fn from(e: ImageError) -> Self {
        match e {
            ImageError::UnknownSubmodule { .. } => ApiError::NoLayoutYet,
            e => ApiError::Image(e),
        }
    }
}

/// Everything a running [`Device`]'s state-change callback reports, shared with the
/// application thread behind a `Mutex`.
struct Shared {
    state: Mutex<(ArState, Option<AbortReason>)>,
}

/// A running PROFINET IO-Device: owns the acyclic loop (on its own thread) and the RT
/// thread `Device` starts/stops around it, and exposes typed (slot, index) reads of the
/// controller's outputs and writes of our inputs over the config's field table.
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
    /// Opens a real `AF_PACKET`/UDP device on `opts.iface`, reading its MAC from
    /// `/sys/class/net/<iface>/address`.
    pub fn start(cfg: DeviceConfig, opts: StartOptions) -> Result<IoDevice, ApiError> {
        let mac = read_mac(&opts.iface)?;
        let eth = AfPacketTransport::open(&opts.iface).map_err(io_err)?;
        eth.attach_filter(&acyclic_filter()).map_err(io_err)?;
        let rpc = UdpRpcTransport::bind(std::net::SocketAddr::from(([0, 0, 0, 0], PNIO_UDP_PORT)))
            .map_err(|e| ApiError::Io(std::io::Error::other(e.to_string())))?;
        let app_cpus = opts.app_cpus.clone();
        Self::start_inner(
            cfg,
            mac,
            opts.ip,
            opts.rt,
            eth,
            rpc,
            crate::rt::RtRunner::spawn,
            app_cpus,
        )
    }

    /// Test/embedding hook: any transports, any runner factory.
    #[doc(hidden)]
    pub fn start_with<E, R>(
        cfg: DeviceConfig,
        mac: MacAddr,
        ip: [u8; 4],
        rt: Option<RtOptions>,
        eth: E,
        rpc: R,
        runner: impl Fn(RtConfig) -> Result<RtHandle, RtError> + Send + 'static,
    ) -> Result<IoDevice, ApiError>
    where
        E: EthTransport + 'static,
        R: RpcTransport + 'static,
    {
        Self::start_inner(cfg, mac, ip, rt, eth, rpc, runner, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn start_inner<E, R>(
        cfg: DeviceConfig,
        mac: MacAddr,
        ip: [u8; 4],
        rt: Option<RtOptions>,
        eth: E,
        rpc: R,
        runner: impl Fn(RtConfig) -> Result<RtHandle, RtError> + Send + 'static,
        app_cpus: Option<Vec<usize>>,
    ) -> Result<IoDevice, ApiError>
    where
        E: EthTransport + 'static,
        R: RpcTransport + 'static,
    {
        let cfg = Arc::new(cfg);
        let mut dev = Device::new(cfg.setup(mac, ip, rt), eth, rpc);
        dev.with_runner_factory(runner);
        let shared = Arc::new(Shared {
            state: Mutex::new((ArState::Idle, None)),
        });
        let params: Arc<Mutex<Option<ArParams>>> = Arc::new(Mutex::new(None));
        {
            let shared = shared.clone();
            dev.on_state_change(move |st, why| {
                *shared.state.lock().unwrap_or_else(|e| e.into_inner()) = (st, why);
            });
        }
        let image = dev.image();
        let stats = dev.rt_stats();
        let stop = Arc::new(AtomicBool::new(false));
        let inputs = cfg
            .submodules()
            .iter()
            .map(|s| Mutex::new(vec![0u8; cfg.input_len(s.slot).unwrap_or(0) as usize]))
            .collect();
        let thread = {
            let stop = stop.clone();
            let params = params.clone();
            std::thread::Builder::new()
                .name("pnio-acyclic".into())
                .spawn(move || {
                    if let Some(cpus) = app_cpus {
                        if let Err(e) = crate::rt::sched::set_affinity(&cpus) {
                            log::warn!("acyclic affinity {cpus:?}: {e}");
                        }
                    }
                    let r = run_publishing_params(&mut dev, &stop, &params);
                    drop(dev); // stops and joins the RT runner before the thread ends
                    r
                })
                .map_err(ApiError::from)?
        };
        Ok(IoDevice {
            cfg,
            image,
            stats,
            shared,
            stop,
            inputs,
            thread: Mutex::new(Some(thread)),
            params,
        })
    }

    pub fn config(&self) -> &DeviceConfig {
        &self.cfg
    }
    pub fn image(&self) -> Arc<IoImage> {
        self.image.clone()
    }
    /// The AR's current state. Note: `ar_state() == ArState::Data` does *not* imply
    /// the I/O image is laid out yet — `Device::dispatch` reports the `Data`
    /// notification (what this reads) *before* calling `start_runner` (which rebuilds
    /// the image) — so a reader/writer can still see [`ApiError::NoLayoutYet`] for a
    /// few microseconds after this first reports `Data`. Use [`IoDevice::ready`] to
    /// wait for both.
    pub fn ar_state(&self) -> ArState {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .0
    }
    /// `ar_state() == Data` *and* the I/O image has actually been rebuilt from the
    /// negotiated layout — the condition a caller should poll for before the first
    /// read/write, since `ar_state()` alone can transiently lag it (see its doc).
    pub fn ready(&self) -> bool {
        self.ar_state() == ArState::Data && !self.image.cells().is_empty()
    }
    pub fn last_abort(&self) -> Option<AbortReason> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .1
    }
    /// The current AR's negotiated parameters, if the acyclic thread has reported one.
    /// Test/embedding hook, needed to build layout-derived offsets against the live AR.
    #[doc(hidden)]
    pub fn ar_params(&self) -> Option<ArParams> {
        self.params
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn validity(&self) -> Validity {
        self.image.validity()
    }
    pub fn freshness(&self) -> Freshness {
        self.image.validity().freshness()
    }
    pub fn stats(&self) -> StatsSnapshot {
        self.stats.snapshot()
    }
    pub fn rt_stats(&self) -> Arc<RtStats> {
        self.stats.clone()
    }

    // ----- lookups -----
    fn index_of(&self, slot: Slot) -> Result<usize, ApiError> {
        self.cfg
            .submodules()
            .iter()
            .position(|s| s.slot == slot)
            .ok_or(ApiError::UnknownSlot(slot.0))
    }

    /// Resolve one field, checking existence, direction and (if `want` is given) type
    /// in that order. `dir` is the direction *this accessor* looks the field up in
    /// (`Output` for reads, `Input` for writes); on a direction mismatch the error
    /// reports the slot's *declared* direction, not `dir` — `dev.write_real(Slot(3),
    /// ..)` on a slot declared `output` reports `expected: Output`, not `Input`.
    fn field(
        &self,
        slot: Slot,
        dir: Direction,
        index: usize,
        want: Option<FieldType>,
    ) -> Result<FieldRef, ApiError> {
        let sm = self
            .cfg
            .submodule(slot)
            .ok_or(ApiError::UnknownSlot(slot.0))?;
        let fields = self.cfg.fields(slot, dir).ok_or(ApiError::WrongDirection {
            slot: slot.0,
            expected: sm.direction(),
        })?;
        let f = *fields.get(index).ok_or(ApiError::IndexOutOfRange {
            slot: slot.0,
            index,
            len: fields.len(),
        })?;
        if let Some(w) = want {
            if w != f.ty {
                return Err(ApiError::TypeMismatch {
                    slot: slot.0,
                    index,
                    expected: f.ty,
                    got: w,
                });
            }
        }
        Ok(f)
    }

    // ----- controller -> device -----
    pub fn read(&self, slot: Slot, index: usize) -> Result<Value, ApiError> {
        let f = self.field(slot, Direction::Output, index, None)?;
        let r = self.image.read_outputs(slot.0, 1, |b, _| {
            Value::decode(f.ty, &b[f.byte as usize..], f.bit as usize)
        })?;
        Ok(r?)
    }
    pub fn read_bool(&self, s: Slot, i: usize) -> Result<bool, ApiError> {
        self.typed(s, i, FieldType::Bool).map(|v| match v {
            Value::Bool(b) => b,
            _ => unreachable!(),
        })
    }
    pub fn read_int(&self, s: Slot, i: usize) -> Result<i16, ApiError> {
        self.typed(s, i, FieldType::Int).map(|v| match v {
            Value::Int(x) => x,
            _ => unreachable!(),
        })
    }
    pub fn read_word(&self, s: Slot, i: usize) -> Result<u16, ApiError> {
        self.typed(s, i, FieldType::Word).map(|v| match v {
            Value::Word(x) => x,
            _ => unreachable!(),
        })
    }
    pub fn read_dint(&self, s: Slot, i: usize) -> Result<i32, ApiError> {
        self.typed(s, i, FieldType::Dint).map(|v| match v {
            Value::Dint(x) => x,
            _ => unreachable!(),
        })
    }
    pub fn read_real(&self, s: Slot, i: usize) -> Result<f32, ApiError> {
        self.typed(s, i, FieldType::Real).map(|v| match v {
            Value::Real(x) => x,
            _ => unreachable!(),
        })
    }
    fn typed(&self, slot: Slot, index: usize, want: FieldType) -> Result<Value, ApiError> {
        let f = self.field(slot, Direction::Output, index, Some(want))?;
        let r = self.image.read_outputs(slot.0, 1, |b, _| {
            Value::decode(f.ty, &b[f.byte as usize..], f.bit as usize)
        })?;
        Ok(r?)
    }
    /// A consistent copy of one slot's output bytes plus the validity of that cycle.
    pub fn outputs(&self, slot: Slot) -> Result<SlotSnapshot, ApiError> {
        let sm = self
            .cfg
            .submodule(slot)
            .ok_or(ApiError::UnknownSlot(slot.0))?;
        let fields: Arc<[FieldRef]> = self
            .cfg
            .fields(slot, Direction::Output)
            .ok_or(ApiError::WrongDirection {
                slot: slot.0,
                expected: sm.direction(),
            })?
            .into();
        let (bytes, validity) = self
            .image
            .read_outputs(slot.0, 1, |b, v| (b.to_vec(), *v))?;
        Ok(SlotSnapshot {
            slot,
            bytes,
            validity,
            fields,
        })
    }

    // ----- device -> controller -----
    pub fn write(&self, slot: Slot, index: usize, v: Value) -> Result<(), ApiError> {
        self.with_inputs(slot, |w| w.set(index, v))
    }
    pub fn write_bool(&self, s: Slot, i: usize, v: bool) -> Result<(), ApiError> {
        self.write(s, i, Value::Bool(v))
    }
    pub fn write_int(&self, s: Slot, i: usize, v: i16) -> Result<(), ApiError> {
        self.write(s, i, Value::Int(v))
    }
    pub fn write_word(&self, s: Slot, i: usize, v: u16) -> Result<(), ApiError> {
        self.write(s, i, Value::Word(v))
    }
    pub fn write_dint(&self, s: Slot, i: usize, v: i32) -> Result<(), ApiError> {
        self.write(s, i, Value::Dint(v))
    }
    pub fn write_real(&self, s: Slot, i: usize, v: f32) -> Result<(), ApiError> {
        self.write(s, i, Value::Real(v))
    }
    /// Modify several fields of one input slot and publish them in one go (same frame).
    ///
    /// `f` runs on a scratch copy of the slot's working buffer, not the buffer in
    /// place: on `Err` (or a panicking unwind out of `f`) the working copy — and so
    /// the image — is left exactly as it was before the call, never partially
    /// overwritten by whichever fields `f` got to before failing. The per-slot lock
    /// is held for the whole call, unlike [`IoImage::read_outputs`]'s callback (which
    /// releases its locks first specifically so it can be reentered): `f` must not
    /// call back into this `IoDevice` for `slot` (another `with_inputs`/`write_*` on
    /// it), since `std::sync::Mutex` is not reentrant.
    pub fn with_inputs<T>(
        &self,
        slot: Slot,
        f: impl FnOnce(&mut SlotWriter<'_>) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        let i = self.index_of(slot)?;
        let sm = &self.cfg.submodules()[i];
        let fields = self
            .cfg
            .fields(slot, Direction::Input)
            .ok_or(ApiError::WrongDirection {
                slot: slot.0,
                expected: sm.direction(),
            })?;
        let mut buf = self.inputs[i].lock().unwrap_or_else(|e| e.into_inner());
        let mut scratch = buf.clone();
        let mut w = SlotWriter {
            slot,
            fields,
            bytes: &mut scratch,
        };
        let out = f(&mut w)?;
        *buf = scratch;
        self.image.write_inputs(slot.0, 1, &buf)?;
        Ok(out)
    }

    pub fn stop(self) -> Result<(), DeviceError> {
        self.stop.store(true, Ordering::Relaxed);
        let h = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        match h {
            Some(h) => h.join().unwrap_or(Ok(())),
            None => Ok(()),
        }
    }
}

impl Drop for IoDevice {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = h.join();
        }
    }
}

/// `Device::run` with the AR parameters mirrored into `params` on every state change.
fn run_publishing_params<E: EthTransport, R: RpcTransport>(
    dev: &mut Device<E, R>,
    stop: &AtomicBool,
    params: &Mutex<Option<ArParams>>,
) -> Result<(), DeviceError> {
    // Same loop as Device::run (200 ms poll), stepping so we can observe ar_params():
    use std::time::{Duration, Instant};
    let mut last = None;
    while !stop.load(Ordering::Relaxed) {
        dev.step(Instant::now(), Some(Duration::from_millis(200)))?;
        let p = dev.ar_params();
        if p != last {
            *params.lock().unwrap_or_else(|e| e.into_inner()) = p.clone();
            last = p;
        }
    }
    Ok(())
}

/// A consistent copy of one output slot's bytes, decoded on demand.
pub struct SlotSnapshot {
    pub slot: Slot,
    bytes: Vec<u8>,
    pub validity: Validity,
    fields: Arc<[FieldRef]>,
}
impl SlotSnapshot {
    pub fn get(&self, index: usize) -> Result<Value, ApiError> {
        let f = *self.fields.get(index).ok_or(ApiError::IndexOutOfRange {
            slot: self.slot.0,
            index,
            len: self.fields.len(),
        })?;
        Ok(Value::decode(
            f.ty,
            &self.bytes[f.byte as usize..],
            f.bit as usize,
        )?)
    }
    pub fn real(&self, i: usize) -> Result<f32, ApiError> {
        match self.get(i)? {
            Value::Real(v) => Ok(v),
            v => Err(self.mismatch(i, FieldType::Real, v)),
        }
    }
    pub fn bool(&self, i: usize) -> Result<bool, ApiError> {
        match self.get(i)? {
            Value::Bool(v) => Ok(v),
            v => Err(self.mismatch(i, FieldType::Bool, v)),
        }
    }
    pub fn int(&self, i: usize) -> Result<i16, ApiError> {
        match self.get(i)? {
            Value::Int(v) => Ok(v),
            v => Err(self.mismatch(i, FieldType::Int, v)),
        }
    }
    pub fn word(&self, i: usize) -> Result<u16, ApiError> {
        match self.get(i)? {
            Value::Word(v) => Ok(v),
            v => Err(self.mismatch(i, FieldType::Word, v)),
        }
    }
    pub fn dint(&self, i: usize) -> Result<i32, ApiError> {
        match self.get(i)? {
            Value::Dint(v) => Ok(v),
            v => Err(self.mismatch(i, FieldType::Dint, v)),
        }
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    fn mismatch(&self, index: usize, got: FieldType, v: Value) -> ApiError {
        ApiError::TypeMismatch {
            slot: self.slot.0,
            index,
            expected: v.field_type(),
            got,
        }
    }
}

/// A borrowed handle onto one input slot's working buffer, used from
/// [`IoDevice::with_inputs`] to set several fields before one publish.
pub struct SlotWriter<'a> {
    slot: Slot,
    fields: &'a [FieldRef],
    bytes: &'a mut Vec<u8>,
}
impl SlotWriter<'_> {
    pub fn set(&mut self, index: usize, v: Value) -> Result<(), ApiError> {
        let f = *self.fields.get(index).ok_or(ApiError::IndexOutOfRange {
            slot: self.slot.0,
            index,
            len: self.fields.len(),
        })?;
        if f.ty != v.field_type() {
            return Err(ApiError::TypeMismatch {
                slot: self.slot.0,
                index,
                expected: f.ty,
                got: v.field_type(),
            });
        }
        v.encode(&mut self.bytes[f.byte as usize..], f.bit as usize)?;
        Ok(())
    }
    pub fn bool(&mut self, i: usize, v: bool) -> Result<(), ApiError> {
        self.set(i, Value::Bool(v))
    }
    pub fn int(&mut self, i: usize, v: i16) -> Result<(), ApiError> {
        self.set(i, Value::Int(v))
    }
    pub fn word(&mut self, i: usize, v: u16) -> Result<(), ApiError> {
        self.set(i, Value::Word(v))
    }
    pub fn dint(&mut self, i: usize, v: i32) -> Result<(), ApiError> {
        self.set(i, Value::Dint(v))
    }
    pub fn real(&mut self, i: usize, v: f32) -> Result<(), ApiError> {
        self.set(i, Value::Real(v))
    }
}

fn read_mac(iface: &str) -> Result<MacAddr, ApiError> {
    let path = format!("/sys/class/net/{iface}/address");
    let s = std::fs::read_to_string(&path)
        .map_err(|e| ApiError::Io(std::io::Error::new(e.kind(), format!("{path}: {e}"))))?;
    let parts: Vec<&str> = s.trim().split(':').collect();
    if parts.len() != 6 {
        return Err(ApiError::Io(std::io::Error::other(format!(
            "{iface}: not a MAC address (expected 6 colon-separated hex octets, got {}): {s:?}",
            parts.len()
        ))));
    }
    let mut m = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        m[i] = u8::from_str_radix(p, 16)
            .map_err(|_| ApiError::Io(std::io::Error::other(format!("{iface}: bad mac {s:?}"))))?;
    }
    Ok(MacAddr(m))
}
fn io_err(e: crate::eth::TransportError) -> ApiError {
    ApiError::Io(std::io::Error::other(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cm::ArState;
    use crate::config::{DeviceConfig, Slot};
    use crate::data::FieldType::*;
    use crate::eth::{MacAddr, MockTransport};
    use crate::rpc::MockRpcTransport;
    use crate::rt::{DataStatus, Layout, RtFrame, RtRunner};
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

    /// A test-only "wire" shared between the acyclic loop's `eth` and the RT thread's:
    /// `push_rx` enqueues onto one shared queue, but `recv_into` only ever returns a
    /// frame whose FrameID falls in this handle's own `range` — mirroring the
    /// `bpf::acyclic_filter`/`bpf::rt_filter` BPF programs that partition a real NIC's
    /// two sockets by FrameID so each wakes only for its own traffic.
    ///
    /// Without this partitioning the two consumers would race a plain FIFO: the
    /// acyclic loop has no `raw_fd` to poll on with mocks, so `Device::step` drains
    /// `eth.recv` in a tight, unthrottled loop (same as `Device::run` — see
    /// `run_publishing_params`), while the RT thread only drains once per ~1ms tick.
    /// The acyclic side would win essentially every time and silently drop every
    /// RTC1 frame as unparsable DCP, so the RT thread would starve.
    #[derive(Clone)]
    struct SharedMock {
        frames: std::sync::Arc<Mutex<std::collections::VecDeque<Vec<u8>>>>,
        range: (u16, u16),
    }
    impl SharedMock {
        /// One shared queue, two role-filtered handles: `(acyclic, rt)`.
        fn new_pair() -> (SharedMock, SharedMock) {
            let frames = std::sync::Arc::new(Mutex::new(std::collections::VecDeque::new()));
            (
                SharedMock {
                    frames: frames.clone(),
                    range: (0xFC00, 0xFFFF),
                },
                SharedMock {
                    frames,
                    range: (0x8000, 0xBFFF),
                },
            )
        }
        fn push_rx(&self, frame: Vec<u8>) {
            self.frames
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(frame);
        }
    }
    /// The FrameID of a VLAN-tagged or untagged PROFINET frame, or `None` if it isn't
    /// one — same header shapes `RtFrame::parse`/the acyclic BPF filter handle.
    fn frame_id(frame: &[u8]) -> Option<u16> {
        if frame.len() >= 20 && frame[12..14] == [0x81, 0x00] {
            Some(u16::from_be_bytes([frame[18], frame[19]]))
        } else if frame.len() >= 16 && frame[12..14] == [0x88, 0x92] {
            Some(u16::from_be_bytes([frame[14], frame[15]]))
        } else {
            None
        }
    }
    impl crate::eth::EthTransport for SharedMock {
        fn send(&self, _f: &[u8]) -> Result<(), crate::eth::TransportError> {
            Ok(())
        }
        fn recv_into(
            &self,
            buf: &mut [u8],
            _t: Option<Duration>,
        ) -> Result<Option<usize>, crate::eth::TransportError> {
            let mut q = self.frames.lock().unwrap_or_else(|e| e.into_inner());
            let (lo, hi) = self.range;
            let pos = q
                .iter()
                .position(|f| matches!(frame_id(f), Some(id) if id >= lo && id <= hi));
            match pos.and_then(|i| q.remove(i)) {
                Some(frame) => {
                    buf[..frame.len()].copy_from_slice(&frame);
                    Ok(Some(frame.len()))
                }
                None => Ok(None),
            }
        }
    }

    /// Start on mocks with the AR driven to Data by a synthetic Connect + the golden
    /// Write/PrmEnd/AppReady exchange (their bodies do not depend on the model).
    fn started() -> (IoDevice, SharedMock) {
        let cfg = sample();
        let model = cfg.model(DEV);
        let rpc = MockRpcTransport::new();
        let cpu = "172.16.2.100:54766".parse().unwrap();
        let cpu_cm = "172.16.2.100:34964".parse().unwrap();
        rpc.push_rx(synthetic_connect_req(&model), cpu);
        rpc.push_rx(golden("write_req")[RPC_OFF..].to_vec(), cpu);
        rpc.push_rx(golden("prmend_req")[RPC_OFF..].to_vec(), cpu);
        rpc.push_rx(golden("appready_res")[RPC_OFF..].to_vec(), cpu_cm);
        let (eth_acyclic, eth_rt) = SharedMock::new_pair();
        // `rt` must be `Some` — `Device::start_runner` no-ops (never rebuilds the
        // image, never calls the runner factory) when it is `None`, per
        // `device/mod.rs`; `iface` is ignored by `spawn_with_transport` below.
        let rt = Some(RtOptions {
            iface: "mock".into(),
            cpu_pin: None,
            rt_priority: None,
            lock_memory: false,
        });
        let dev = IoDevice::start_with(
            cfg,
            DEV,
            [172, 16, 2, 10],
            rt,
            eth_acyclic.clone(),
            rpc,
            move |rt_cfg| RtRunner::spawn_with_transport(rt_cfg, eth_rt.clone()),
        )
        .unwrap();
        (dev, eth_acyclic)
    }

    /// Waits for `dev.ready()` — AR at `Data` *and* the image actually laid out.
    /// `ar_state() == Data` alone is not enough: see its doc.
    fn wait_until_ready(dev: &IoDevice) {
        let t0 = std::time::Instant::now();
        while !dev.ready() {
            assert!(
                t0.elapsed() < Duration::from_secs(2),
                "AR stuck in {:?}",
                dev.ar_state()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// `rt_snapshot_inputs` is non-blocking (`try_lock`) and can lose a race with the
    /// RT thread's own snapshot of the same buffer; retry rather than assert on the
    /// first attempt.
    fn snapshot_inputs_retry(image: &IoImage, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        for _ in 0..50 {
            if image.rt_snapshot_inputs(&mut buf) {
                return buf;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("rt_snapshot_inputs never got the lock");
    }

    #[test]
    fn reads_before_data_are_no_layout_yet() {
        let cfg = sample();
        let dev = IoDevice::start_with(
            cfg,
            DEV,
            [172, 16, 2, 10],
            None,
            MockTransport::new(),
            MockRpcTransport::new(),
            |c| RtRunner::spawn_with_transport(c, MockTransport::new()),
        )
        .unwrap();
        assert_eq!(dev.ar_state(), ArState::Idle);
        assert_eq!(
            dev.read_real(Slot(3), 0).unwrap_err(),
            ApiError::NoLayoutYet
        );
        assert_eq!(
            dev.write_real(Slot(1), 0, 1.0).unwrap_err(),
            ApiError::NoLayoutYet
        );
        dev.stop().unwrap();
    }

    #[test]
    fn typed_errors_come_from_the_config_table() {
        let (dev, _eth) = started();
        wait_until_ready(&dev);
        assert_eq!(
            dev.read_real(Slot(9), 0).unwrap_err(),
            ApiError::UnknownSlot(9)
        );
        assert_eq!(
            dev.read_real(Slot(3), 16).unwrap_err(),
            ApiError::IndexOutOfRange {
                slot: 3,
                index: 16,
                len: 16
            }
        );
        assert_eq!(
            dev.read_bool(Slot(3), 0).unwrap_err(),
            ApiError::TypeMismatch {
                slot: 3,
                index: 0,
                expected: Real,
                got: Bool
            }
        );
        assert_eq!(
            dev.write_real(Slot(3), 0, 1.0).unwrap_err(),
            ApiError::WrongDirection {
                slot: 3,
                expected: Direction::Output
            }
        );
        assert_eq!(
            dev.read_real(Slot(1), 0).unwrap_err(),
            ApiError::WrongDirection {
                slot: 1,
                expected: Direction::Input
            }
        );
        dev.stop().unwrap();
    }

    /// Builds a valid `0x8001` RTC1 frame for the AR `dev` currently holds: IOPS/IOCS
    /// bytes at `0x80` for every object in the negotiated output layout, `sets` written
    /// at their layout-derived offsets, dst = our device MAC, src = the CPU's MAC
    /// (`params.initiator_mac`). Every offset comes from `Layout::from_ar` against the
    /// live AR params — nothing here is hard-coded to a particular model.
    fn cpu_frame_for(dev: &IoDevice, cfg: &DeviceConfig, sets: &[(Slot, usize, Value)]) -> Vec<u8> {
        let params = dev.ar_params().expect("AR established");
        let model = cfg.model(DEV);
        let layout = Layout::from_ar(&params, &model).unwrap();
        let mut csdu = vec![0u8; layout.output_cr.data_length];
        for obj in &layout.output_cr.objects {
            csdu[obj.iops_off] = 0x80;
        }
        for cs in &layout.output_cr.iocs {
            csdu[cs.iocs_off] = 0x80;
        }
        for &(slot, index, value) in sets {
            let obj = layout
                .output_cr
                .objects
                .iter()
                .find(|o| o.slot == slot.0)
                .unwrap_or_else(|| panic!("slot {} has no output object", slot.0));
            let field = cfg
                .field(slot, Direction::Output, index)
                .unwrap_or_else(|| panic!("slot {} index {index} not declared", slot.0));
            value
                .encode(
                    &mut csdu[obj.data_off + field.byte as usize..],
                    field.bit as usize,
                )
                .unwrap();
        }
        let mut buf = vec![0u8; crate::rt::frame_len(csdu.len())];
        let n = RtFrame {
            frame_id: 0x8001,
            csdu: &csdu,
            cycle_counter: 1024,
            data_status: DataStatus(0x35),
            transfer_status: 0,
        }
        .write(&mut buf, DEV, params.initiator_mac)
        .unwrap();
        buf.truncate(n);
        buf
    }

    #[test]
    fn writes_publish_the_whole_submodule_and_reads_decode_cpu_frames() {
        let (dev, eth) = started();
        wait_until_ready(&dev);
        let cfg = sample();

        // Group write: two fields of slot 1, one publish.
        dev.with_inputs(Slot(1), |w| {
            w.real(0, 1.0)?;
            w.real(15, -2.5)
        })
        .unwrap();
        dev.write_bool(Slot(2), 31, true).unwrap();
        let image = dev.image();
        let params = dev.ar_params().unwrap();
        let layout = Layout::from_ar(&params, &cfg.model(DEV)).unwrap();
        let buf = snapshot_inputs_retry(&image, layout.input_cr.data_length);
        let s1_off = layout
            .input_cr
            .objects
            .iter()
            .find(|o| o.slot == 1)
            .unwrap()
            .data_off;
        let s1 = &buf[s1_off..s1_off + 64];
        assert_eq!(&s1[..4], &[0x3F, 0x80, 0, 0]);
        assert_eq!(&s1[60..64], &[0xC0, 0x20, 0, 0]);

        // Inject a CPU frame carrying REAL 1.0 at slot 3 index 0 and bit 7 (index 31)
        // of slot 4. A real controller resends every cycle, so a background feeder
        // keeps pushing it (~1ms, the CR's own period) instead of a single push +
        // fixed sleep: the RT thread's consumer watchdog (a few ms here) would
        // otherwise expire between one push and the later `Fresh` assertions.
        let frame = cpu_frame_for(
            &dev,
            &cfg,
            &[
                (Slot(3), 0, Value::Real(1.0)),
                (Slot(4), 31, Value::Bool(true)),
            ],
        );
        let stop_feed = std::sync::Arc::new(AtomicBool::new(false));
        let feeder = {
            let stop_feed = stop_feed.clone();
            let eth = eth.clone();
            std::thread::spawn(move || {
                while !stop_feed.load(Ordering::Relaxed) {
                    eth.push_rx(frame.clone());
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        };
        let t0 = std::time::Instant::now();
        loop {
            if dev.read_real(Slot(3), 0) == Ok(1.0) && dev.freshness() == Freshness::Fresh {
                break;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(2),
                "CPU frame never landed"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let snap = dev.outputs(Slot(3)).unwrap();
        assert_eq!(snap.real(0).unwrap(), 1.0);
        assert_eq!(dev.read_real(Slot(3), 0).unwrap(), 1.0);
        assert_eq!(dev.freshness(), Freshness::Fresh);
        // Bit 7 of slot 4 byte 3 (field index 31) was set; its neighbor (index 30,
        // bit 6) must still read false — proves the bit write didn't spill over.
        assert!(dev.read_bool(Slot(4), 31).unwrap());
        assert!(!dev.read_bool(Slot(4), 30).unwrap());
        stop_feed.store(true, Ordering::Relaxed);
        feeder.join().unwrap();
        dev.stop().unwrap();
    }

    #[test]
    fn with_inputs_rolls_back_the_working_copy_on_error() {
        let (dev, _eth) = started();
        wait_until_ready(&dev);
        let cfg = sample();
        let params = dev.ar_params().unwrap();
        let layout = Layout::from_ar(&params, &cfg.model(DEV)).unwrap();
        let s1_off = layout
            .input_cr
            .objects
            .iter()
            .find(|o| o.slot == 1)
            .unwrap()
            .data_off;

        // Field 0 succeeds, field 99 (out of slot 1's 16 REALs) fails: the whole
        // closure must be rejected without publishing field 0's write.
        let err = dev
            .with_inputs(Slot(1), |w| {
                w.real(0, 1.0)?;
                w.real(99, 0.0)
            })
            .unwrap_err();
        assert_eq!(
            err,
            ApiError::IndexOutOfRange {
                slot: 1,
                index: 99,
                len: 16
            }
        );

        // A later, independent write to a different field of the same slot must see
        // the working copy exactly as it was before the failed attempt: field 0
        // still 0.0 (not the 1.0 the failed closure set on its scratch copy), field 1
        // now 2.0.
        dev.write_real(Slot(1), 1, 2.0).unwrap();
        let image = dev.image();
        let buf = snapshot_inputs_retry(&image, layout.input_cr.data_length);
        let s1 = &buf[s1_off..s1_off + 64];
        assert_eq!(&s1[0..4], &[0, 0, 0, 0]);
        assert_eq!(&s1[4..8], &[0x40, 0x00, 0x00, 0x00]);
        dev.stop().unwrap();
    }

    #[test]
    fn drop_without_stop_does_not_panic() {
        let (dev, _eth) = started();
        wait_until_ready(&dev);
        drop(dev);
    }
}

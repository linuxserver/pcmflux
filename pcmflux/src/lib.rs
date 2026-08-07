/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! pcmflux: PulseAudio/PipeWire audio capture with Opus encoding, plus mic-uplink
//! playback, exposed as a pure-Rust PyO3 extension (the audio sibling of pixelflux).
//!
//! The capture thread pulls S16LE PCM fragments from a PulseAudio record stream,
//! reassembles them into fixed-size Opus frames, encodes them (mono/stereo via the
//! `opus` crate, 5.1/7.1 surround via the multistream API), optionally wraps them in
//! RFC 2198 RED framing — redundant copies of recent frames, so a client on a lossy
//! transport rebuilds a dropped packet from the next one it receives instead of stalling
//! for a retransmit — and hands each frame to a delivery thread that runs the Python
//! callback off the audio path, so a slow or GIL-blocked callback can never stall the
//! PulseAudio pump. The playback path mirrors this in reverse: it decodes the Opus mic
//! uplink and writes PCM into a virtual sink.
//!
//! Concurrency design (the invariants below are load-bearing):
//!   - A lifecycle mutex serializes joining/reassigning the capture thread.
//!   - A single stop_state atomic is the one source of truth (0 = running, -1 =
//!     external stop, a positive value = self-stop recorded under the issuing thread's
//!     tid — the delivery thread's for a callback-issued stop). The
//!     external -1 is stored INSIDE that lock immediately before join, so a stop can
//!     never be lost between observing a live thread and asking it to stop. A
//!     re-entrant self-start undoes only its own self-stop via one compare-exchange,
//!     so a racing external stop is never clobbered.
//!   - The PulseAudio mainloop is pumped with a bounded ~20ms timeout, so a stop is
//!     observed within ~20ms even if the audio source delivers no data (is wedged).
//!   - The GIL is released around join, because joining the capture thread transitively
//!     joins the delivery thread, whose in-flight Python callback needs the GIL; holding
//!     it while joining would deadlock.
//!   - A callback may itself call stop/start; the callback runs on the delivery thread,
//!     so that re-entrant case is detected via the delivery thread's OS tid (the capture
//!     thread's tid is checked too) and short-circuits without joining — a join from
//!     inside the callback would cycle (stopper joins capture, capture joins delivery,
//!     delivery is the stopper).

use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyString};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::thread::{JoinHandle, ThreadId};
use std::time::{Duration, Instant};

use libpulse_binding as pulse;
use pulse::callbacks::ListResult;
use pulse::context::{Context, FlagSet as CtxFlags};
use pulse::def::BufferAttr;
use pulse::sample::{Format, Spec};
use pulse::mainloop::standard::Mainloop;
use pulse::stream::{FlagSet as StreamFlags, PeekResult, Stream};
use pulse::time::MicroSeconds;

use opus::{Application, Channels};

/// Non-panicking `println!` replacement that swallows write errors (e.g. EPIPE) instead
/// of panicking, so a broken output pipe can't unwind the capture thread or a callback.
macro_rules! plog {
    ($($arg:tt)*) => {{
        use std::io::Write;
        let _ = writeln!(std::io::stdout().lock(), $($arg)*);
    }};
}
/// Non-panicking `eprintln!` replacement — the stderr sibling of `plog!`.
macro_rules! elog {
    ($($arg:tt)*) => {{
        use std::io::Write;
        let _ = writeln!(std::io::stderr().lock(), $($arg)*);
    }};
}

/// Returns the calling thread's OS tid (`gettid` syscall) so a stop/start issued from
/// inside the Python callback can detect it is on the capture thread and avoid self-joining.
#[inline]
fn gettid() -> i64 {
    unsafe { libc::syscall(libc::SYS_gettid) as i64 }
}

/// `start_state` handshake: startup in progress.
const ST_STARTING: u8 = 1;
/// `start_state` handshake: the hot loop is running.
const ST_RUNNING: u8 = 2;
/// `start_state` handshake: startup failed.
const ST_FAILED: u8 = 3;

/// `stop_state` sentinel: no stop pending (running).
const STOP_NONE: i64 = 0;
/// `stop_state` sentinel: authoritative external stop (-1). Any positive value
/// instead is the OS tid of a capture thread that self-stopped from its callback.
const STOP_EXTERNAL: i64 = -1;

/// PulseAudio mainloop pump timeout (~20 ms) — upper bound on how long a pending
/// stop can go unobserved even when the audio source delivers nothing.
const PUMP_TIMEOUT_US: u64 = 20 * 1000;
/// Max Opus output packet size in bytes per stream; sizes the emit buffer pool.
const MAX_OPUS_PACKET: usize = 4000;

/// Max RED timestamp offset — 14-bit ceiling in 48 kHz samples from the primary.
const RED_MAX_OFFSET: u64 = 16383;
/// Max RED block length — 10-bit ceiling in bytes.
const RED_MAX_LEN: usize = 1023;
/// RED block payload type.
const RED_BLOCK_PT: u8 = 0;
/// Max redundant copies per frame and RED history depth.
const RED_MAX_DISTANCE: i32 = 4;

/// Test-only reference implementation of the WS audio frame body — builds the
/// full RFC 2198 RED framing + primary Opus packet for byte-for-byte assertion
/// against the pooled in-place `write_ws_prefix_into` on the hot path.
///
/// RED carries redundant copies of recent frames alongside each primary, so a client on a
/// lossy transport can rebuild a dropped packet from the next one it receives with no
/// retransmit — that packet-loss concealment is the whole reason the layout below exists.
///
/// `history` is oldest-first (front = oldest, largest timestamp offset back from
/// `primary_pts`). The layout is chosen by `emit_header` and `red_distance`:
///
/// 1. **Header omitted** (`!emit_header`): the raw primary Opus packet, with no framing
///    bytes — the header is what carries the redundant-block count, so without it there
///    is no `[0x01,n]` prefix.
/// 2. **`red_distance == 0`, or no usable redundancy**: the 2-byte `[0x01, 0x00]` framing
///    then the primary. On this wire `n_red == 0` must mean **exactly** those two bytes,
///    so a first frame after a (re)start (empty history) collapses here too; emitting a
///    lone primary-only RED header instead would be mis-stripped by the client's
///    `n_red == 0` path and corrupt the frame.
/// 3. **`red_distance > 0` with usable history**: the full RFC 2198 RED framing —
///    - `0x01` audio-chunk tag, then `n_red` (count of redundant blocks that actually fit).
///    - The primary timestamp (low 32 bits, big-endian), letting the client order and dedup
///      recovered frames against what it has already played.
///    - One 4-byte header per redundant block, oldest-first: `0x80 | PT` (F bit set,
///      i.e. another block follows), then `(offset14 << 10) | len10` big-endian.
///    - The 1-byte primary block header (`PT`, F bit clear).
///    - The block payloads in that same order: redundant oldest-first, then the primary.
///
/// A redundant block whose 48 kHz sample offset overflows `RED_MAX_OFFSET` (14 bits) or
/// whose byte length overflows `RED_MAX_LEN` (10 bits) is skipped, so `n_red` counts only
/// what fit.
#[cfg(test)]
fn build_ws_body(
    primary: &[u8],
    primary_pts: u64,
    history: &VecDeque<(Vec<u8>, u64)>,
    red_distance: usize,
    emit_header: bool,
) -> Vec<u8> {
    if !emit_header {
        return primary.to_vec();
    }
    if red_distance == 0 {
        let mut out = Vec::with_capacity(2 + primary.len());
        out.push(0x01);
        out.push(0x00);
        out.extend_from_slice(primary);
        return out;
    }
    let start = history.len().saturating_sub(red_distance);
    let mut blocks: Vec<(&[u8], u32)> = Vec::with_capacity(red_distance);
    for (data, pts) in history.iter().skip(start) {
        let offset = primary_pts.saturating_sub(*pts);
        if offset > RED_MAX_OFFSET || data.len() > RED_MAX_LEN {
            continue;
        }
        blocks.push((data.as_slice(), offset as u32));
    }
    if blocks.is_empty() {
        let mut out = Vec::with_capacity(2 + primary.len());
        out.push(0x01);
        out.push(0x00);
        out.extend_from_slice(primary);
        return out;
    }
    let redundant_bytes: usize = blocks.iter().map(|(d, _)| d.len()).sum();
    let mut out = Vec::with_capacity(6 + 4 * blocks.len() + 1 + redundant_bytes + primary.len());
    out.push(0x01);
    out.push(blocks.len() as u8);
    out.extend_from_slice(&(primary_pts as u32).to_be_bytes());
    for (data, offset) in &blocks {
        out.push(0x80 | (RED_BLOCK_PT & 0x7F));
        let word = ((offset & 0x3FFF) << 10) | (data.len() as u32 & 0x3FF);
        out.push((word >> 16) as u8);
        out.push((word >> 8) as u8);
        out.push(word as u8);
    }
    out.push(RED_BLOCK_PT & 0x7F);
    for (data, _) in &blocks {
        out.extend_from_slice(data);
    }
    out.extend_from_slice(primary);
    out
}

/// Worst-case byte length of the WS frame prefix (tag + n_red + pts + headers + payloads).
const RED_PREFIX_MAX: usize =
    6 + 4 * RED_MAX_DISTANCE as usize + 1 + RED_MAX_DISTANCE as usize * RED_MAX_LEN;

/// Emit the RFC 2198 RED framing prefix into `buf` in-place and return its length,
/// so the encoder can serialize the Opus packet directly after it with no scratch
/// buffer and no per-frame allocation. The runtime counterpart of `build_ws_body`.
///
/// 1. **Header omitted**: returns `0`; the caller emits the raw primary with no prefix.
/// 2. **`red_distance > 0` with usable history**: writes the full RED header — tag,
///    `n_red`, the primary timestamp (low 32 bits, big-endian), one 4-byte header per
///    redundant block (F bit set, then `(offset14 << 10) | len10`), the 1-byte primary
///    header (F bit clear), and the redundant block payloads oldest-first. Usable blocks
///    are selected by history index into a fixed `RED_MAX_DISTANCE` array — the distance is
///    clamped at ingest, so this allocates nothing.
/// 3. **`red_distance == 0`, or no usable redundancy**: the 2-byte `[0x01, 0x00]` framing.
///
/// `buf` must hold at least `RED_PREFIX_MAX` bytes when `emit_header && red_distance > 0`,
/// and at least 2 bytes otherwise.
fn write_ws_prefix_into(
    buf: &mut [u8],
    primary_pts: u64,
    history: &VecDeque<(Vec<u8>, u64)>,
    red_distance: usize,
    emit_header: bool,
) -> usize {
    if !emit_header {
        return 0;
    }
    if red_distance > 0 {
        debug_assert!(red_distance <= RED_MAX_DISTANCE as usize);
        let start = history.len().saturating_sub(red_distance);
        let mut idx = [0usize; RED_MAX_DISTANCE as usize];
        let mut n = 0usize;
        for (i, (data, pts)) in history.iter().enumerate().skip(start) {
            let offset = primary_pts.saturating_sub(*pts);
            if offset <= RED_MAX_OFFSET && data.len() <= RED_MAX_LEN {
                idx[n] = i;
                n += 1;
            }
        }
        if n > 0 {
            buf[0] = 0x01;
            buf[1] = n as u8;
            buf[2..6].copy_from_slice(&(primary_pts as u32).to_be_bytes());
            let mut i = 6;
            for &k in &idx[..n] {
                let (data, pts) = &history[k];
                let offset = primary_pts.saturating_sub(*pts) as u32;
                buf[i] = 0x80 | (RED_BLOCK_PT & 0x7F);
                let word = ((offset & 0x3FFF) << 10) | (data.len() as u32 & 0x3FF);
                buf[i + 1] = (word >> 16) as u8;
                buf[i + 2] = (word >> 8) as u8;
                buf[i + 3] = word as u8;
                i += 4;
            }
            buf[i] = RED_BLOCK_PT & 0x7F;
            i += 1;
            for &k in &idx[..n] {
                let data = &history[k].0;
                buf[i..i + data.len()].copy_from_slice(data);
                i += data.len();
            }
            return i;
        }
    }
    buf[0] = 0x01;
    buf[1] = 0x00;
    2
}

/// Capture/encode settings, snapshotted from `AudioCaptureSettings` at start.
#[derive(Clone)]
struct Settings {
    device_name: Option<String>,
    sample_rate: u32,
    channels: i32,
    opus_bitrate: i32,
    frame_duration_ms: f64,
    use_vbr: bool,
    use_silence_gate: bool,
    debug_logging: bool,
    latency_ms: i32,
    omit_audio_header: bool,
    red_distance: i32,
}

/// True if `ms` is a valid Opus frame duration (2.5, 5, 10, 20, 40, or 60 ms).
///
/// The comparison is done in tenths of a millisecond (`round(ms * 10)`) so the fractional
/// 2.5 ms case is accepted exactly, without float-equality pitfalls.
fn valid_opus_duration(ms: f64) -> bool {
    matches!((ms * 10.0).round() as i64, 25 | 50 | 100 | 200 | 400 | 600)
}

/// Normalize a Python `device_name` (`str | bytes | None`) into `Option<String>`,
/// mapping both `None` and the empty string to `None` (meaning the system default).
/// Shared by the capture and playback settings extractors.
fn parse_device_name(dev_obj: &Bound<'_, PyAny>) -> PyResult<Option<String>> {
    if dev_obj.is_none() {
        Ok(None)
    } else if let Ok(st) = dev_obj.cast::<PyString>() {
        let v = st.to_str()?.to_string();
        Ok(if v.is_empty() { None } else { Some(v) })
    } else if let Ok(b) = dev_obj.cast::<PyBytes>() {
        let v = String::from_utf8_lossy(b.as_bytes()).into_owned();
        Ok(if v.is_empty() { None } else { Some(v) })
    } else {
        let v: String = dev_obj.extract()?;
        Ok(if v.is_empty() { None } else { Some(v) })
    }
}

/// Read a Python `AudioCaptureSettings` into a Rust `Settings` by attribute name.
///
/// `red_distance` is clamped into `[0, RED_MAX_DISTANCE]` — it selects how many redundant
/// Opus copies each frame carries, and cannot exceed the RFC 2198 history depth.
fn extract_settings(s: &Bound<'_, PyAny>) -> PyResult<Settings> {
    let device_name = parse_device_name(&s.getattr("device_name")?)?;
    Ok(Settings {
        device_name,
        sample_rate: s.getattr("sample_rate")?.extract()?,
        channels: s.getattr("channels")?.extract()?,
        opus_bitrate: s.getattr("opus_bitrate")?.extract()?,
        frame_duration_ms: s.getattr("frame_duration_ms")?.extract()?,
        use_vbr: s.getattr("use_vbr")?.extract()?,
        use_silence_gate: s.getattr("use_silence_gate")?.extract()?,
        debug_logging: s.getattr("debug_logging")?.extract()?,
        latency_ms: s.getattr("latency_ms")?.extract()?,
        omit_audio_header: s.getattr("omit_audio_header")?.extract()?,
        red_distance: s.getattr("red_distance")?.extract::<i32>()?.clamp(0, RED_MAX_DISTANCE),
    })
}

/// Playback settings, snapshotted from the Python `AudioPlaybackSettings` at
/// start so the playback thread owns an immutable copy for the run's lifetime.
#[derive(Clone)]
struct PbSettings {
    device_name: Option<String>,
    sample_rate: u32,
    channels: i32,
    latency_ms: i32,
    max_buffer_bytes: usize,
    debug_logging: bool,
}

/// Read a Python `AudioPlaybackSettings` into a Rust `PbSettings` by attribute name.
fn extract_pb_settings(s: &Bound<'_, PyAny>) -> PyResult<PbSettings> {
    Ok(PbSettings {
        device_name: parse_device_name(&s.getattr("device_name")?)?,
        sample_rate: s.getattr("sample_rate")?.extract()?,
        channels: s.getattr("channels")?.extract()?,
        latency_ms: s.getattr("latency_ms")?.extract()?,
        max_buffer_bytes: s.getattr("max_buffer_bytes")?.extract()?,
        debug_logging: s.getattr("debug_logging")?.extract()?,
    })
}

/// Python-facing capture/encode configuration read by `start_capture`.
///
/// Declared `#[pyclass(dict)]` so callers may stash extra attributes on instances; the
/// fields below are the ones read by attribute name in `extract_settings`. `device_name`
/// accepts `str | bytes | None`.
#[pyclass(dict)]
struct AudioCaptureSettings {
    #[pyo3(get, set)]
    device_name: Py<PyAny>,
    #[pyo3(get, set)]
    sample_rate: u32,
    #[pyo3(get, set)]
    channels: i32,
    #[pyo3(get, set)]
    opus_bitrate: i32,
    #[pyo3(get, set)]
    frame_duration_ms: f64,
    #[pyo3(get, set)]
    use_vbr: bool,
    #[pyo3(get, set)]
    use_silence_gate: bool,
    #[pyo3(get, set)]
    debug_logging: bool,
    #[pyo3(get, set)]
    latency_ms: i32,
    #[pyo3(get, set)]
    omit_audio_header: bool,
    #[pyo3(get, set)]
    red_distance: i32,
}

#[pymethods]
impl AudioCaptureSettings {
    #[new]
    fn new(py: Python<'_>) -> Self {
        AudioCaptureSettings {
            device_name: py.None(),
            sample_rate: 48000,
            channels: 2,
            opus_bitrate: 128000,
            frame_duration_ms: 20.0,
            use_vbr: true,
            use_silence_gate: true,
            debug_logging: false,
            latency_ms: 0,
            omit_audio_header: false,
            red_distance: 0,
        }
    }
}

/// Python-facing mic-playback configuration read by `AudioPlayback.start`.
///
/// Defaults match the client mic wire (S16LE / mono / 24 kHz). `max_buffer_bytes` is the
/// single byte bound on the drop-oldest playback queue (~2 s at 24 kHz mono s16), and
/// `device_name` accepts `str | bytes | None`.
#[pyclass]
struct AudioPlaybackSettings {
    #[pyo3(get, set)]
    device_name: Py<PyAny>,
    #[pyo3(get, set)]
    sample_rate: u32,
    #[pyo3(get, set)]
    channels: i32,
    #[pyo3(get, set)]
    latency_ms: i32,
    #[pyo3(get, set)]
    max_buffer_bytes: usize,
    #[pyo3(get, set)]
    debug_logging: bool,
}

#[pymethods]
impl AudioPlaybackSettings {
    #[new]
    fn new(py: Python<'_>) -> Self {
        AudioPlaybackSettings {
            device_name: PyString::new(py, "input").into_any().unbind(),
            sample_rate: 24000,
            channels: 1,
            latency_ms: 40,
            max_buffer_bytes: 96000,
            debug_logging: false,
        }
    }
}

/// Zero-copy buffer-protocol result type handed to the Python callback.
///
/// Owns its `Vec<u8>` and exposes it read-only through the buffer protocol, so Python can
/// read the encoded frame without a copy. When the last Python reference is released and
/// the frame is dropped, a pooled buffer is recycled back to the capture thread (see the
/// `Drop` impl), keeping the steady-state emit path allocation-free.
#[pyclass]
struct AudioFrame {
    data: Vec<u8>,
    pts: u64,
    /// Set when the buffer came from a capture's `BufferPool`; recycled to it on drop.
    pool: Option<Arc<BufferPool>>,
}

impl Drop for AudioFrame {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            pool.put(std::mem::take(&mut self.data));
        }
    }
}

#[pymethods]
impl AudioFrame {
    fn __len__(&self) -> usize {
        self.data.len()
    }

    #[getter]
    fn pts(&self) -> u64 {
        self.pts
    }

    /// Expose the owned bytes to Python's buffer protocol without a copy.
    ///
    /// `PyBuffer_FillInfo` INCREFs `slf` into `view->obj`, pinning the `Vec` alive until
    /// every `memoryview` / slice over it is released. The view is readonly, so the
    /// consumer cannot mutate the encoded frame.
    unsafe fn __getbuffer__(
        slf: PyRefMut<'_, Self>,
        view: *mut pyo3::ffi::Py_buffer,
        flags: std::os::raw::c_int,
    ) -> PyResult<()> {
        let r = pyo3::ffi::PyBuffer_FillInfo(
            view,
            slf.as_ptr(),
            slf.data.as_ptr() as *mut std::os::raw::c_void,
            slf.data.len() as pyo3::ffi::Py_ssize_t,
            1,
            flags,
        );
        if r != 0 {
            return Err(PyErr::fetch(slf.py()));
        }
        Ok(())
    }

    unsafe fn __releasebuffer__(&self, _view: *mut pyo3::ffi::Py_buffer) {}
}

/// Lock-free shared state for one capture (or playback) run: the lifecycle
/// state machine plus the per-frame settings mirrors the worker reads on the hot path.
///
/// The lifecycle is driven by two atomics — `stop_state` (the single source of truth for
/// "should this run stop") and `start_state` (the STARTING → RUNNING/FAILED startup
/// handshake) — plus `capture_tid` and `deliver_tid`, the worker and delivery threads'
/// OS tids used to detect a re-entrant stop/start issued from inside the Python callback
/// (which runs on the delivery thread). The remaining atomics mirror settings the worker
/// consults each frame without re-snapshotting `Settings`, so `update_audio_bitrate` can
/// retune the encoder mid-run without locking; the silence and header flags are published
/// once at start and only read per frame.
struct Inner {
    /// Single lifecycle source of truth: `STOP_NONE` (running), `STOP_EXTERNAL`, or a
    /// positive tid meaning the run self-stopped from inside its own callback (recorded
    /// under the issuing thread's tid — the delivery thread's). A re-entrant start clears
    /// only its own self-stop via compare-exchange, so it can never clobber an external
    /// stop that raced in mid-join (which would strand it).
    stop_state: AtomicI64,
    started_ok: AtomicBool,
    start_state: AtomicU8,
    /// OS tid of the running capture thread; `0` when no worker is live.
    capture_tid: AtomicI64,
    /// OS tid of the running delivery thread (the one that invokes the Python callback);
    /// `0` when none is live. Checked by the re-entrancy guards alongside `capture_tid`,
    /// because a stop/start issued from inside the callback executes on THIS thread — a
    /// join from it would cycle (stopper joins capture, capture joins delivery).
    deliver_tid: AtomicI64,
    /// Lock-free per-frame settings mirrors, re-read by the worker each frame.
    /// `opus_bitrate` is republished by `update_audio_bitrate`; the rest are published
    /// once, by the run that starts.
    opus_bitrate: AtomicI32,
    use_silence_gate: AtomicBool,
    debug_logging: AtomicBool,
    emit_audio_header: AtomicBool,
}

impl Inner {
    fn new() -> Self {
        Inner {
            stop_state: AtomicI64::new(STOP_NONE),
            started_ok: AtomicBool::new(false),
            start_state: AtomicU8::new(0),
            capture_tid: AtomicI64::new(0),
            deliver_tid: AtomicI64::new(0),
            opus_bitrate: AtomicI32::new(128000),
            use_silence_gate: AtomicBool::new(true),
            debug_logging: AtomicBool::new(false),
            emit_audio_header: AtomicBool::new(true),
        }
    }

    /// Request an authoritative external stop, stored unconditionally.
    ///
    /// The single source of truth for stopping a run. External stops win every race: they
    /// are published (inside the lifecycle lock, immediately before join) with a plain
    /// store, so a concurrent self-stop/self-start — which only ever compare-exchanges
    /// state it owns — can never clobber one and strand the join forever.
    fn request_external_stop(&self) {
        self.stop_state.store(STOP_EXTERNAL, Ordering::Release);
    }

    /// Record a re-entrant self-stop from inside the run's own callback.
    ///
    /// Only transitions `STOP_NONE -> me` via compare-exchange, so it never overwrites a
    /// pending external stop (which must win the join).
    fn request_self_stop(&self, me: i64) {
        let _ = self
            .stop_state
            .compare_exchange(STOP_NONE, me, Ordering::AcqRel, Ordering::Acquire);
    }

    /// Undo a re-entrant self-stop (self-start from inside the callback).
    ///
    /// Compare-exchanges `me -> STOP_NONE`, clearing the stop only if this same thread
    /// still owns it. If an external stop landed in between, the CAS fails and that stop
    /// stands.
    fn undo_self_stop(&self, me: i64) {
        let _ = self
            .stop_state
            .compare_exchange(me, STOP_NONE, Ordering::AcqRel, Ordering::Acquire);
    }

    /// Clear the stop state back to running. Only ever called under the lifecycle
    /// lock (after join, before spawn), where no external stop can be in flight — the
    /// lost-stop invariant that lets this be an unconditional store.
    fn clear_stop(&self) {
        self.stop_state.store(STOP_NONE, Ordering::Release);
    }

    /// True once any stop (external or self) is pending; the hot loops poll this.
    fn stop_pending(&self) -> bool {
        self.stop_state.load(Ordering::Acquire) != STOP_NONE
    }

    /// True while a worker is (or is still becoming) live: the startup handshake
    /// is in flight, or the hot loop is running with no stop pending.
    ///
    /// Goes false the moment the worker fails, is stopped, or dies mid-run — the hot loop
    /// clears `started_ok` before breaking on error, even with no stop pending and
    /// `start_state` still `RUNNING`. Producers (e.g. `AudioPlayback::write`) gate on this
    /// so they surface a dead stream instead of feeding state nothing services.
    fn worker_alive(&self) -> bool {
        if self.start_state.load(Ordering::Acquire) == ST_STARTING {
            return true;
        }
        self.started_ok.load(Ordering::Acquire) && !self.stop_pending()
    }

    /// True when the calling thread is one of this run's own threads — the capture
    /// worker or the delivery thread that runs the Python callback — i.e. the call is a
    /// re-entrant stop/start/drop from inside the callback. Such a caller must never
    /// join: teardown has the capture thread join the delivery thread, so a join from
    /// either one closes a cycle and deadlocks.
    fn is_own_thread(&self, me: i64) -> bool {
        self.capture_tid.load(Ordering::Acquire) == me
            || self.deliver_tid.load(Ordering::Acquire) == me
    }
}

/// Per-`AudioCapture` shared handle: the lock-free `Inner` state plus the
/// lifecycle-locked join handle for the capture thread.
struct Shared {
    inner: Arc<Inner>,
    /// Lifecycle lock: serializes take/join/reassign of the capture thread's handle.
    thread: Mutex<Option<JoinHandle<()>>>,
}

/// Process-wide registry of live captures, swept at interpreter exit. Holds `Weak`
/// references so it keeps nothing alive on its own.
static REGISTRY: OnceLock<Mutex<Vec<Weak<Shared>>>> = OnceLock::new();
/// Lazily initialize and return the capture registry.
fn registry() -> &'static Mutex<Vec<Weak<Shared>>> {
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// Locked takeover + spawn of a worker thread, shared by the capture and playback
/// starts. Returns the new thread's id, or `None` if the spawn failed.
///
/// Under the lifecycle lock, in order:
///
/// 1. **Stop and join any prior worker**: sets the external stop INSIDE the lock,
///    immediately before `join()`, then clears `capture_tid` — the set-before-join
///    ordering that makes the lost-stop invariant hold.
/// 2. **Reset the lifecycle atomics**: clears `stop_state` back to `STOP_NONE` (done ONLY
///    here, under the lock, after the join and before the spawn, where no external stop
///    can be in flight), and arms the startup handshake at `ST_STARTING`.
/// 3. **Spawn `body` on a named thread**: the thread applies a best-effort `nice` boost
///    (audio must not stutter when the captured workload saturates the CPU; EPERM without
///    `CAP_SYS_NICE` is silently a no-op), publishes its OS tid into `capture_tid` for the
///    re-entrancy guard, runs `body`, then clears the tid on exit.
///
/// The returned `ThreadId` is the identity a failed start later hands to
/// `join_failed_start`, so a losing start tears down only the thread it spawned.
fn spawn_worker(
    slot: &Mutex<Option<JoinHandle<()>>>,
    inner: &Arc<Inner>,
    name: &str,
    body: impl FnOnce() + Send + 'static,
) -> Option<ThreadId> {
    let mut guard = slot.lock().unwrap();
    if let Some(handle) = guard.take() {
        inner.request_external_stop();
        let _ = handle.join();
        inner.capture_tid.store(0, Ordering::Release);
    }
    inner.clear_stop();
    inner.started_ok.store(false, Ordering::Release);
    inner.start_state.store(ST_STARTING, Ordering::Release);
    let t_inner = inner.clone();
    match std::thread::Builder::new().name(name.into()).spawn(move || {
        unsafe {
            let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
            let _ = libc::setpriority(libc::PRIO_PROCESS, tid, -15);
        }
        t_inner.capture_tid.store(gettid(), Ordering::Release);
        // A worker panic must flip the liveness contract (started_ok/start_state):
        // an unguarded unwind would leave is_capturing reporting true forever with
        // no frames flowing and no error anywhere.
        let p_inner = t_inner.clone();
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).is_err() {
            elog!("[pcmflux] ERROR: worker thread panicked; marking the capture dead.");
            p_inner.started_ok.store(false, Ordering::Release);
            p_inner.start_state.store(ST_FAILED, Ordering::Release);
        }
        t_inner.capture_tid.store(0, Ordering::Release);
    }) {
        Ok(h) => {
            let id = h.thread().id();
            *guard = Some(h);
            Some(id)
        }
        Err(_) => None,
    }
}

/// Failed-start teardown: stop and join the thread `spawned` by THIS start attempt,
/// but only if it still owns the slot.
///
/// A concurrent start may have already joined this thread and published a live replacement,
/// which must not be torn down. The guard is an identity check: `ThreadId`s are never reused
/// within a process, so it cannot false-match. When it does own the slot, the external stop
/// is set INSIDE the lock before `join()`, matching every other join site (the
/// set-before-join / lost-stop invariant).
fn join_failed_start(slot: &Mutex<Option<JoinHandle<()>>>, inner: &Inner, spawned: ThreadId) {
    let mut guard = slot.lock().unwrap();
    if guard.as_ref().map(|h| h.thread().id()) != Some(spawned) {
        return;
    }
    if let Some(handle) = guard.take() {
        inner.request_external_stop();
        let _ = handle.join();
        inner.capture_tid.store(0, Ordering::Release);
    }
}

/// Bounded, drop-oldest byte queue for the mic-PCM handoff into the virtual
/// "input" sink — the whole of the playback path's buffering in a single bound.
///
/// `push` runs on the Python side with the GIL released; `drain_upto` runs on the
/// playback thread. Overflow discards the OLDEST bytes, keeping the newest window (mic
/// audio is drift-tolerant and stale samples are worthless).
struct PlayQueue {
    buf: Mutex<VecDeque<u8>>,
    /// Bounds (re)published by `start()`; atomics so a restart can reconfigure the
    /// `Arc`-shared queue in place without swapping it. `frame_bytes >= 1` (set at `new()`).
    max_bytes: AtomicUsize,
    frame_bytes: AtomicUsize,
}

impl PlayQueue {
    fn new() -> Self {
        PlayQueue {
            buf: Mutex::new(VecDeque::new()),
            max_bytes: AtomicUsize::new(96000),
            frame_bytes: AtomicUsize::new(2),
        }
    }

    /// Apply this run's byte bound and frame alignment, and drop any stale audio
    /// left from a prior run. `frame_bytes` is floored to 1; the bound is floored
    /// to a whole-frame multiple (min one frame) so overflow drops can never split
    /// a sample frame.
    fn configure(&self, max_bytes: usize, frame_bytes: usize) {
        let fb = frame_bytes.max(1);
        self.frame_bytes.store(fb, Ordering::Relaxed);
        self.max_bytes.store((max_bytes / fb * fb).max(fb), Ordering::Relaxed);
        self.clear();
    }

    /// Drop everything queued, keeping the bounds. Used when a run starts and whenever the
    /// playback session is reopened, since audio buffered across an outage is stale.
    fn clear(&self) {
        self.buf.lock().unwrap().clear();
    }

    /// Append client PCM, dropping the OLDEST whole frames once the queue passes
    /// the byte bound so the newest audio is always retained. Drops stay
    /// frame-aligned: trimming mid-frame would phase-shift every later drain into
    /// interleaved garbage.
    fn push(&self, data: &[u8]) {
        let max = self.max_bytes.load(Ordering::Relaxed);
        let fb = self.frame_bytes.load(Ordering::Relaxed);
        let mut q = self.buf.lock().unwrap();
        q.extend(data.iter().copied());
        let over = q.len().saturating_sub(max);
        if over > 0 {
            let drop = (over.div_ceil(fb) * fb).min(q.len());
            q.drain(..drop);
        }
    }

    /// Drain up to `n` bytes into `out`, clamped to what is queued and floored to a
    /// whole frame, since a PA write must be a multiple of the sample-spec frame size.
    fn drain_upto(&self, n: usize, out: &mut Vec<u8>) {
        let fb = self.frame_bytes.load(Ordering::Relaxed);
        let mut q = self.buf.lock().unwrap();
        let mut take = n.min(q.len());
        take -= take % fb;
        out.clear();
        out.extend(q.drain(..take));
    }
}

/// Turns the mic uplink back into PCM: the client always sends the mic as Opus, so
/// every inbound packet must be decoded before it can be queued for the virtual sink. It
/// decodes one packet to interleaved S16LE PCM, reusing a scratch buffer across calls to
/// stay off the per-decode allocation path. Lives behind a `Mutex` on `PbShared` and is
/// driven from `write` / `write_red`.
struct OpusPlaybackDecoder {
    dec: opus::Decoder,
    channels: usize,
    pcm: Vec<i16>,
    /// RFC 2198 RED recovery cursor: the timestamp of the last frame decoded, so a
    /// redundant copy of a dropped frame is decoded exactly once, in order. `None` until
    /// the first RED frame arrives.
    last_ts: Option<i64>,
}

impl OpusPlaybackDecoder {
    /// Create a mono/stereo Opus decoder for the mic uplink; `None` if creation
    /// fails. `channels <= 1` decodes as mono, otherwise stereo.
    fn new(sample_rate: u32, channels: i32) -> Option<Self> {
        let ch = if channels <= 1 { Channels::Mono } else { Channels::Stereo };
        let dec = opus::Decoder::new(sample_rate, ch).ok()?;
        Some(OpusPlaybackDecoder {
            dec,
            channels: channels.max(1) as usize,
            pcm: Vec::new(),
            last_ts: None,
        })
    }

    /// Decode one Opus packet and return the interleaved S16LE PCM as bytes, or `None`
    /// for an empty or undecodable packet.
    ///
    /// The scratch `pcm` buffer is grown once to `5760 * channels` — an Opus packet decodes
    /// to at most 120 ms, which is 5760 samples per channel at 48 kHz — then reused across
    /// calls, and the result is a view straight over it, so a decode never allocates. The
    /// view borrows the decoder, so callers must queue it before decoding the next packet.
    /// Reinterpreting the samples as S16LE bytes assumes a little-endian host, exactly as
    /// the capture side does when it fills `accum` from PulseAudio fragments.
    fn decode_to_pcm(&mut self, packet: &[u8]) -> Option<&[u8]> {
        if packet.is_empty() {
            return None;
        }
        let cap = 5760 * self.channels;
        if self.pcm.len() < cap {
            self.pcm.resize(cap, 0);
        }
        let samples = self.dec.decode(packet, &mut self.pcm[..cap], false).ok()?;
        let n = samples * self.channels;
        Some(bytemuck::cast_slice(&self.pcm[..n]))
    }

    /// Reconstruct the mic uplink across packet loss: recover any frames the sender
    /// dropped from the redundant copies RED carries, and decode each new frame exactly once
    /// into `queue`. This is what lets a lossy UDP/WebRTC uplink play through gaps without ever
    /// waiting for a retransmit. It all runs off the GIL — the work is pure byte-slicing plus
    /// Opus decode with no Python state, so releasing the GIL keeps the mic path from
    /// serializing behind the rest of the interpreter.
    ///
    /// 1. **Parse the block headers**: walk the redundant headers (F bit set) collecting each
    ///    block's 14-bit timestamp offset and 10-bit length, then consume the 1-byte primary
    ///    header (F bit clear). A truncated payload, or more redundancy than `RED_MAX_DISTANCE`,
    ///    bails out.
    /// 2. **Resolve block boundaries**: turn the headers into `(ts, start, len)` triples,
    ///    oldest-first, where each redundant `ts` is `primary_ts - offset` and the primary is
    ///    whatever bytes remain.
    /// 3. **Anchor the first packet**: with no prior `last_ts`, decode only the primary and set
    ///    `last_ts` to it — its trailing redundancy describes frames never played, so it is not
    ///    replayed.
    /// 4. **Recover and advance**: otherwise decode every block whose `ts` is strictly newer
    ///    than `last_ts` (in oldest-first order, so a gap left by a dropped packet is filled
    ///    before the primary), pushing PCM to `queue` and advancing `last_ts`. Blocks at or
    ///    below `last_ts` are already-played duplicates and are skipped — the timestamp dedup
    ///    that makes redundancy free of double-decoding under no loss.
    fn decode_red_into_queue(&mut self, payload: &[u8], primary_ts: i64, queue: &PlayQueue) {
        let n = payload.len();
        let mut i = 0usize;
        let mut offs = [0i64; RED_MAX_DISTANCE as usize];
        let mut lens = [0usize; RED_MAX_DISTANCE as usize];
        let mut nh = 0usize;
        while i < n && (payload[i] & 0x80) != 0 {
            if i + 4 > n || nh >= RED_MAX_DISTANCE as usize {
                return;
            }
            let field = ((payload[i + 1] as u32) << 16)
                | ((payload[i + 2] as u32) << 8)
                | (payload[i + 3] as u32);
            offs[nh] = ((field >> 10) & 0x3FFF) as i64;
            lens[nh] = (field & 0x3FF) as usize;
            nh += 1;
            i += 4;
        }
        if i >= n {
            return;
        }
        i += 1;

        let mut frames = [(0i64, 0usize, 0usize); RED_MAX_DISTANCE as usize + 1];
        let mut nf = 0usize;
        for k in 0..nh {
            if i + lens[k] > n {
                return;
            }
            if lens[k] > 0 {
                frames[nf] = (primary_ts - offs[k], i, lens[k]);
                nf += 1;
            }
            i += lens[k];
        }
        frames[nf] = (primary_ts, i, n - i);
        nf += 1;

        if self.last_ts.is_none() {
            let (ts, start, len) = frames[nf - 1];
            if len > 0 {
                if let Some(pcm) = self.decode_to_pcm(&payload[start..start + len]) {
                    queue.push(pcm);
                }
            }
            self.last_ts = Some(ts);
            return;
        }
        let mut last = self.last_ts.unwrap();
        for &(ts, start, len) in frames.iter().take(nf) {
            // RTP timestamps are 32-bit and wrap; compare serial-number style so a
            // wraparound isn't mistaken for "already played" (ts > last would fail
            // for every frame after the rollover).
            let newer = ((ts.wrapping_sub(last)) as i32) > 0;
            if len > 0 && newer {
                if let Some(pcm) = self.decode_to_pcm(&payload[start..start + len]) {
                    queue.push(pcm);
                }
                last = ts;
            }
        }
        self.last_ts = Some(last);
    }
}

/// Per-`AudioPlayback` shared handle — the mirror of `Shared` for the playback path.
///
/// Reuses the capture lifecycle core on `Inner` (the `stop_state` protocol, the start
/// handshake, and the worker-tid re-entrancy guard); the Opus/silence settings mirrors on
/// `Inner` are unused here. Adds the bounded PCM `queue` and the always-Opus mic decoder.
struct PbShared {
    inner: Arc<Inner>,
    thread: Mutex<Option<JoinHandle<()>>>,
    queue: Arc<PlayQueue>,
    /// The mic uplink is always Opus; `write` / `write_red` decode each packet through this
    /// before enqueuing PCM. Set at `start()`; `None` only before a successful start.
    opus_dec: Mutex<Option<OpusPlaybackDecoder>>,
}

/// Process-wide registry of live playbacks, swept by the same atexit sweep as
/// captures. Holds `Weak` references so it keeps nothing alive on its own.
static PLAYBACK_REGISTRY: OnceLock<Mutex<Vec<Weak<PbShared>>>> = OnceLock::new();
/// Lazily initialize and return the playback registry.
fn playback_registry() -> &'static Mutex<Vec<Weak<PbShared>>> {
    PLAYBACK_REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// Run one bounded iteration of a PulseAudio standard mainloop:
/// `prepare(timeout_us)` → `poll` → `dispatch`. Returns `false` if any stage errors.
///
/// The `timeout_us` bound is what makes a pending stop observable within ~20 ms even when
/// the audio source delivers nothing, since every loop that calls this re-checks
/// `stop_pending()` between pumps.
fn pump(ml: &mut pulse::mainloop::standard::Mainloop, timeout_us: u64) -> bool {
    if ml.prepare(Some(MicroSeconds(timeout_us))).is_err() {
        return false;
    }
    if ml.poll().is_err() {
        return false;
    }
    if ml.dispatch().is_err() {
        return false;
    }
    true
}

/// Chromium's multistream-Opus surround layout for a channel count, as
/// `(streams, coupled, mapping)`; `None` for anything but 5.1 (6) or 7.1 (8).
///
/// The same tables are advertised in the WebRTC SDP (`multiopus`), so the browser's decoder
/// inverts exactly the stream/coupling/mapping this encoder applies.
fn multiopus_layout(channels: i32) -> Option<(i32, i32, &'static [u8])> {
    match channels {
        6 => Some((4, 2, &[0, 4, 1, 2, 3, 5])),
        8 => Some((5, 3, &[0, 6, 1, 2, 3, 4, 5, 7])),
        _ => None,
    }
}

/// One encode surface over both Opus APIs: the `opus` crate for mono/stereo, and
/// the raw multistream C API for 6/8-channel surround.
enum PcmEncoder {
    Stereo(opus::Encoder),
    Multi(MultiOpus),
}

/// Owning wrapper over a raw `OpusMSEncoder` (the surround multistream encoder).
///
/// The raw pointer is only ever touched from the capture thread that owns the enclosing
/// `RunState`, which is what makes the `unsafe impl Send` below sound; `Drop` destroys the
/// C encoder.
struct MultiOpus {
    st: *mut audiopus_sys::OpusMSEncoder,
}

unsafe impl Send for MultiOpus {}

impl Drop for MultiOpus {
    fn drop(&mut self) {
        unsafe { audiopus_sys::opus_multistream_encoder_destroy(self.st) }
    }
}

impl PcmEncoder {
    /// Build the Opus encoder for a channel count, selecting the API by width.
    ///
    /// - **Mono/stereo** (`channels <= 2`): the safe `opus` crate encoder in `LowDelay`
    ///   application mode; a failure to apply the initial bitrate or VBR mode is logged but
    ///   not fatal.
    /// - **Surround** (6/8): the raw multistream C encoder created from `multiopus_layout`
    ///   in `RESTRICTED_LOWDELAY`; an unsupported channel count is a hard error.
    ///
    /// Bitrate and VBR are applied at creation and can be retuned live via `set_bitrate`.
    fn new(sample_rate: u32, channels: i32, vbr: bool, bitrate: i32) -> Result<Self, String> {
        if channels <= 2 {
            let ch = if channels == 1 { Channels::Mono } else { Channels::Stereo };
            let mut enc = opus::Encoder::new(sample_rate, ch, Application::LowDelay)
                .map_err(|e| format!("opus_encoder_create() failed: {e:?}"))?;
            if let Err(e) = enc.set_bitrate(opus::Bitrate::Bits(bitrate)) {
                elog!("[pcmflux] WARNING: failed to apply initial bitrate: {e:?}");
            }
            if let Err(e) = enc.set_vbr(vbr) {
                elog!("[pcmflux] WARNING: failed to apply VBR mode: {e:?}");
            }
            return Ok(PcmEncoder::Stereo(enc));
        }
        let (streams, coupled, mapping) = multiopus_layout(channels)
            .ok_or_else(|| format!("unsupported surround channel count {channels}"))?;
        unsafe {
            let mut err: i32 = 0;
            let st = audiopus_sys::opus_multistream_encoder_create(
                sample_rate as i32,
                channels,
                streams,
                coupled,
                mapping.as_ptr(),
                audiopus_sys::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
                &mut err,
            );
            if st.is_null() || err != 0 {
                return Err(format!("opus_multistream_encoder_create() failed: {err}"));
            }
            if audiopus_sys::opus_multistream_encoder_ctl(
                st,
                audiopus_sys::OPUS_SET_BITRATE_REQUEST,
                bitrate,
            ) != 0
            {
                elog!("[pcmflux] WARNING: failed to apply initial surround bitrate");
            }
            if audiopus_sys::opus_multistream_encoder_ctl(
                st,
                audiopus_sys::OPUS_SET_VBR_REQUEST,
                vbr as i32,
            ) != 0
            {
                elog!("[pcmflux] WARNING: failed to apply surround VBR mode");
            }
            Ok(PcmEncoder::Multi(MultiOpus { st }))
        }
    }

    /// Encode one interleaved-PCM frame into `out`, returning the packet byte length.
    ///
    /// Dispatches to the `opus` crate (mono/stereo) or the raw multistream encode (surround,
    /// which needs the explicit `frame_size_per_channel`). Either error surfaces as a `String`.
    fn encode(
        &mut self,
        pcm: &[i16],
        frame_size_per_channel: usize,
        out: &mut [u8],
    ) -> Result<usize, String> {
        match self {
            PcmEncoder::Stereo(enc) => enc
                .encode(pcm, out)
                .map_err(|e| format!("opus_encode() failed: {e:?}")),
            PcmEncoder::Multi(ms) => unsafe {
                let n = audiopus_sys::opus_multistream_encode(
                    ms.st,
                    pcm.as_ptr(),
                    frame_size_per_channel as i32,
                    out.as_mut_ptr(),
                    out.len() as i32,
                );
                if n < 0 {
                    Err(format!("opus_multistream_encode() failed: {n}"))
                } else {
                    Ok(n as usize)
                }
            },
        }
    }

    /// Retune the encoder's target bitrate live (bits/s), for either API.
    fn set_bitrate(&mut self, bits: i32) -> Result<(), String> {
        match self {
            PcmEncoder::Stereo(enc) => enc
                .set_bitrate(opus::Bitrate::Bits(bits))
                .map_err(|e| format!("{e:?}")),
            PcmEncoder::Multi(ms) => unsafe {
                let ret = audiopus_sys::opus_multistream_encoder_ctl(
                    ms.st,
                    audiopus_sys::OPUS_SET_BITRATE_REQUEST,
                    bits,
                );
                if ret != 0 {
                    Err(format!("ctl error {ret}"))
                } else {
                    Ok(())
                }
            },
        }
    }
}

/// Backing store for the delivery ring: `Some(queue)` while open, `None` once closed
/// so `pop` wakes and returns `None` for a clean shutdown.
type FrameQueue = Option<VecDeque<(Vec<u8>, u64)>>;

/// Bounded, drop-oldest hand-off from the capture thread to the Python delivery
/// thread, so a slow or GIL-blocked callback can never stall the PulseAudio pump.
///
/// The capture thread `push`es encoded `(frame, pts)` pairs; the delivery thread blocks in
/// `pop`. Stale audio is worthless, so overflow past `capacity` (a few frames of slack)
/// discards the OLDEST frame and bumps `dropped`. `close` empties the queue to `None` and
/// wakes the consumer so it exits.
struct DeliveryRing {
    q: Mutex<FrameQueue>,
    cv: Condvar,
    dropped: AtomicU64,
    capacity: usize,
}

impl DeliveryRing {
    /// Create an open ring pre-sized to `capacity` frames.
    fn new(capacity: usize) -> Self {
        Self {
            q: Mutex::new(Some(VecDeque::with_capacity(capacity))),
            cv: Condvar::new(),
            dropped: AtomicU64::new(0),
            capacity,
        }
    }

    /// Enqueue one encoded frame, dropping the oldest (and bumping `dropped`) if the
    /// ring is at capacity, then wake the consumer. A no-op once closed.
    fn push(&self, data: Vec<u8>, pts: u64) {
        let mut g = self.q.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(q) = g.as_mut() {
            if q.len() >= self.capacity {
                q.pop_front();
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            q.push_back((data, pts));
            self.cv.notify_one();
        }
    }

    /// Block until a frame is available and return it, or return `None` once the ring
    /// is closed and drained — the delivery thread's loop condition.
    fn pop(&self) -> Option<(Vec<u8>, u64)> {
        let mut g = self.q.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match g.as_mut() {
                None => return None,
                Some(q) => {
                    if let Some(item) = q.pop_front() {
                        return Some(item);
                    }
                }
            }
            g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Close the ring: drop any queued frames and wake every waiter so `pop` returns
    /// `None`. Called during capture teardown to join the delivery thread.
    fn close(&self) {
        *self.q.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.cv.notify_all();
    }
}

/// Owns the delivery thread for the lifetime of one capture run and tears it down on
/// `Drop`, so teardown also happens when the capture thread UNWINDS.
///
/// Closing the ring is the delivery thread's only wake-up: skip it and the thread parks
/// in `pop()` forever, pinning the Python callback and leaving `deliver_tid` set to a tid
/// the OS may hand to an unrelated thread (whose `stop_capture` would then be mistaken
/// for a re-entrant self-stop and silently do nothing).
struct DeliveryThread<'a> {
    ring: Arc<DeliveryRing>,
    inner: &'a Inner,
    join: Option<JoinHandle<()>>,
}

impl Drop for DeliveryThread<'_> {
    fn drop(&mut self) {
        self.ring.close();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        self.inner.deliver_tid.store(0, Ordering::Release);
    }
}

/// Recycles outgoing frame buffers from dropped `AudioFrame`s back to the capture
/// thread, so the steady-state emit path allocates nothing.
///
/// INVARIANT: every buffer is born as `vec![0u8; buf_size]`, so bytes `[0, buf_size)` stay
/// initialized for the allocation's whole lifetime — `truncate` only shortens `len`, never
/// de-initializes memory. That is what makes `restore`'s `set_len` back to `buf_size` sound.
struct BufferPool {
    bufs: Mutex<Vec<Vec<u8>>>,
    buf_size: usize,
}

impl BufferPool {
    /// Cap on pooled buffers. Outstanding frames rarely exceed the delivery-ring
    /// capacity plus a few Python-held references; anything past this goes back to the
    /// allocator rather than growing the pool unboundedly.
    const MAX_POOLED: usize = 16;

    /// Create an empty pool that hands out (and accepts) `buf_size`-byte buffers.
    fn new(buf_size: usize) -> Self {
        Self { bufs: Mutex::new(Vec::new()), buf_size }
    }

    /// Take a fully initialized buffer of exactly `buf_size` length, recycling a
    /// pooled one when available. The runtime path takes through `PoolTaker`; this direct,
    /// locking form serves the unit tests.
    #[cfg(test)]
    fn take(&self) -> Vec<u8> {
        let recycled = self.bufs.lock().unwrap_or_else(|e| e.into_inner()).pop();
        match recycled {
            Some(v) => self.restore(v),
            None => vec![0u8; self.buf_size],
        }
    }

    /// Restore a recycled buffer to full `buf_size` length via `set_len`.
    ///
    /// Sound per the pool invariant: the bytes were written at allocation and truncation
    /// does not de-initialize them, so extending `len` back to `buf_size` never exposes
    /// uninitialized memory.
    fn restore(&self, mut v: Vec<u8>) -> Vec<u8> {
        debug_assert!(v.capacity() >= self.buf_size);
        unsafe { v.set_len(self.buf_size) };
        v
    }

    /// Return a buffer to the pool, unless it is undersized or the pool is already at
    /// `MAX_POOLED` (in which case it is dropped to the allocator).
    fn put(&self, v: Vec<u8>) {
        if v.capacity() < self.buf_size {
            return;
        }
        let mut g = self.bufs.lock().unwrap_or_else(|e| e.into_inner());
        if g.len() < Self::MAX_POOLED {
            g.push(v);
        }
    }

    /// Move every pooled buffer into `into` under a single lock — the batched refill
    /// for the sole-consumer `PoolTaker`, whose empty local stash is `into`.
    fn drain_into(&self, into: &mut Vec<Vec<u8>>) {
        let mut g = self.bufs.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::swap(&mut *g, into);
    }
}

/// Sole-consumer view over the shared `BufferPool` for the capture thread.
///
/// Refills are batched into a `local` stash, so the per-frame `take` is lock-free in the
/// steady state, while returns from the delivery/Python side (`AudioFrame` drops) still go
/// back through the shared pool. Only the capture thread owns a `PoolTaker`.
struct PoolTaker {
    pool: Arc<BufferPool>,
    local: Vec<Vec<u8>>,
}

impl PoolTaker {
    /// Wrap a shared pool with an empty local stash.
    fn new(pool: Arc<BufferPool>) -> Self {
        Self { pool, local: Vec::new() }
    }

    /// Take one `buf_size` buffer: pop from the local stash, batch-refilling it from
    /// the shared pool (one lock) only when empty, and allocating fresh when both are empty.
    fn take(&mut self) -> Vec<u8> {
        if self.local.is_empty() {
            self.pool.drain_into(&mut self.local);
        }
        match self.local.pop() {
            Some(v) => self.pool.restore(v),
            None => vec![0u8; self.pool.buf_size],
        }
    }

    /// Return a buffer on an error path without locking — it stays in the local stash.
    fn put(&mut self, v: Vec<u8>) {
        if v.capacity() >= self.pool.buf_size && self.local.len() < BufferPool::MAX_POOLED {
            self.local.push(v);
        }
    }
}

/// Per-run encode/deliver state, living on the capture thread's stack for the
/// lifetime of one capture. Holds the encoder, the frame-reassembly buffers, the outgoing
/// buffer recycler, the RED redundancy history, and the running debug-log counters.
struct RunState<'a> {
    inner: &'a Inner,
    ring: &'a DeliveryRing,
    encoder: PcmEncoder,
    frame_size_per_channel: usize,
    channels: usize,
    /// Reassembly buffer for exactly one Opus frame (`i16` samples, filled byte-wise from
    /// the incoming PulseAudio fragments).
    accum: Vec<i16>,
    /// A zeroed reference of the same length as `accum`; comparing `accum == silence_ref`
    /// lowers to a single vectorized memcmp for the silence gate, versus a scalar per-sample
    /// scan.
    silence_ref: Vec<i16>,
    pcm_fill_bytes: usize,
    /// Outgoing-buffer recycler, shared with delivered `AudioFrame`s whose drop refills it.
    pool: PoolTaker,
    /// RFC 2198 redundancy history: the last `red_distance` emitted `(opus, pts)` frames,
    /// oldest-first. Per-run — reset on start, and the frame size is fixed for a run.
    red_history: VecDeque<(Vec<u8>, u64)>,
    /// Retired `red_history` buffers, reused for the next entry so the steady state (a
    /// silence gap included) allocates nothing. Bounded by `red_distance`: every buffer
    /// is either in the history or here.
    red_spare: Vec<Vec<u8>>,
    red_distance: usize,
    total_samples_processed: u64,
    first_sound_detected: bool,
    current_applied_bitrate: i32,
    chunks_read: u64,
    chunks_silent: u64,
    chunks_encoded: u64,
    bytes_encoded: u64,
}

impl<'a> RunState<'a> {
    /// Feed one PulseAudio PCM fragment into the reassembly buffer, emitting a frame
    /// each time `accum` fills to exactly one Opus frame.
    ///
    /// Fragments arrive at arbitrary byte boundaries, so this copies from `src` into `accum`
    /// at `pcm_fill_bytes`, calling `emit_frame` (and resetting the fill cursor) whenever a
    /// full `frame_size_per_channel * channels * 2`-byte chunk accumulates, and loops until
    /// `src` is drained. Any partial remainder is carried into the next fragment.
    fn feed(&mut self, mut src: &[u8]) {
        let chunk_bytes = self.frame_size_per_channel * self.channels * 2;
        while !src.is_empty() {
            let want = chunk_bytes - self.pcm_fill_bytes;
            let take = want.min(src.len());
            {
                let dst: &mut [u8] = bytemuck::cast_slice_mut(&mut self.accum);
                dst[self.pcm_fill_bytes..self.pcm_fill_bytes + take]
                    .copy_from_slice(&src[..take]);
            }
            self.pcm_fill_bytes += take;
            src = &src[take..];
            if self.pcm_fill_bytes == chunk_bytes {
                self.emit_frame();
                self.pcm_fill_bytes = 0;
            }
        }
    }

    /// Encode one reassembled frame and hand it to the delivery thread. The heart of
    /// the capture encode path.
    ///
    /// 1. **Dynamic bitrate**: re-reads the `opus_bitrate` mirror and reconfigures the encoder
    ///    only when it changed, so a live bitrate update costs nothing on unchanged frames.
    /// 2. **Timestamp**: `pts` is the running 48 kHz-domain sample count
    ///    (`total_samples_processed`) *before* this frame, then advanced by
    ///    `frame_size_per_channel` — a monotonic per-frame timestamp used for RED offsets and
    ///    client-side ordering.
    /// 3. **Silence gate**: when enabled, a frame equal to the zeroed `silence_ref` is counted
    ///    and dropped (nothing is sent), so pure silence costs no bandwidth. The first
    ///    non-silent frame logs once.
    /// 4. **Encode in place**: `write_ws_prefix_into` writes the RFC 2198 RED framing prefix
    ///    (which depends only on `pts` + history) into a pooled buffer, and the Opus packet is
    ///    encoded DIRECTLY after it — no assembly copy, and the buffer recycles through the
    ///    pool, so the steady state allocates nothing. An encode error or a zero-length packet
    ///    returns the buffer to the pool and drops the frame.
    /// 5. **Retain redundancy**: with `red_distance > 0`, the just-encoded primary is copied
    ///    onto `red_history` (bounded, oldest-first) to serve as a future redundant copy,
    ///    into the buffer the retiring entry hands back.
    /// 6. **Hand off**: the truncated buffer is pushed to the `DeliveryRing`; the capture
    ///    thread itself never touches the GIL.
    fn emit_frame(&mut self) {
        self.chunks_read += 1;

        let requested = self.inner.opus_bitrate.load(Ordering::Relaxed);
        if requested != self.current_applied_bitrate {
            match self.encoder.set_bitrate(requested) {
                Ok(()) => {
                    plog!(
                        "[pcmflux] Dynamic Bitrate Update: {} -> {} kbps",
                        self.current_applied_bitrate / 1000,
                        requested / 1000
                    );
                    self.current_applied_bitrate = requested;
                }
                Err(e) => {
                    // current_applied_bitrate stays put, so the next frame retries the
                    // rejected value instead of latching it as if it had been applied.
                    elog!("[pcmflux] Failed to update bitrate ({requested}): {e:?}");
                }
            }
        }

        let pts = self.total_samples_processed;
        self.total_samples_processed += self.frame_size_per_channel as u64;

        if self.inner.use_silence_gate.load(Ordering::Relaxed)
            && self.accum == self.silence_ref
        {
            self.chunks_silent += 1;
            // Flush the RED backlog: if these pre-silence frames were kept, the first
            // packet after a long quiet stretch would ship minutes-old audio as
            // "redundant" data, and a receiver could reconstruct it into the gap. The
            // emptied buffers are kept for reuse, so a silence gap costs no allocations.
            self.red_spare
                .extend(self.red_history.drain(..).map(|(v, _)| v));
            return;
        }
        if !self.first_sound_detected {
            plog!("[pcmflux] First non-silent audio chunk detected! Encoding...");
            self.first_sound_detected = true;
        }

        let n = self.frame_size_per_channel * self.channels;
        let emit_header = self.inner.emit_audio_header.load(Ordering::Relaxed);
        let mut data = self.pool.take();
        let prefix = write_ws_prefix_into(
            &mut data,
            pts,
            &self.red_history,
            self.red_distance,
            emit_header,
        );
        let encoded = match self.encoder.encode(
            &self.accum[..n],
            self.frame_size_per_channel,
            &mut data[prefix..],
        ) {
            Ok(b) => b,
            Err(e) => {
                elog!("[pcmflux] ERROR: {e}");
                self.pool.put(data);
                return;
            }
        };
        if encoded == 0 {
            self.pool.put(data);
            return;
        }
        self.chunks_encoded += 1;
        self.bytes_encoded += encoded as u64;
        if self.red_distance > 0 {
            let mut slot = if self.red_history.len() >= self.red_distance {
                self.red_history.pop_front().map(|(v, _)| v).unwrap_or_default()
            } else {
                self.red_spare.pop().unwrap_or_default()
            };
            slot.clear();
            slot.extend_from_slice(&data[prefix..prefix + encoded]);
            self.red_history.push_back((slot, pts));
        }
        data.truncate(prefix + encoded);

        self.ring.push(data, pts);
    }
}

/// Own one whole capture run end to end on a dedicated thread — connect PulseAudio,
/// encode, and deliver — until stopped. It runs on its own thread because the PulseAudio
/// mainloop must be pumped continuously and independently of Python: sharing the caller's
/// thread would tie capture cadence to the GIL and let any Python stall starve the audio.
/// The body handed to `spawn_worker`.
///
/// Publishes `start_state` for the handshake (`FAILED` on any startup error, `RUNNING` on
/// entering the hot loop) and returns when `stop_state` leaves `STOP_NONE` or on a fatal
/// error. The startup sequence, in order:
///
/// 1. **Seed the mirrors + validate**: copies the settings snapshot into the `Inner`
///    per-frame atomics, and rejects an invalid Opus frame duration, channel count, or
///    sample spec before touching PulseAudio.
/// 2. **Buffer attr / latency**: a configured `latency_ms` uses `ADJUST_LATENCY` with
///    `fragsize` set to that latency; otherwise `fragsize` is floored at ~20 ms, which yields
///    a prompt first frame and avoids PipeWire's ~2 s default fragment.
/// 3. **Connect + probe**: drives the context to `Ready` on the bounded pump (re-checking
///    `stop_pending` each turn), and up-front validates a NAMED device via an introspect
///    probe — an async `connect_record` would not fail synchronously on a bad name. The probe
///    closure is called from C inside mainloop dispatch, so its body is `catch_unwind`-guarded
///    to keep a panic from unwinding across the FFI boundary.
/// 4. **Encoder + record stream**: creates the `PcmEncoder` (mono/stereo or surround) and
///    drives the record stream to `Ready`.
/// 5. **Delivery thread**: spawns the delivery thread that pops from the `DeliveryRing` and
///    runs the Python callback there, so GIL stalls cannot back up the PA pump; a callback
///    error is reported as an unraisable exception and never propagates into the loop. The
///    buffer pool is sized to the worst-case body — RED prefix plus a max Opus packet, scaled
///    by stream count for surround (one self-delimited packet per stream).
/// 6. **Hot loop**: `pump`s on the ~20 ms bound, then drains every buffered fragment via
///    peek/discard (a `Hole` is an xrun — the read index is just advanced), feeding each into
///    `RunState`. A stop is observed within the pump bound even when the source is wedged. On
///    exit it disconnects the stream, drops the encoder, closes and joins the delivery ring,
///    and reports any dropped stale frames.
/// One PulseAudio session for capture: mainloop, context, and the record stream,
/// all recreated together on reconnect.
/// Drop order matters (declaration order): the stream must die first, then its
/// owning context, then the mainloop both pulse threads pump — the reverse of
/// the build. A wrong order is a use-after-free on the libpulse side.
struct PaCaptureSession {
    stream: Stream,
    /// Must outlive the stream (the connection owns it); never read after open.
    #[allow(dead_code)]
    context: Context,
    mainloop: Mainloop,
}

/// Why a session failed to open: drives the retry policy of the caller.
enum SessionOpenError {
    /// The named source is absent at startup (misconfiguration-ish); the caller gives it
    /// only its short bring-up window rather than the full startup retry budget.
    DeviceNotFound(String),
    /// Server down, busy, or a bring-up race: retryable.
    Transient(String),
    /// stop_pending observed while opening; caller must shut down cleanly.
    Aborted,
}

/// Open a capture session: mainloop + context + record stream driven to `Ready` on the
/// bounded pump, honoring `stop_pending` at every turn. A NAMED device is validated by
/// an introspect probe on every call (an async connect_record would not fail
/// synchronously on a bad name, and on reconnect this is also what notices the device
/// reappearing after an outage). `device_was_present` distinguishes initial bring-up
/// from reconnect: a named device missing at startup is probably a misconfiguration, so
/// it is reported as `DeviceNotFound` and the caller spends only a short window on it;
/// the same device vanishing mid-run (PulseAudio/PipeWire restart kills every source) is
/// transient and must be retried or audio never comes back.
fn pa_capture_session_open(
    inner: &Inner,
    spec: &Spec,
    device: Option<&str>,
    attr: &BufferAttr,
    adjust_latency: bool,
    device_was_present: bool,
) -> Result<PaCaptureSession, SessionOpenError> {
    let tr = |e: &str| SessionOpenError::Transient(e.to_string());
    let mut mainloop = match Mainloop::new() {
        Some(m) => m,
        None => return Err(tr("pa_mainloop_new() failed")),
    };
    let mut context = match Context::new(&mainloop, "pcmflux") {
        Some(c) => c,
        None => return Err(tr("pa_context_new() failed")),
    };
    if context.connect(None, CtxFlags::NOFLAGS, None).is_err() {
        return Err(tr("pa_context_connect() failed"));
    }
    loop {
        let st = context.get_state();
        if st == pulse::context::State::Ready {
            break;
        }
        if !st.is_good() {
            return Err(tr("PulseAudio context connection failed"));
        }
        if inner.stop_pending() {
            return Err(SessionOpenError::Aborted);
        }
        if !pump(&mut mainloop, PUMP_TIMEOUT_US) {
            return Err(tr("mainloop iterate failed during connect"));
        }
    }

    if let Some(dev) = device {
        let probe = Arc::new(Mutex::new((false, false)));
        let p2 = probe.clone();
        let op = context.introspect().get_source_info_by_name(dev, move |res| {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut g = p2.lock().unwrap();
                match res {
                    ListResult::Item(_) => g.0 = true,
                    ListResult::End | ListResult::Error => g.1 = true,
                }
            }));
        });
        loop {
            if probe.lock().unwrap().1 {
                break;
            }
            if inner.stop_pending() {
                drop(op);
                return Err(SessionOpenError::Aborted);
            }
            if !context.get_state().is_good() {
                drop(op);
                return Err(tr("context failed during source probe"));
            }
            if !pump(&mut mainloop, PUMP_TIMEOUT_US) {
                drop(op);
                return Err(tr("mainloop iterate failed during source probe"));
            }
        }
        drop(op);
        if !probe.lock().unwrap().0 {
            let msg = format!("PulseAudio source not found: '{dev}'");
            return Err(if device_was_present {
                // Mid-run: the server was just restarted; its sources are all
                // gone for now, not misconfigured.
                SessionOpenError::Transient(msg)
            } else {
                SessionOpenError::DeviceNotFound(msg)
            });
        }
    }

    let mut stream = match Stream::new(&mut context, "Audio Capture", spec, None) {
        Some(s) => s,
        None => return Err(tr("pa_stream_new() failed")),
    };
    let flags = if adjust_latency {
        StreamFlags::ADJUST_LATENCY
    } else {
        StreamFlags::NOFLAGS
    };
    if stream.connect_record(device, Some(attr), flags).is_err() {
        return Err(tr("pa_stream_connect_record() failed"));
    }
    loop {
        let st = stream.get_state();
        if st == pulse::stream::State::Ready {
            break;
        }
        if !st.is_good() {
            return Err(SessionOpenError::Transient(format!(
                "PulseAudio record stream failed (device '{}')",
                device.unwrap_or("default")
            )));
        }
        if inner.stop_pending() {
            return Err(SessionOpenError::Aborted);
        }
        if !pump(&mut mainloop, PUMP_TIMEOUT_US) {
            return Err(tr("mainloop iterate failed during stream connect"));
        }
    }
    Ok(PaCaptureSession {
        stream,
        context,
        mainloop,
    })
}

fn capture_run(inner: &Arc<Inner>, settings: &Settings, callback: &Py<PyAny>) {
    inner.opus_bitrate.store(settings.opus_bitrate, Ordering::Relaxed);
    inner.use_silence_gate.store(settings.use_silence_gate, Ordering::Relaxed);
    inner.debug_logging.store(settings.debug_logging, Ordering::Relaxed);
    inner.emit_audio_header.store(!settings.omit_audio_header, Ordering::Relaxed);

    let fail = || {
        inner.started_ok.store(false, Ordering::Release);
        inner.start_state.store(ST_FAILED, Ordering::Release);
    };

    if !valid_opus_duration(settings.frame_duration_ms) {
        elog!(
            "[pcmflux] ERROR: invalid frame_duration_ms ({}). Must be one of 2.5,5,10,20,40,60.",
            settings.frame_duration_ms
        );
        fail();
        return;
    }
    if !matches!(settings.channels, 1 | 2 | 6 | 8) {
        elog!(
            "[pcmflux] ERROR: channels must be 1, 2, 6 or 8 (got {}).",
            settings.channels
        );
        fail();
        return;
    }

    let spec = Spec {
        format: Format::S16le,
        rate: settings.sample_rate,
        channels: settings.channels as u8,
    };
    if !spec.is_valid() {
        elog!("[pcmflux] ERROR: invalid sample spec.");
        fail();
        return;
    }

    let mut attr = BufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: u32::MAX,
    };
    let adjust_latency = settings.latency_ms > 0;
    if adjust_latency {
        attr.fragsize =
            spec.usec_to_bytes(MicroSeconds(settings.latency_ms as u64 * 1000)) as u32;
    } else {
        attr.fragsize = spec.usec_to_bytes(MicroSeconds(20 * 1000)) as u32;
    }

    let device = settings.device_name.as_deref();

    let encoder = match PcmEncoder::new(
        settings.sample_rate,
        settings.channels,
        settings.use_vbr,
        settings.opus_bitrate,
    ) {
        Ok(e) => e,
        Err(e) => {
            elog!("[pcmflux] ERROR: {e}");
            fail();
            return;
        }
    };
    plog!("[pcmflux] SUCCESS: Opus encoder created ({} ch).", settings.channels);

    let frame_size_per_channel =
        (settings.sample_rate as f64 * settings.frame_duration_ms / 1000.0) as usize;
    let channels = settings.channels as usize;

    let ring = Arc::new(DeliveryRing::new(8));
    // Surround encodes one self-delimited packet per multistream stream (4 for 5.1, 5 for
    // 7.1), so the worst-case body scales with the stream count of the actual layout.
    let max_pkt = multiopus_layout(settings.channels)
        .map_or(1, |(streams, _, _)| streams as usize)
        * MAX_OPUS_PACKET;
    let pool = Arc::new(BufferPool::new(RED_PREFIX_MAX + max_pkt));
    let deliver_ring = Arc::clone(&ring);
    let deliver_pool = Arc::clone(&pool);
    let deliver_inner = Arc::clone(inner);
    let deliver_cb: Py<PyAny> = Python::attach(|py| callback.clone_ref(py));
    let spawned = std::thread::Builder::new()
        .name("pcmflux-deliver".into())
        .spawn(move || {
            unsafe {
                let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
                let _ = libc::setpriority(libc::PRIO_PROCESS, tid, -10);
            }
            deliver_inner.deliver_tid.store(gettid(), Ordering::Release);
            while let Some((data, pts)) = deliver_ring.pop() {
                Python::attach(|py| {
                    let frame = match Py::new(
                        py,
                        AudioFrame { data, pts, pool: Some(Arc::clone(&deliver_pool)) },
                    ) {
                        Ok(f) => f,
                        Err(e) => {
                            elog!("[pcmflux] AudioFrame alloc failed: {e:?}");
                            return;
                        }
                    };
                    if let Err(e) = deliver_cb.call1(py, (frame,)) {
                        e.write_unraisable(py, Some(deliver_cb.bind(py)));
                    }
                });
            }
            deliver_inner.deliver_tid.store(0, Ordering::Release);
        });
    let delivery = match spawned {
        Ok(join) => DeliveryThread {
            ring: Arc::clone(&ring),
            inner,
            join: Some(join),
        },
        Err(_) => {
            elog!("[pcmflux] ERROR: delivery thread spawn failed.");
            fail();
            return;
        }
    };

    let mut run = RunState {
        inner,
        ring: &ring,
        encoder,
        frame_size_per_channel,
        channels,
        accum: vec![0i16; frame_size_per_channel * channels],
        silence_ref: vec![0i16; frame_size_per_channel * channels],
        pcm_fill_bytes: 0,
        pool: PoolTaker::new(Arc::clone(&pool)),
        red_history: VecDeque::new(),
        red_spare: Vec::new(),
        red_distance: settings.red_distance.max(0) as usize,
        total_samples_processed: 0,
        first_sound_detected: false,
        current_applied_bitrate: settings.opus_bitrate,
        chunks_read: 0,
        chunks_silent: 0,
        chunks_encoded: 0,
        bytes_encoded: 0,
    };

    let mut last_log = Instant::now();

    // Session loop: the record stream CAN die mid-run (PulseAudio/PipeWire restart,
    // source unplugged) and a plain `break` there leaves audio dead until some
    // unrelated settings change restarts the capture. Reopen with backoff instead.
    // Three retry budgets, shortest first:
    //   - DEVICE_WAIT_TRIES: a start that finds the NAMED source missing. The sink whose
    //     monitor is being recorded may still be materializing (container bring-up, or a
    //     capture start that raced a server restart), but a misconfigured name must still
    //     surface quickly, so this window is only a few seconds.
    //   - START_TRIES: any other failure to bring the first session up.
    //   - RECONNECT_TRIES: a mid-run reconnect, which has to outlast a whole
    //     PulseAudio/PipeWire restart or audio never comes back.
    const DEVICE_WAIT_TRIES: u32 = 6;
    const START_TRIES: u32 = 12;
    const RECONNECT_TRIES: u32 = 40;
    let mut session: Option<PaCaptureSession> = None;
    let mut ever_connected = false;
    let mut tries: u32 = 0;
    let mut backoff_ms: u64 = 250;
    let mut terminal_error: Option<String> = None;

    loop {
        if inner.stop_pending() {
            break;
        }
        if session.is_none() {
            let opened =
                pa_capture_session_open(inner, &spec, device, &attr, adjust_latency, ever_connected);
            let cap = match (&opened, ever_connected) {
                (Err(SessionOpenError::DeviceNotFound(_)), _) => DEVICE_WAIT_TRIES,
                (_, true) => RECONNECT_TRIES,
                (_, false) => START_TRIES,
            };
            match opened {
                Ok(s) => {
                    session = Some(s);
                    tries = 0;
                    backoff_ms = 250;
                    if !ever_connected {
                        ever_connected = true;
                        inner.started_ok.store(true, Ordering::Release);
                        inner.start_state.store(ST_RUNNING, Ordering::Release);
                        plog!(
                            "[pcmflux] Capture loop started. Device: {}, Rate: {}, Channels: {}, Bitrate: {} kbps, \
                             VBR: {}, Silence Gate: {}",
                            device.unwrap_or("system_default"),
                            settings.sample_rate,
                            settings.channels,
                            settings.opus_bitrate / 1000,
                            if settings.use_vbr {
                                "On"
                            } else {
                                "Off"
                            },
                            if settings.use_silence_gate { "On" } else { "Off" }
                        );
                    } else {
                        plog!("[pcmflux] audio capture reconnected; resuming.");
                    }
                }
                Err(SessionOpenError::Aborted) => break,
                Err(SessionOpenError::DeviceNotFound(e)) | Err(SessionOpenError::Transient(e)) => {
                    tries += 1;
                    if tries >= cap {
                        terminal_error = Some(e);
                        break;
                    }
                    elog!("[pcmflux] audio capture open failed ({e}); retry {tries}/{cap} in {backoff_ms}ms");
                    let mut slept = 0u64;
                    while slept < backoff_ms && !inner.stop_pending() {
                        std::thread::sleep(Duration::from_millis(50));
                        slept += 50;
                    }
                    backoff_ms = (backoff_ms * 2).min(5000);
                    continue;
                }
            }
        }
        let s = session.as_mut().expect("session checked above");
        if !pump(&mut s.mainloop, PUMP_TIMEOUT_US) {
            elog!("[pcmflux] ERROR: mainloop iterate failed; reopening the session.");
            session = None;
            // Drop the partially reassembled frame: the next fragments come from after
            // the outage, and stitching them onto pre-outage PCM would emit one frame
            // with a discontinuity in the middle, charged to the wrong pts.
            run.pcm_fill_bytes = 0;
            continue;
        }
        let sstate = s.stream.get_state();
        if sstate != pulse::stream::State::Ready {
            elog!("[pcmflux] record stream lost; reopening the session.");
            session = None;
            run.pcm_fill_bytes = 0;
            continue;
        }

        loop {
            let mut discard = false;
            let mut done = false;
            match s.stream.peek() {
                Ok(PeekResult::Empty) => done = true,
                Ok(PeekResult::Hole(_)) => discard = true,
                Ok(PeekResult::Data(buf)) => {
                    run.feed(buf);
                    discard = true;
                }
                Err(_) => {
                    elog!("[pcmflux] ERROR: pa_stream_peek() failed.");
                    done = true;
                }
            }
            if discard {
                let _ = s.stream.discard();
            }
            if done {
                break;
            }
        }

        if run.inner.debug_logging.load(Ordering::Relaxed) {
            let elapsed = last_log.elapsed();
            if elapsed >= Duration::from_secs(2) {
                let secs = elapsed.as_secs_f64();
                let kbps = (run.bytes_encoded * 8) as f64 / (secs * 1000.0);
                let silent_pct = if run.chunks_read > 0 {
                    100.0 * run.chunks_silent as f64 / run.chunks_read as f64
                } else {
                    0.0
                };
                plog!(
                    "[pcmflux] Status | Read: {}, Silent: {} ({:.1}%), Encoded: {}, Rate: {:.2} kbps",
                    run.chunks_read, run.chunks_silent, silent_pct, run.chunks_encoded, kbps
                );
                last_log = Instant::now();
                run.chunks_read = 0;
                run.chunks_silent = 0;
                run.chunks_encoded = 0;
                run.bytes_encoded = 0;
            }
        }
    }

    if let Some(e) = terminal_error {
        elog!("[pcmflux] ERROR: audio capture could not stay connected (last error: {e}); stopping.");
        inner.started_ok.store(false, Ordering::Release);
        inner.start_state.store(ST_FAILED, Ordering::Release);
    } else {
        plog!("[pcmflux] Stop requested. Cleaning up capture loop...");
        inner.started_ok.store(false, Ordering::Release);
        if !ever_connected {
            // Stopped before the first session came up: resolve the startup handshake, or
            // the waiting `start_capture` polls out with the run still marked STARTING.
            inner.start_state.store(ST_FAILED, Ordering::Release);
        }
    }
    if let Some(s) = session.as_mut() {
        let _ = s.stream.disconnect();
    }
    drop(session);
    drop(run);
    drop(delivery);
    let dropped = ring.dropped.load(Ordering::Relaxed);
    if dropped > 0 {
        plog!("[pcmflux] Delivery ring dropped {dropped} stale frame(s) to a slow consumer.");
    }
    plog!("[pcmflux] Audio capture loop finished. Resources released.");
}


/// One PulseAudio session for playback: mainloop, context, and the playback stream, all
/// recreated together on reconnect — the mirror of `PaCaptureSession`, with the same
/// drop-order requirement (stream first, then its context, then the mainloop).
struct PaPlaybackSession {
    stream: Stream,
    /// Must outlive the stream (the connection owns it); never read after open.
    #[allow(dead_code)]
    context: Context,
    mainloop: Mainloop,
}

/// Open a playback session: mainloop + context + playback stream driven to `Ready` on the
/// bounded pump, honoring `stop_pending` at every turn. The mirror of
/// `pa_capture_session_open`.
///
/// Every failure is `Transient`: `connect_playback` resolves a sink name asynchronously,
/// so a wrong device name and a server that is still coming up are the same failed stream
/// state here, and the caller's retry budget is what bounds either one.
fn pa_playback_session_open(
    inner: &Inner,
    spec: &Spec,
    device: Option<&str>,
    attr: &BufferAttr,
) -> Result<PaPlaybackSession, SessionOpenError> {
    let tr = |e: &str| SessionOpenError::Transient(e.to_string());
    let mut mainloop = match Mainloop::new() {
        Some(m) => m,
        None => return Err(tr("pa_mainloop_new() failed (playback)")),
    };
    let mut context = match Context::new(&mainloop, "pcmflux") {
        Some(c) => c,
        None => return Err(tr("pa_context_new() failed (playback)")),
    };
    if context.connect(None, CtxFlags::NOFLAGS, None).is_err() {
        return Err(tr("pa_context_connect() failed (playback)"));
    }
    loop {
        let st = context.get_state();
        if st == pulse::context::State::Ready {
            break;
        }
        if !st.is_good() {
            return Err(tr("PulseAudio context connection failed (playback)"));
        }
        if inner.stop_pending() {
            return Err(SessionOpenError::Aborted);
        }
        if !pump(&mut mainloop, PUMP_TIMEOUT_US) {
            return Err(tr("mainloop iterate failed during connect (playback)"));
        }
    }

    let mut stream = match Stream::new(&mut context, "Microphone Playback", spec, None) {
        Some(s) => s,
        None => return Err(tr("pa_stream_new() failed (playback)")),
    };
    if stream
        .connect_playback(device, Some(attr), StreamFlags::ADJUST_LATENCY, None, None)
        .is_err()
    {
        return Err(SessionOpenError::Transient(format!(
            "pa_stream_connect_playback() failed (device '{}')",
            device.unwrap_or("default")
        )));
    }
    loop {
        let st = stream.get_state();
        if st == pulse::stream::State::Ready {
            break;
        }
        if !st.is_good() {
            return Err(SessionOpenError::Transient(format!(
                "PulseAudio playback stream failed (device '{}')",
                device.unwrap_or("default")
            )));
        }
        if inner.stop_pending() {
            return Err(SessionOpenError::Aborted);
        }
        if !pump(&mut mainloop, PUMP_TIMEOUT_US) {
            return Err(tr("mainloop iterate failed during stream connect (playback)"));
        }
    }
    Ok(PaPlaybackSession {
        stream,
        context,
        mainloop,
    })
}

/// Drive one whole mic-playback run on the playback thread. The body handed to
/// `spawn_worker`; the mirror of `capture_run` for the uplink.
///
/// This thread solely owns the PA playback stream, so writes are serialized structurally
/// with no executor. It mirrors `capture_run`'s lifecycle: `start_state` goes `RUNNING`
/// once the first session is up and `FAILED` when the run gives up, and it returns when
/// `stop_state` leaves `STOP_NONE` or the retry budget is spent.
///
/// **Session loop**: the sink can die under a live stream (PulseAudio/PipeWire restart,
/// sink removed). Breaking out there would leave the mic uplink dead until something
/// upstream noticed and restarted the whole playback, losing every packet in between, so
/// the session is reopened with the same backoff and budgets capture uses.
///
/// **Buffer sizing and the prebuf timing rule** (load-bearing): `tlength` is the target
/// latency in bytes, and `prebuf` is a quarter of it, floored to one frame. `prebuf` must
/// NOT be zero: with `prebuf == 0` PulseAudio starts playback instantly, the realtime read
/// pointer then runs ahead of the write index, and every `SeekMode::Relative` write lands
/// "in the past" — the server silently discards it forever (observed as bytes flowing at
/// exactly realtime rate while the sink monitor stayed silent). A quarter-buffer prebuf makes
/// the stream wait for data before starting and re-prebuffer after each underrun, so a late
/// chunk plays slightly delayed instead of vanishing.
///
/// **Hot loop**: `pump`s on the ~20 ms bound, then, whenever the server is writable, drains
/// that many bytes from the `PlayQueue` (clamped and frame-aligned) and writes them with
/// `free_cb = None`, so PA copies the bytes and the `scratch` buffer is reused next
/// iteration. Newly queued bytes are picked up on the next pump — no cross-thread wakeup is
/// needed, mirroring capture's poll style — and a stop is observed within the pump bound.
fn playback_run(inner: &Inner, settings: &PbSettings, queue: &PlayQueue) {
    inner.debug_logging.store(settings.debug_logging, Ordering::Relaxed);

    let fail = || {
        inner.started_ok.store(false, Ordering::Release);
        inner.start_state.store(ST_FAILED, Ordering::Release);
    };

    if settings.channels != 1 && settings.channels != 2 {
        elog!("[pcmflux] ERROR: playback channels must be 1 or 2 (got {}).", settings.channels);
        fail();
        return;
    }
    let spec = Spec {
        format: Format::S16le,
        rate: settings.sample_rate,
        channels: settings.channels as u8,
    };
    if !spec.is_valid() {
        elog!("[pcmflux] ERROR: invalid playback sample spec.");
        fail();
        return;
    }

    let device = settings.device_name.as_deref();
    plog!(
        "[pcmflux] Attempting to connect playback to PulseAudio device: {} (latency {}ms)",
        device.unwrap_or("system_default"),
        settings.latency_ms
    );

    let tlength =
        spec.usec_to_bytes(MicroSeconds(settings.latency_ms.max(0) as u64 * 1000)) as u32;
    let attr = BufferAttr {
        maxlength: u32::MAX,
        tlength,
        prebuf: (tlength / 4).max(spec.frame_size() as u32),
        minreq: u32::MAX,
        fragsize: u32::MAX,
    };

    // Retry budgets, matching capture: a start gets a short window, a mid-run reconnect
    // one long enough to outlast a whole PulseAudio/PipeWire restart.
    const START_TRIES: u32 = 12;
    const RECONNECT_TRIES: u32 = 40;
    let mut session: Option<PaPlaybackSession> = None;
    let mut ever_connected = false;
    let mut tries: u32 = 0;
    let mut backoff_ms: u64 = 250;
    let mut terminal_error: Option<String> = None;

    let mut scratch: Vec<u8> = Vec::new();
    let mut bytes_written: u64 = 0;
    let mut writable_hits: u64 = 0;
    let mut last_pb_log = Instant::now();

    loop {
        if inner.stop_pending() {
            break;
        }
        if session.is_none() {
            match pa_playback_session_open(inner, &spec, device, &attr) {
                Ok(s) => {
                    session = Some(s);
                    tries = 0;
                    backoff_ms = 250;
                    if !ever_connected {
                        ever_connected = true;
                        inner.started_ok.store(true, Ordering::Release);
                        inner.start_state.store(ST_RUNNING, Ordering::Release);
                        plog!(
                            "[pcmflux] Playback loop started. Device: {}, Rate: {}, Channels: {}, Latency: {}ms",
                            device.unwrap_or("system_default"),
                            settings.sample_rate,
                            settings.channels,
                            settings.latency_ms
                        );
                    } else {
                        // Mic audio queued during the outage is stale; playing it out
                        // would only push that much extra latency into the uplink.
                        queue.clear();
                        plog!("[pcmflux] audio playback reconnected; resuming.");
                    }
                }
                Err(SessionOpenError::Aborted) => break,
                Err(SessionOpenError::DeviceNotFound(e)) | Err(SessionOpenError::Transient(e)) => {
                    tries += 1;
                    let cap = if ever_connected { RECONNECT_TRIES } else { START_TRIES };
                    if tries >= cap {
                        terminal_error = Some(e);
                        break;
                    }
                    elog!("[pcmflux] audio playback open failed ({e}); retry {tries}/{cap} in {backoff_ms}ms");
                    let mut slept = 0u64;
                    while slept < backoff_ms && !inner.stop_pending() {
                        std::thread::sleep(Duration::from_millis(50));
                        slept += 50;
                    }
                    backoff_ms = (backoff_ms * 2).min(5000);
                    continue;
                }
            }
        }
        let s = session.as_mut().expect("session checked above");
        if !pump(&mut s.mainloop, PUMP_TIMEOUT_US) {
            elog!("[pcmflux] ERROR: mainloop iterate failed; reopening the playback session.");
            session = None;
            continue;
        }
        if s.stream.get_state() != pulse::stream::State::Ready {
            elog!("[pcmflux] playback stream lost; reopening the session.");
            session = None;
            continue;
        }
        if let Some(can) = s.stream.writable_size() {
            if can > 0 {
                writable_hits += 1;
                queue.drain_upto(can, &mut scratch);
                if !scratch.is_empty() {
                    if let Err(e) =
                        s.stream.write(&scratch, None, 0, pulse::stream::SeekMode::Relative)
                    {
                        elog!("[pcmflux] ERROR: pa_stream_write() failed: {e:?}");
                    } else {
                        bytes_written += scratch.len() as u64;
                    }
                }
            }
        }
        if inner.debug_logging.load(Ordering::Relaxed) && last_pb_log.elapsed().as_secs() >= 1 {
            plog!(
                "[pcmflux] Playback | writable_hits: {writable_hits}, bytes_written: {bytes_written}, queued: {}",
                queue.buf.lock().map(|q| q.len()).unwrap_or(0)
            );
            last_pb_log = Instant::now();
        }
    }

    if let Some(e) = terminal_error {
        elog!("[pcmflux] ERROR: audio playback could not stay connected (last error: {e}); stopping.");
        fail();
    } else {
        plog!("[pcmflux] Stop requested. Cleaning up playback loop...");
        inner.started_ok.store(false, Ordering::Release);
        if !ever_connected {
            // Stopped before the first session came up: resolve the startup handshake, so
            // `worker_alive` stops reporting a STARTING run that will never run.
            inner.start_state.store(ST_FAILED, Ordering::Release);
        }
    }
    if let Some(s) = session.as_mut() {
        let _ = s.stream.disconnect();
    }
    drop(session);
    plog!("[pcmflux] Audio playback loop finished. Resources released.");
}

/// Python-facing capture handle. Owns the `Shared` lifecycle state and exposes
/// `start_capture` / `stop_capture` / `update_audio_bitrate` / `is_capturing` to Python.
#[pyclass]
struct AudioCapture {
    shared: Arc<Shared>,
}

impl AudioCapture {
    fn inner(&self) -> &Arc<Inner> {
        &self.shared.inner
    }
}

#[pymethods]
impl AudioCapture {
    #[new]
    fn new() -> Self {
        AudioCapture {
            shared: Arc::new(Shared {
                inner: Arc::new(Inner::new()),
                thread: Mutex::new(None),
            }),
        }
    }

    /// Start (or restart) audio capture, delivering encoded frames to `callback`.
    ///
    /// 1. **Re-entrancy guard**: if called on one of the run's own threads — the delivery
    ///    thread (where the Python callback actually executes) or the capture thread — it
    ///    cannot join/recreate the run it is part of (the capture thread joins the delivery
    ///    thread on teardown, so a join from either closes a cycle), so it just undoes a
    ///    nested SELF-stop and returns. That undo is a compare-exchange that clears the stop
    ///    ONLY if this thread still owns it — if an external stop stored `STOP_EXTERNAL`
    ///    meanwhile, the CAS fails and that stop stands (clearing it would strand its
    ///    in-flight join forever).
    /// 2. **Spawn with the GIL released**: `spawn_worker` stops/joins any prior thread and
    ///    spawns the new one via `py.detach`, because the lifecycle lock and `join()` must not
    ///    be held while holding the GIL — joining the capture thread transitively joins the
    ///    delivery thread, whose in-flight callback needs the GIL, so that would deadlock.
    ///    The stop/clear ordering (the lost-stop invariant) lives in `spawn_worker`.
    /// 3. **Register** the handle for the atexit sweep (best-effort), pruning dead weaks.
    /// 4. **Startup handshake**: waits up to ~2 s (GIL released) for the thread to publish
    ///    `RUNNING` or `FAILED`. On `FAILED`, `join_failed_start` tears down ONLY the thread
    ///    this call spawned (identity-checked) — a concurrent start may already own the slot
    ///    with a live run that must survive — and the error is surfaced to Python.
    fn start_capture(
        &self,
        py: Python<'_>,
        settings: &Bound<'_, PyAny>,
        callback: Py<PyAny>,
    ) -> PyResult<()> {
        let inner = self.inner().clone();

        let me = gettid();
        if inner.is_own_thread(me) {
            inner.undo_self_stop(me);
            return Ok(());
        }

        let parsed = extract_settings(settings)?;

        let shared = &self.shared;
        let inner_ref = &inner;
        let t_inner = inner.clone();
        let body = move || capture_run(&t_inner, &parsed, &callback);
        let spawned =
            py.detach(move || spawn_worker(&shared.thread, inner_ref, "pcmflux-capture", body));
        let my_thread = match spawned {
            Some(id) => id,
            None => {
                return Err(pyo3::exceptions::PyRuntimeError::new_err(
                    "capture thread spawn failed",
                ))
            }
        };

        if let Ok(mut reg) = registry().lock() {
            reg.retain(|w| w.strong_count() > 0);
            reg.push(Arc::downgrade(&self.shared));
        }

        let mut state = inner.start_state.load(Ordering::Acquire);
        if state == ST_STARTING {
            py.detach(|| {
                for _ in 0..200 {
                    state = inner.start_state.load(Ordering::Acquire);
                    if state != ST_STARTING {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            });
        }
        if state == ST_FAILED {
            let shared2 = &self.shared;
            let inner2 = &inner;
            py.detach(move || join_failed_start(&shared2.thread, inner2, my_thread));
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "audio capture failed to start (see stderr for details)",
            ));
        }
        Ok(())
    }

    /// Stop audio capture, joining the capture thread.
    ///
    /// A re-entrant stop from one of the run's own threads — the delivery thread (where the
    /// Python callback executes) or the capture thread — only records a self-stop: a join
    /// from inside the run would cycle (the capture thread joins the delivery thread on
    /// teardown). It must not clobber an external stop already in effect, which has to win
    /// the join. Otherwise it takes the lifecycle lock and joins with the GIL released (via
    /// `py.detach`): joining the capture thread transitively joins the delivery thread,
    /// whose in-flight callback needs the GIL, so holding it would deadlock. The
    /// authoritative external stop is set INSIDE the lock immediately before the join, so
    /// it wins over any concurrent self-stop.
    fn stop_capture(&self, py: Python<'_>) {
        let inner = self.inner();
        let me = gettid();
        if inner.is_own_thread(me) {
            inner.request_self_stop(me);
            return;
        }
        let shared = &self.shared;
        py.detach(|| {
            let mut guard = shared.thread.lock().unwrap();
            if let Some(handle) = guard.take() {
                inner.request_external_stop();
                let _ = handle.join();
                inner.capture_tid.store(0, Ordering::Release);
            }
        });
    }

    /// Set the live Opus target bitrate (bits/s) via the atomic mirror; the capture
    /// loop applies it on the next frame, without a restart. Values are clamped to the
    /// valid Opus range so an out-of-range request can never wedge the encoder.
    fn update_audio_bitrate(&self, bps: i32) {
        let clamped = bps.clamp(6000, 510000);
        self.inner().opus_bitrate.store(clamped, Ordering::Relaxed);
    }

    /// True while a capture worker is running with no stop pending.
    #[getter]
    fn is_capturing(&self) -> bool {
        let inner = self.inner();
        inner.started_ok.load(Ordering::Acquire) && !inner.stop_pending()
    }
}

impl Drop for AudioCapture {
    /// Best-effort stop on GC/dealloc: the re-entrant case (running on the run's own
    /// delivery or capture thread) records a self-stop only (never clobbering a pending
    /// external stop); otherwise it takes the lifecycle lock and joins the capture thread
    /// with the GIL released, matching `stop_capture`.
    fn drop(&mut self) {
        let inner = &self.shared.inner;
        let me = gettid();
        if inner.is_own_thread(me) {
            inner.request_self_stop(me);
            return;
        }
        let shared = &self.shared;
        Python::attach(|py| {
            py.detach(|| {
                if let Ok(mut guard) = shared.thread.lock() {
                    if let Some(handle) = guard.take() {
                        inner.request_external_stop();
                        let _ = handle.join();
                        inner.capture_tid.store(0, Ordering::Release);
                    }
                }
            });
        });
    }
}

/// Python-facing mic-playback handle, symmetric to `AudioCapture`.
///
/// Same lifecycle protocol, but a PA playback stream instead of a record stream, and a
/// bounded drop-oldest queue fed by `write` / `write_red` instead of a Python callback.
/// Python never holds the PA handle — only the playback thread touches it — so a
/// close-versus-inflight-write use-after-free is structurally impossible here.
#[pyclass]
struct AudioPlayback {
    shared: Arc<PbShared>,
}

impl AudioPlayback {
    fn inner(&self) -> &Arc<Inner> {
        &self.shared.inner
    }
}

#[pymethods]
impl AudioPlayback {
    #[new]
    fn new() -> Self {
        AudioPlayback {
            shared: Arc::new(PbShared {
                inner: Arc::new(Inner::new()),
                thread: Mutex::new(None),
                queue: Arc::new(PlayQueue::new()),
                opus_dec: Mutex::new(None),
            }),
        }
    }

    /// Start (or restart) mic playback into the virtual sink. The playback mirror of
    /// `start_capture`.
    ///
    /// Same shape as capture: a re-entrant start from the playback thread just undoes a
    /// nested self-stop; the worker is spawned with the GIL released (the stop/clear ordering
    /// lives in `spawn_worker`); the handle is registered for the atexit sweep; and a ~2 s
    /// startup handshake surfaces a `FAILED` start after tearing down only the thread THIS
    /// call spawned (identity-checked, sparing a concurrent winner). Before spawning, it
    /// applies this run's byte bound + frame alignment to the queue (dropping any stale
    /// audio) and creates the Opus decoder up front, since the mic uplink is always Opus and
    /// `write` / `write_red` decode packets to PCM off the GIL for this same run.
    fn start(&self, py: Python<'_>, settings: &Bound<'_, PyAny>) -> PyResult<()> {
        let inner = self.inner().clone();

        let me = gettid();
        if inner.capture_tid.load(Ordering::Acquire) == me {
            inner.undo_self_stop(me);
            return Ok(());
        }

        let parsed = extract_pb_settings(settings)?;
        let frame_bytes = (parsed.channels.max(1) as usize) * 2;
        self.shared.queue.configure(parsed.max_buffer_bytes, frame_bytes);

        let decoder = OpusPlaybackDecoder::new(parsed.sample_rate, parsed.channels);
        if decoder.is_none() {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "failed to create Opus decoder for playback",
            ));
        }
        *self.shared.opus_dec.lock().unwrap_or_else(|e| e.into_inner()) = decoder;

        let shared = &self.shared;
        let inner_ref = &inner;
        let queue = self.shared.queue.clone();
        let t_inner = inner.clone();
        let body = move || playback_run(&t_inner, &parsed, &queue);
        let spawned =
            py.detach(move || spawn_worker(&shared.thread, inner_ref, "pcmflux-playback", body));
        let my_thread = match spawned {
            Some(id) => id,
            None => {
                return Err(pyo3::exceptions::PyRuntimeError::new_err(
                    "playback thread spawn failed",
                ))
            }
        };

        if let Ok(mut reg) = playback_registry().lock() {
            reg.retain(|w| w.strong_count() > 0);
            reg.push(Arc::downgrade(&self.shared));
        }

        let mut state = inner.start_state.load(Ordering::Acquire);
        if state == ST_STARTING {
            py.detach(|| {
                for _ in 0..200 {
                    state = inner.start_state.load(Ordering::Acquire);
                    if state != ST_STARTING {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            });
        }
        if state == ST_FAILED {
            let shared2 = &self.shared;
            let inner2 = &inner;
            py.detach(move || join_failed_start(&shared2.thread, inner2, my_thread));
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "audio playback failed to start (see stderr for details)",
            ));
        }
        Ok(())
    }

    /// Push one Opus mic packet for playback. The steady-state hot path.
    ///
    /// Gated on `worker_alive`: it raises once no playback thread services the queue (start
    /// failure, stop, or a PA outage the session loop could not reconnect through), so the
    /// caller's reopen-on-error path engages instead of the audio being swallowed silently.
    /// A reconnect in progress stays "alive" and keeps queueing. Otherwise it decodes to PCM
    /// and enqueues it with the GIL released — the decode touches no Python state, so dropping
    /// the GIL lets it run concurrently with the rest of the app — and a bad packet is dropped
    /// rather than corrupting the stream. It never blocks on PA (drop-oldest happens inside
    /// `PlayQueue::push`).
    fn write(&self, py: Python<'_>, data: &Bound<'_, PyBytes>) -> PyResult<()> {
        if !self.inner().worker_alive() {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "audio playback is not running (stream failed, stopped, or never started)",
            ));
        }
        let b = data.as_bytes();
        py.detach(|| {
            let mut dec = self.shared.opus_dec.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(d) = dec.as_mut() {
                if let Some(pcm) = d.decode_to_pcm(b) {
                    self.shared.queue.push(pcm);
                }
            }
        });
        Ok(())
    }

    /// Play one RFC 2198 RED mic frame from the WebRTC/UDP uplink, recovering across any
    /// packet loss on the way in. The lossy-transport counterpart of `write`.
    ///
    /// The payload is de-framed, loss-recovered, and decoded entirely off the GIL by
    /// `decode_red_into_queue` (see there for why RED exists and why the decode runs off the
    /// GIL). `primary_ts` is the packet's monotonic RTP timestamp; the redundant blocks carry
    /// offsets back from it. Gated on `worker_alive` exactly like `write`.
    fn write_red(&self, py: Python<'_>, data: &Bound<'_, PyBytes>, primary_ts: i64) -> PyResult<()> {
        if !self.inner().worker_alive() {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "audio playback is not running (stream failed, stopped, or never started)",
            ));
        }
        let b = data.as_bytes();
        py.detach(|| {
            let mut dec = self.shared.opus_dec.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(d) = dec.as_mut() {
                d.decode_red_into_queue(b, primary_ts, &self.shared.queue);
            }
        });
        Ok(())
    }

    /// Stop mic playback, joining the playback thread.
    ///
    /// A re-entrant stop from the playback thread records a self-stop only (it cannot
    /// self-join) and never clobbers an external stop already in effect. Otherwise it joins
    /// with the GIL released so a slow PA disconnect cannot stall the interpreter; `stop`
    /// returns only once the thread is joined and the sink is released.
    fn stop(&self, py: Python<'_>) {
        let inner = self.inner();
        let me = gettid();
        if inner.capture_tid.load(Ordering::Acquire) == me {
            inner.request_self_stop(me);
            return;
        }
        let shared = &self.shared;
        py.detach(|| {
            let mut guard = shared.thread.lock().unwrap();
            if let Some(handle) = guard.take() {
                inner.request_external_stop();
                let _ = handle.join();
                inner.capture_tid.store(0, Ordering::Release);
            }
        });
    }

    /// True while a playback worker is running with no stop pending.
    #[getter]
    fn is_running(&self) -> bool {
        let inner = self.inner();
        inner.started_ok.load(Ordering::Acquire) && !inner.stop_pending()
    }
}

impl Drop for AudioPlayback {
    /// Best-effort stop on GC/dealloc; symmetric to `AudioCapture::drop`.
    fn drop(&mut self) {
        let inner = &self.shared.inner;
        let me = gettid();
        if inner.capture_tid.load(Ordering::Acquire) == me {
            inner.request_self_stop(me);
            return;
        }
        let shared = &self.shared;
        Python::attach(|py| {
            py.detach(|| {
                if let Ok(mut guard) = shared.thread.lock() {
                    if let Some(handle) = guard.take() {
                        inner.request_external_stop();
                        let _ = handle.join();
                        inner.capture_tid.store(0, Ordering::Release);
                    }
                }
            });
        });
    }
}

/// atexit sweep: stop and join every live capture and playback before interpreter
/// shutdown, so no worker thread is still calling into Python during finalization.
///
/// Snapshots the two `Weak` registries into strong references (skipping any already
/// dropped), then for each takes the lifecycle lock, sets the external stop before joining,
/// and clears the tid — all with the GIL released. Registered on `atexit` from the module
/// init.
#[pyfunction]
fn _stop_all_captures(py: Python<'_>) {
    let snapshot: Vec<Arc<Shared>> = match registry().lock() {
        Ok(reg) => reg.iter().filter_map(|w| w.upgrade()).collect(),
        Err(_) => Vec::new(),
    };
    for shared in snapshot {
        py.detach(|| {
            if let Ok(mut guard) = shared.thread.lock() {
                if let Some(handle) = guard.take() {
                    shared.inner.request_external_stop();
                    let _ = handle.join();
                    shared.inner.capture_tid.store(0, Ordering::Release);
                }
            }
        });
    }
    let pb_snapshot: Vec<Arc<PbShared>> = match playback_registry().lock() {
        Ok(reg) => reg.iter().filter_map(|w| w.upgrade()).collect(),
        Err(_) => Vec::new(),
    };
    for shared in pb_snapshot {
        py.detach(|| {
            if let Ok(mut guard) = shared.thread.lock() {
                if let Some(handle) = guard.take() {
                    shared.inner.request_external_stop();
                    let _ = handle.join();
                    shared.inner.capture_tid.store(0, Ordering::Release);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Timestamps that wrap the 32-bit RTP range still decode in order — the
    /// post-rollover frame is not mistaken for an already-played duplicate.
    #[test]
    fn red_playback_timestamp_wraparound() {
        let f = opus_frames(3);
        let mut dec = OpusPlaybackDecoder::new(24000, 1).unwrap();
        let q = PlayQueue::new();
        q.configure(1 << 20, 2);
        let wrap = (u32::MAX as i64) - 100;
        dec.decode_red_into_queue(&build_red_payload(&[], &f[0]), wrap, &q);
        // One 20 ms step past the 32-bit rollover (mod 2^32): ts is numerically far
        // BELOW `wrap`, so a plain `ts > last` treats it as an already-played frame.
        let next = (wrap.wrapping_add(480)) & 0xFFFF_FFFF;
        dec.decode_red_into_queue(&build_red_payload(&[(480, &f[0])], &f[1]), next, &q);
        let mut out = Vec::new();
        q.drain_upto(1 << 20, &mut out);
        assert_eq!(out.len(), 2 * FRAME_PCM_BYTES,
            "frame after the 32-bit wrap was dropped as a duplicate");
    }

    /// The re-entrancy guard must recognize BOTH of a run's own threads: the
    /// capture worker AND the delivery thread — the Python callback executes on the
    /// delivery thread, so a stop/start it issues arrives with the delivery tid, and
    /// treating it as external would join into the capture→delivery join cycle and
    /// deadlock.
    #[test]
    fn reentrancy_guard_matches_delivery_thread() {
        let inner = Inner::new();
        let me = gettid();
        assert!(!inner.is_own_thread(me), "no run live: nothing should match");
        inner.deliver_tid.store(me, Ordering::Release);
        assert!(inner.is_own_thread(me), "delivery tid must short-circuit the guard");
        inner.deliver_tid.store(0, Ordering::Release);
        inner.capture_tid.store(me, Ordering::Release);
        assert!(inner.is_own_thread(me), "capture tid must still short-circuit the guard");
    }

    /// Encode 5.1 with a tone only on FC (input channel 2), decode with the same
    /// layout, and verify the energy comes back on that same channel — proving the
    /// `multiopus_layout` tables are self-consistent end to end.
    #[test]
    fn multiopus_surround_roundtrip() {
        let channels = 6usize;
        let frame = 480usize;
        let mut enc = PcmEncoder::new(48000, channels as i32, true, 256000).expect("encoder");
        let mut pcm = vec![0i16; frame * channels];
        for i in 0..frame {
            let v = (8000.0 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 48000.0).sin())
                as i16;
            pcm[i * channels + 2] = v;
        }
        let mut out = vec![0u8; 4 * MAX_OPUS_PACKET];
        let mut n = 0;
        for _ in 0..3 {
            n = enc.encode(&pcm, frame, &mut out).expect("encode");
        }
        assert!(n > 0, "surround encode produced no bytes");

        unsafe {
            let (streams, coupled, mapping) = multiopus_layout(channels as i32).unwrap();
            let mut err = 0;
            let dec = audiopus_sys::opus_multistream_decoder_create(
                48000,
                channels as i32,
                streams,
                coupled,
                mapping.as_ptr(),
                &mut err,
            );
            assert!(!dec.is_null() && err == 0, "decoder create failed: {err}");
            let mut decoded = vec![0i16; frame * channels];
            let got = audiopus_sys::opus_multistream_decode(
                dec,
                out.as_ptr(),
                n as i32,
                decoded.as_mut_ptr(),
                frame as i32,
                0,
            );
            audiopus_sys::opus_multistream_decoder_destroy(dec);
            assert_eq!(got, frame as i32, "decode length mismatch");
            let mut rms = vec![0f64; channels];
            for i in 0..frame {
                for (c, r) in rms.iter_mut().enumerate() {
                    let s = decoded[i * channels + c] as f64;
                    *r += s * s;
                }
            }
            let loudest = rms
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
                .unwrap()
                .0;
            assert_eq!(loudest, 2, "tone did not come back on FC: rms={rms:?}");
        }
    }

    /// `valid_opus_duration` accepts exactly the six legal Opus frame durations and
    /// rejects everything else (including the near-misses 3, 15, 25, 30 ms).
    #[test]
    fn opus_durations() {
        for ms in [2.5, 5.0, 10.0, 20.0, 40.0, 60.0] {
            assert!(valid_opus_duration(ms));
        }
        for ms in [0.0, 1.0, 3.0, 15.0, 25.0, 30.0, 50.0, 100.0] {
            assert!(!valid_opus_duration(ms));
        }
    }

    /// The samples-per-channel and PCM-byte arithmetic matches the wire cases:
    /// 48 kHz / 20 ms / stereo is 960 samples/ch and 3840 bytes, and 24 kHz / 10 ms / mono
    /// is 240 samples and 480 bytes.
    #[test]
    fn frame_geometry() {
        let fspc = (48000usize * 20) / 1000;
        assert_eq!(fspc, 960);
        assert_eq!(fspc * 2 * 2, 3840);
        let m = (24000usize * 10) / 1000;
        assert_eq!(m, 240);
        assert_eq!(m * 2, 480);
    }

    /// `red_distance == 0` produces exactly the 2-byte `[0x01, 0x00]` + opus framing,
    /// and the omit-header path returns the raw opus with no prefix at all.
    #[test]
    fn red_zero_is_byte_identical() {
        let opus = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x42];
        let hist: VecDeque<(Vec<u8>, u64)> = VecDeque::new();

        let legacy_expected = {
            let mut v = vec![0x01u8, 0x00];
            v.extend_from_slice(&opus);
            v
        };
        assert_eq!(build_ws_body(&opus, 4096, &hist, 0, true), legacy_expected);
        assert_eq!(build_ws_body(&opus, 4096, &hist, 0, false), opus);
    }

    /// `red_distance == 2`: parse the emitted body back (`n_red` 4-byte headers, the
    /// 1-byte primary header, block datas split by their lengths) and assert the primary and
    /// the two redundant blocks round-trip with the expected oldest-first offsets 1920 & 960.
    #[test]
    fn red_two_roundtrips() {
        let f_n2 = vec![0xA0, 0xA1, 0xA2];
        let f_n1 = vec![0xB0, 0xB1, 0xB2, 0xB3];
        let primary = vec![0xC0, 0xC1];
        let mut hist: VecDeque<(Vec<u8>, u64)> = VecDeque::new();
        hist.push_back((f_n2.clone(), 0));
        hist.push_back((f_n1.clone(), 960));

        let body = build_ws_body(&primary, 1920, &hist, 2, true);
        assert_eq!(body[0], 0x01);
        let n_red = body[1] as usize;
        assert_eq!(n_red, 2);
        assert_eq!(u32::from_be_bytes([body[2], body[3], body[4], body[5]]), 1920);

        let mut idx = 6;
        let mut offsets = Vec::new();
        let mut lens = Vec::new();
        for _ in 0..n_red {
            assert_eq!(body[idx] & 0x80, 0x80, "redundant header F bit must be set");
            let word = ((body[idx + 1] as u32) << 16)
                | ((body[idx + 2] as u32) << 8)
                | (body[idx + 3] as u32);
            offsets.push((word >> 10) & 0x3FFF);
            lens.push((word & 0x3FF) as usize);
            idx += 4;
        }
        assert_eq!(body[idx] & 0x80, 0x00, "primary header F bit must be clear");
        idx += 1;

        assert_eq!(offsets, vec![1920, 960]);
        assert_eq!(lens, vec![f_n2.len(), f_n1.len()]);

        let b0 = &body[idx..idx + lens[0]];
        idx += lens[0];
        let b1 = &body[idx..idx + lens[1]];
        idx += lens[1];
        let prim = &body[idx..];
        assert_eq!(b0, &f_n2[..]);
        assert_eq!(b1, &f_n1[..]);
        assert_eq!(prim, &primary[..]);
    }

    /// Test helper: build an RFC 2198 RED payload — redundant blocks oldest-first,
    /// each with a 14-bit timestamp offset back from the primary, then the primary. Mirrors
    /// the wire format `decode_red_into_queue` parses (the mic-uplink counterpart of
    /// `build_ws_body`).
    fn build_red_payload(reds: &[(u64, &[u8])], primary: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        for (off, blk) in reds {
            let field = (((*off as u32) & 0x3FFF) << 10) | ((blk.len() as u32) & 0x3FF);
            v.push(0x80 | (RED_BLOCK_PT & 0x7F));
            v.push((field >> 16) as u8);
            v.push((field >> 8) as u8);
            v.push(field as u8);
        }
        v.push(RED_BLOCK_PT & 0x7F);
        for (_, blk) in reds {
            v.extend_from_slice(blk);
        }
        v.extend_from_slice(primary);
        v
    }

    /// Test helper: `n` distinct valid 20 ms mono Opus packets at 24 kHz
    /// (480 samples/frame).
    fn opus_frames(n: usize) -> Vec<Vec<u8>> {
        let mut enc = opus::Encoder::new(24000, Channels::Mono, Application::LowDelay).unwrap();
        (0..n)
            .map(|s| {
                let pcm: Vec<i16> = (0..480)
                    .map(|k| ((k * (s as i32 + 1)) % 4000 - 2000) as i16)
                    .collect();
                let mut out = vec![0u8; 4000];
                let len = enc.encode(&pcm, &mut out).unwrap();
                out.truncate(len);
                out
            })
            .collect()
    }

    /// Test constant: each decoded 20 ms mono frame is 480 samples * 2 bytes.
    const FRAME_PCM_BYTES: usize = 480 * 2;

    /// A dropped middle packet is recovered from the next packet's redundancy: the
    /// redundant copy of the gap frame is decoded, while the redundant copy of an
    /// already-played frame is not (timestamp dedup). Exercises the off-GIL RED playback path
    /// end to end — anchor on packet 1 (ts 1000), drop packet 2 (ts 1480), then packet 3
    /// (ts 1960) carries redundant copies of 1000 and 1480, so the output is exactly the
    /// three frames 1000 + 1480 + 1960.
    #[test]
    fn red_playback_recovers_lost_frame() {
        let f = opus_frames(3);
        let mut dec = OpusPlaybackDecoder::new(24000, 1).unwrap();
        let q = PlayQueue::new();
        q.configure(1 << 20, 2);

        dec.decode_red_into_queue(&build_red_payload(&[], &f[0]), 1000, &q);
        let pkt3 = build_red_payload(&[(960, &f[0]), (480, &f[1])], &f[2]);
        dec.decode_red_into_queue(&pkt3, 1960, &q);

        let mut out = Vec::new();
        q.drain_upto(1 << 20, &mut out);
        assert_eq!(out.len(), 3 * FRAME_PCM_BYTES);
        assert_eq!(dec.last_ts, Some(1960));
    }

    /// With no loss, redundancy is pure overhead: every frame decodes exactly once and
    /// the redundant copies are dropped, so three packets yield exactly three frames.
    #[test]
    fn red_playback_no_double_decode() {
        let f = opus_frames(3);
        let mut dec = OpusPlaybackDecoder::new(24000, 1).unwrap();
        let q = PlayQueue::new();
        q.configure(1 << 20, 2);

        dec.decode_red_into_queue(&build_red_payload(&[], &f[0]), 1000, &q);
        dec.decode_red_into_queue(&build_red_payload(&[(480, &f[0])], &f[1]), 1480, &q);
        dec.decode_red_into_queue(&build_red_payload(&[(480, &f[1])], &f[2]), 1960, &q);

        let mut out = Vec::new();
        q.drain_upto(1 << 20, &mut out);
        assert_eq!(out.len(), 3 * FRAME_PCM_BYTES);
        assert_eq!(dec.last_ts, Some(1960));
    }

    /// A too-old block (offset > 16383, overflowing the 14-bit field) and an oversize
    /// block (len > 1023, overflowing the 10-bit field) are both skipped, so `n_red` counts
    /// only the one in-range block that fit the RFC 2198 fields.
    #[test]
    fn red_skips_oversize_and_too_old() {
        let too_old = vec![0x11u8; 3];
        let oversize = vec![0x22u8; 1100];
        let good = vec![0x33u8; 5];
        let primary = vec![0x44u8; 2];
        let primary_pts = 100_000u64;

        let mut hist: VecDeque<(Vec<u8>, u64)> = VecDeque::new();
        hist.push_back((too_old, primary_pts - 20_000));
        hist.push_back((oversize, primary_pts - 1920));
        hist.push_back((good.clone(), primary_pts - 960));

        let body = build_ws_body(&primary, primary_pts, &hist, 4, true);
        assert_eq!(body[0], 0x01);
        assert_eq!(body[1], 1, "only the in-range block survives");
        assert_eq!(u32::from_be_bytes([body[2], body[3], body[4], body[5]]), primary_pts as u32);

        let word = ((body[7] as u32) << 16) | ((body[8] as u32) << 8) | (body[9] as u32);
        assert_eq!((word >> 10) & 0x3FFF, 960);
        let len = (word & 0x3FF) as usize;
        assert_eq!(len, good.len());
        assert_eq!(body[10] & 0x80, 0x00, "primary header follows the one redundant header");
        assert_eq!(&body[11..11 + len], &good[..]);
        assert_eq!(&body[11 + len..], &primary[..]);
    }

    /// `red_distance > 0` with no usable history (e.g. the first frame after a
    /// (re)start) collapses to exactly the 2-byte `[0x01, 0x00]` + opus framing — the same
    /// bytes as `n_red == 0`, not a primary-only RED header. The client's `n_red == 0` path
    /// strips exactly 2 bytes, so emitting a lone primary-only RED header here would be
    /// mis-stripped and corrupt the frame; the body is asserted to be exactly those 2 bytes
    /// plus the opus.
    #[test]
    fn red_empty_history_collapses_to_bare_header() {
        let opus = vec![0x77u8; 4];
        let hist: VecDeque<(Vec<u8>, u64)> = VecDeque::new();
        let body = build_ws_body(&opus, 960, &hist, 2, true);
        assert_eq!(body[0], 0x01);
        assert_eq!(body[1], 0x00);
        assert_eq!(&body[2..], &opus[..]);
        assert_eq!(body.len(), 2 + opus.len());
    }

    /// The all-zero comparison behind the silence gate: an all-zero buffer reads as
    /// silent, and a single non-zero sample makes it non-silent.
    #[test]
    fn silence_detection() {
        let silent = vec![0i16; 960 * 2];
        assert!(silent.iter().all(|&s| s == 0));
        let mut not = silent.clone();
        not[123] = 7;
        assert!(!not.iter().all(|&s| s == 0));
    }

    /// Deterministic lost-stop probe against the real `stop_state` protocol.
    ///
    /// A stand-in "capture thread" hammers `request_self_stop` + `undo_self_stop` (the
    /// re-entrant callback pattern) while another thread issues `request_external_stop` and
    /// waits for the loop's `stop_pending()` to observe it. The external stop must never be
    /// cleared by a self-start, so the thread always observes `STOP_EXTERNAL` and the join
    /// returns (no hang). The bounded spin count is the in-test watchdog; 5000 iterations
    /// shake out the race.
    #[test]
    fn external_stop_never_lost_to_self_restart() {
        use std::sync::Arc;
        let me: i64 = 987654;
        for _ in 0..5000 {
            let inner = Arc::new(Inner::new());
            let inner_c = inner.clone();
            let observed = Arc::new(AtomicBool::new(false));
            let observed_c = observed.clone();
            let h = std::thread::spawn(move || {
                let mut spins: u64 = 0;
                loop {
                    inner_c.request_self_stop(me);
                    inner_c.undo_self_stop(me);
                    if inner_c.stop_pending()
                        && inner_c.stop_state.load(Ordering::Acquire) == STOP_EXTERNAL
                    {
                        observed_c.store(true, Ordering::Release);
                        return;
                    }
                    spins += 1;
                    assert!(spins < 50_000_000, "external stop was lost (would hang the join)");
                }
            });
            inner.request_external_stop();
            h.join().unwrap();
            assert!(observed.load(Ordering::Acquire));
            assert_eq!(inner.stop_state.load(Ordering::Acquire), STOP_EXTERNAL);
            assert!(inner.stop_pending());
        }
    }

    /// Pushing past the byte bound drops the OLDEST bytes, keeping the newest window:
    /// an 8-byte bound fed 12 bytes retains the last 8, and a second drain comes back empty.
    #[test]
    fn playqueue_drop_oldest_keeps_newest() {
        let q = PlayQueue::new();
        q.configure(8, 2);
        q.push(&[1, 2, 3, 4, 5, 6]);
        q.push(&[7, 8, 9, 10, 11, 12]);
        let mut out = Vec::new();
        q.drain_upto(100, &mut out);
        assert_eq!(out, vec![5, 6, 7, 8, 9, 10, 11, 12]);
        q.drain_upto(100, &mut out);
        assert!(out.is_empty());
    }

    /// `drain_upto` clamps to the requested count AND floors to a whole frame, so a PA
    /// write never gets a partial frame: with 4-byte frames, a request of 7 yields 4 bytes,
    /// the next drain yields the next 4, and the trailing 2 bytes are withheld as a partial
    /// frame.
    #[test]
    fn playqueue_drain_is_frame_aligned() {
        let q = PlayQueue::new();
        q.configure(1000, 4);
        q.push(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let mut out = Vec::new();
        q.drain_upto(7, &mut out);
        assert_eq!(out, vec![0, 1, 2, 3]);
        q.drain_upto(100, &mut out);
        assert_eq!(out, vec![4, 5, 6, 7]);
        q.drain_upto(100, &mut out);
        assert!(out.is_empty());
    }

    /// `configure()` applies the new bounds and drops stale audio from a prior run, so
    /// bytes queued before it are gone afterward.
    #[test]
    fn playqueue_configure_resets() {
        let q = PlayQueue::new();
        q.push(&[1, 2, 3, 4]);
        q.configure(96000, 2);
        let mut out = Vec::new();
        q.drain_upto(100, &mut out);
        assert!(out.is_empty(), "configure must clear stale audio");
    }

    /// A byte bound that is not a whole-frame multiple must not let overflow drops
    /// split a sample frame — a mid-frame trim would phase-shift every later drain
    /// into interleaved garbage. The bound floors to whole frames and drops are made
    /// in whole frames.
    #[test]
    fn playqueue_misaligned_bound_never_splits_frames() {
        let q = PlayQueue::new();
        q.configure(9, 4); // floors the bound to 8 = two 4-byte frames
        q.push(&[0, 1, 2, 3]);
        q.push(&[4, 5, 6, 7]);
        q.push(&[8, 9, 10, 11]); // 12 queued, bound 8: exactly the oldest frame goes
        let mut out = Vec::new();
        q.drain_upto(100, &mut out);
        assert_eq!(out, vec![4, 5, 6, 7, 8, 9, 10, 11]);
    }

    /// `worker_alive` (which gates `AudioPlayback::write`) tracks the lifecycle: it is
    /// false before any worker, true through the startup handshake and the healthy run, and
    /// goes false the moment the hot loop's error exit clears `started_ok` — even with no stop
    /// pending and `start_state` still `RUNNING` (the silent mid-run death case). A pending
    /// external stop also reads as not-alive.
    #[test]
    fn worker_alive_tracks_loop_error_exit() {
        let inner = Inner::new();
        assert!(!inner.worker_alive(), "no worker yet");

        inner.start_state.store(ST_STARTING, Ordering::Release);
        assert!(inner.worker_alive(), "startup handshake counts as alive");

        inner.started_ok.store(true, Ordering::Release);
        inner.start_state.store(ST_RUNNING, Ordering::Release);
        assert!(inner.worker_alive());

        inner.started_ok.store(false, Ordering::Release);
        assert_eq!(inner.stop_state.load(Ordering::Acquire), STOP_NONE);
        assert!(!inner.worker_alive(), "dead loop must be observable");

        inner.started_ok.store(true, Ordering::Release);
        inner.request_external_stop();
        assert!(!inner.worker_alive());
    }

    /// A LOSING start's failed-start cleanup must never tear down a WINNING start's
    /// freshly spawned thread.
    ///
    /// Drives the bad interleaving directly: loser L's worker fails, winner W takes the slot
    /// over (joins dead L, spawns live W) BEFORE L runs its `ST_FAILED` cleanup. That late
    /// `join_failed_start` must be an identity-mismatch no-op, leaving W running and still
    /// tear-down-able through the ordinary external stop path.
    #[test]
    fn failed_start_cleanup_spares_concurrent_winner() {
        let inner = Arc::new(Inner::new());
        let slot: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

        let li = inner.clone();
        let l_id = spawn_worker(&slot, &inner, "loser", move || {
            li.started_ok.store(false, Ordering::Release);
            li.start_state.store(ST_FAILED, Ordering::Release);
        })
        .unwrap();
        while inner.start_state.load(Ordering::Acquire) != ST_FAILED {
            std::thread::yield_now();
        }

        let wi = inner.clone();
        let w_id = spawn_worker(&slot, &inner, "winner", move || {
            wi.started_ok.store(true, Ordering::Release);
            wi.start_state.store(ST_RUNNING, Ordering::Release);
            while !wi.stop_pending() {
                std::thread::sleep(Duration::from_millis(1));
            }
            wi.started_ok.store(false, Ordering::Release);
        })
        .unwrap();
        assert_ne!(l_id, w_id);
        while inner.start_state.load(Ordering::Acquire) != ST_RUNNING {
            std::thread::yield_now();
        }

        join_failed_start(&slot, &inner, l_id);
        assert!(slot.lock().unwrap().is_some(), "winner's handle must survive");
        assert_eq!(inner.stop_state.load(Ordering::Acquire), STOP_NONE);
        assert!(inner.worker_alive(), "winner must still be running");

        {
            let mut g = slot.lock().unwrap();
            let h = g.take().expect("winner handle present");
            inner.request_external_stop();
            h.join().unwrap();
        }
        assert!(!inner.worker_alive());
    }

    /// Two threads race the full start sequence (`spawn_worker` + handshake poll +
    /// conditional failed-start cleanup), 200 times.
    ///
    /// `spawn_worker` joins the prior worker before spawning, so the bodies run in spawn
    /// order: the first fails, the second serves until stopped. Whatever the interleaving,
    /// the second run must end `RUNNING` with no stray stop — a loser cleanup that killed the
    /// winner would leave an empty slot and a `STOP_EXTERNAL` behind.
    #[test]
    fn concurrent_start_loser_never_kills_winner() {
        for _ in 0..200 {
            let inner = Arc::new(Inner::new());
            let slot = Arc::new(Mutex::new(None::<JoinHandle<()>>));
            let runs = Arc::new(AtomicUsize::new(0));

            let starter = |name: &'static str| {
                let inner = inner.clone();
                let slot = slot.clone();
                let runs = runs.clone();
                std::thread::spawn(move || {
                    let bi = inner.clone();
                    let bruns = runs.clone();
                    let id = spawn_worker(&slot, &inner, name, move || {
                        if bruns.fetch_add(1, Ordering::AcqRel) == 0 {
                            bi.started_ok.store(false, Ordering::Release);
                            bi.start_state.store(ST_FAILED, Ordering::Release);
                        } else {
                            bi.started_ok.store(true, Ordering::Release);
                            bi.start_state.store(ST_RUNNING, Ordering::Release);
                            let mut spins: u64 = 0;
                            while !bi.stop_pending() {
                                std::thread::sleep(Duration::from_millis(1));
                                spins += 1;
                                assert!(spins < 10_000, "external stop was lost");
                            }
                            bi.started_ok.store(false, Ordering::Release);
                        }
                    })
                    .unwrap();
                    let mut state = inner.start_state.load(Ordering::Acquire);
                    let mut tries = 0;
                    while state == ST_STARTING && tries < 2000 {
                        std::thread::sleep(Duration::from_millis(1));
                        state = inner.start_state.load(Ordering::Acquire);
                        tries += 1;
                    }
                    if state == ST_FAILED {
                        join_failed_start(&slot, &inner, id);
                    }
                })
            };
            let a = starter("start-a");
            let b = starter("start-b");
            a.join().unwrap();
            b.join().unwrap();

            assert_eq!(runs.load(Ordering::Acquire), 2);
            assert!(slot.lock().unwrap().is_some(), "winner's thread must survive");
            assert_eq!(inner.start_state.load(Ordering::Acquire), ST_RUNNING);
            assert!(inner.worker_alive(), "winner must still be alive after both starts");

            {
                let mut g = slot.lock().unwrap();
                let h = g.take().expect("winner handle present");
                inner.request_external_stop();
                h.join().unwrap();
            }
            assert!(!inner.worker_alive());
        }
    }

    /// The in-place `write_ws_prefix_into` + appended primary is byte-identical to the
    /// copy-based `build_ws_body` reference across the full matrix — empty vs mixed history
    /// (including one block aged out of the 14-bit offset at high pts and one oversized block
    /// that is always skipped), every `red_distance`, header on/off, and several pts values.
    #[test]
    fn prefix_writer_matches_build_ws_body() {
        let primary: Vec<u8> = (0u8..200).collect();
        let mut mixed: VecDeque<(Vec<u8>, u64)> = VecDeque::new();
        mixed.push_back((vec![1u8; 100], 0));
        mixed.push_back((vec![2u8; RED_MAX_LEN + 1], 960));
        mixed.push_back((vec![3u8; 50], 1440));
        mixed.push_back((vec![4u8; 900], 1900));
        for hist in [VecDeque::new(), mixed] {
            for red in [0usize, 1, 2, 4] {
                for hdr in [true, false] {
                    for pts in [960u64, 3840, 20000] {
                        let reference = build_ws_body(&primary, pts, &hist, red, hdr);
                        let mut buf = vec![0u8; RED_PREFIX_MAX + MAX_OPUS_PACKET];
                        let prefix = write_ws_prefix_into(&mut buf, pts, &hist, red, hdr);
                        buf[prefix..prefix + primary.len()].copy_from_slice(&primary);
                        buf.truncate(prefix + primary.len());
                        assert_eq!(buf, reference, "red={red} hdr={hdr} pts={pts}");
                    }
                }
            }
        }
    }

    /// A truncated buffer returned to the pool comes back as the same allocation
    /// (recycled) restored to full `buf_size` length, and an undersized foreign buffer is
    /// rejected rather than pooled.
    #[test]
    fn buffer_pool_recycles_and_restores_length() {
        let pool = BufferPool::new(64);
        let mut a = pool.take();
        assert_eq!(a.len(), 64);
        let ptr = a.as_ptr() as usize;
        a.truncate(7);
        pool.put(a);
        let b = pool.take();
        assert_eq!(b.as_ptr() as usize, ptr, "buffer must be recycled");
        assert_eq!(b.len(), 64, "length must be restored");
        pool.put(Vec::new());
        assert_eq!(pool.take().len(), 64);
    }

    /// Dropping an `AudioFrame` whose `pool` is set returns its buffer to that pool,
    /// so the next `take` hands back the same allocation.
    #[test]
    fn audio_frame_drop_refills_pool() {
        let pool = Arc::new(BufferPool::new(32));
        let buf = pool.take();
        let ptr = buf.as_ptr() as usize;
        drop(AudioFrame { data: buf, pts: 0, pool: Some(Arc::clone(&pool)) });
        let recycled = pool.take();
        assert_eq!(recycled.as_ptr() as usize, ptr);
    }

    /// Micro-benchmark (not a correctness test) isolating the emit-path assembly cost:
    /// the copy-based `build_ws_body` + per-frame alloc versus the pooled in-place prefix,
    /// with the encoder stubbed to a memcpy.
    ///
    /// `#[ignore]`d by default; run with
    /// `cargo test --release bench_emit_assembly -- --ignored --nocapture`. The pooled arm
    /// circulates buffers at the delivery-ring depth (>8 in flight) so refill batching engages
    /// as it does live — several returns per drain, not one.
    #[test]
    #[ignore]
    fn bench_emit_assembly() {
        use std::hint::black_box;
        use std::time::Instant;

        const ITERS: u32 = 500_000;
        const PAYLOAD: usize = 200;
        let src = vec![0xA5u8; PAYLOAD];
        let mut hist: VecDeque<(Vec<u8>, u64)> = VecDeque::new();
        hist.push_back((vec![6u8; 180], 0));
        hist.push_back((vec![7u8; 180], 480));

        for (red, label) in [(0usize, "red=0"), (2usize, "red=2")] {
            let old = Instant::now();
            let mut out = vec![0u8; MAX_OPUS_PACKET];
            for i in 0..ITERS {
                out[..PAYLOAD].copy_from_slice(&src);
                let data = build_ws_body(&out[..PAYLOAD], 960 + i as u64, &hist, red, true);
                black_box(&data);
            }
            let old_ns = old.elapsed().as_nanos() / ITERS as u128;

            let pool = Arc::new(BufferPool::new(RED_PREFIX_MAX + MAX_OPUS_PACKET));
            let mut taker = PoolTaker::new(Arc::clone(&pool));
            let mut inflight: VecDeque<Vec<u8>> = VecDeque::new();
            let new = Instant::now();
            for i in 0..ITERS {
                let mut data = taker.take();
                let prefix =
                    write_ws_prefix_into(&mut data, 960 + i as u64, &hist, red, true);
                data[prefix..prefix + PAYLOAD].copy_from_slice(&src);
                data.truncate(prefix + PAYLOAD);
                black_box(&data);
                inflight.push_back(data);
                if inflight.len() > 8 {
                    pool.put(inflight.pop_front().unwrap());
                }
            }
            let new_ns = new.elapsed().as_nanos() / ITERS as u128;
            println!("{label}: old {old_ns} ns/frame -> pooled in-place {new_ns} ns/frame");
        }
    }
}

/// PyO3 module init: register the capture/playback classes and the atexit sweep.
///
/// Exposes `AudioCapture`, `AudioCaptureSettings`, `AudioFrame`, `AudioPlayback`, and
/// `AudioPlaybackSettings`, then registers `_stop_all_captures` on Python's `atexit` so no
/// still-running capture thread is calling into Python during interpreter finalization.
#[pymodule]
fn pcmflux(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<AudioCapture>()?;
    m.add_class::<AudioCaptureSettings>()?;
    m.add_class::<AudioFrame>()?;
    m.add_class::<AudioPlayback>()?;
    m.add_class::<AudioPlaybackSettings>()?;
    m.add_function(wrap_pyfunction!(_stop_all_captures, m)?)?;
    if let Ok(atexit) = m.py().import("atexit") {
        let _ = atexit.call_method1("register", (m.getattr("_stop_all_captures")?,));
    }
    Ok(())
}

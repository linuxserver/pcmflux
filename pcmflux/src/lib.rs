/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// pcmflux: PulseAudio -> Opus audio capture as a pure-Rust PyO3 extension.
//
// Concurrency design (the invariants below are load-bearing):
//   - A lifecycle mutex serializes joining/reassigning the capture thread.
//   - A single stop_state atomic is the one source of truth (0 = running, -1 =
//     external stop, a positive value = self-stop by that capture thread's tid). The
//     external -1 is stored INSIDE that lock immediately before join, so a stop can
//     never be lost between observing a live thread and asking it to stop. A
//     re-entrant self-start undoes only its own self-stop via one compare-exchange,
//     so a racing external stop is never clobbered.
//   - The PulseAudio mainloop is pumped with a bounded ~20ms timeout, so a stop is
//     observed within ~20ms even if the audio source delivers no data (is wedged).
//   - The GIL is released around join, because the capture thread's final callback
//     needs the GIL; holding it while joining would deadlock.
//   - A callback may itself call stop/start from the capture thread; that re-entrant
//     case is detected via the capture thread's OS tid and short-circuits without
//     self-joining.

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
use pulse::stream::{FlagSet as StreamFlags, PeekResult, Stream};
use pulse::time::MicroSeconds;

use opus::{Application, Channels};

// Non-panicking logging. Rust's std println!/eprintln! PANIC on any write error
// other than EBADF (e.g. EPIPE when a piped stdout reader exits, as in `... | head`).
// A panic here would otherwise unwind out of the capture thread / callback path; we
// swallow write errors instead while preserving the exact message text and stream.
macro_rules! plog {
    ($($arg:tt)*) => {{
        use std::io::Write;
        let _ = writeln!(std::io::stdout().lock(), $($arg)*);
    }};
}
macro_rules! elog {
    ($($arg:tt)*) => {{
        use std::io::Write;
        let _ = writeln!(std::io::stderr().lock(), $($arg)*);
    }};
}

#[inline]
fn gettid() -> i64 {
    // OS thread id; stored as an atomic so a re-entrant stop/start invoked from
    // inside the callback can detect it's on the capture thread and avoid self-joining.
    unsafe { libc::syscall(libc::SYS_gettid) as i64 }
}

// start_state values, published by the capture thread for the startup handshake.
const ST_STARTING: u8 = 1;
const ST_RUNNING: u8 = 2;
const ST_FAILED: u8 = 3;

// stop_state sentinels; any positive value is the tid of a self-stopping capture thread.
const STOP_NONE: i64 = 0;
const STOP_EXTERNAL: i64 = -1;

const PUMP_TIMEOUT_US: u64 = 20 * 1000;
const MAX_OPUS_PACKET: usize = 4000;

// RFC 2198 RED audio redundancy on the WS path (recover NetEQ concealment under
// loss on future unreliable transports; a null benefit over reliable TCP by design).
// The timestamp offset is a 14-bit field and the block length a 10-bit field, so a
// redundant history block that overflows either is dropped. blockPT is nominal on
// the WS path -- the client decodes only the primary block as Opus.
const RED_MAX_OFFSET: u64 = 16383;
const RED_MAX_LEN: usize = 1023;
const RED_BLOCK_PT: u8 = 0;
const RED_MAX_DISTANCE: i32 = 4;

// Assemble the WS audio frame body from the encoded primary packet and its
// redundant history per the shared RFC 2198 RED contract. `history` is ordered
// oldest-first (front = oldest, largest timestamp offset). Redundant blocks whose
// 48 kHz sample offset or byte length overflow the RFC 2198 fields are skipped, so
// the emitted count reflects only what fit. With `red_distance == 0` the output is
// byte-identical to the legacy [0x01,0x00]+opus frame; with the header omitted there
// is no [0x01,n] prefix at all (the header carries the count), so the raw opus is
// returned unchanged -- the pre-RED omit-header behavior.
// Reference implementation of the frame layout: the runtime path assembles the same
// bytes in place via write_ws_prefix_into (equality is unit-tested against this).
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
        out.push(0x01); // audio chunk tag
        out.push(0x00); // n_red == 0 (legacy reserved byte)
        out.extend_from_slice(primary);
        return out;
    }
    // Redundant blocks, oldest-first, capped at red_distance and filtered to the
    // RFC 2198 field ranges.
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
        // No usable redundancy yet (first frame after a (re)start, or all history out of
        // range): fall back to the legacy 2-byte framing. On this wire n_red==0 must mean
        // exactly [0x01,0x00]+opus, so a primary-only RED header here would be mis-stripped
        // by the client's legacy path and corrupt the frame.
        let mut out = Vec::with_capacity(2 + primary.len());
        out.push(0x01);
        out.push(0x00);
        out.extend_from_slice(primary);
        return out;
    }
    let redundant_bytes: usize = blocks.iter().map(|(d, _)| d.len()).sum();
    let mut out = Vec::with_capacity(6 + 4 * blocks.len() + 1 + redundant_bytes + primary.len());
    out.push(0x01); // audio chunk tag
    out.push(blocks.len() as u8); // n_red (always >= 1 here)
    // Primary timestamp (low 32 bits, big-endian): lets the client order and dedup
    // recovered frames against what it has already played.
    out.extend_from_slice(&(primary_pts as u32).to_be_bytes());
    // Redundant block headers (4 bytes): F|PT, then (offset14 << 10 | len10) big-endian.
    for (data, offset) in &blocks {
        out.push(0x80 | (RED_BLOCK_PT & 0x7F)); // F bit set: another block follows
        let word = ((offset & 0x3FFF) << 10) | (data.len() as u32 & 0x3FF);
        out.push((word >> 16) as u8);
        out.push((word >> 8) as u8);
        out.push(word as u8);
    }
    // Primary (final) block header: 1 byte, F bit clear.
    out.push(RED_BLOCK_PT & 0x7F);
    // Block data, same order: redundant oldest-first, then primary.
    for (data, _) in &blocks {
        out.extend_from_slice(data);
    }
    out.extend_from_slice(primary);
    out
}

// Worst-case build_ws_body prefix: tag + n_red + 4-byte pts + RED_MAX_DISTANCE 4-byte
// block headers + the 1-byte primary header + the redundant block data itself.
const RED_PREFIX_MAX: usize =
    6 + 4 * RED_MAX_DISTANCE as usize + 1 + RED_MAX_DISTANCE as usize * RED_MAX_LEN;

// The same framing as build_ws_body, but writing only the PREFIX (everything before
// the primary payload) into `buf`, so the encoder can serialize the primary directly
// into its final buffer. Returns the prefix length; `buf` must hold at least
// RED_PREFIX_MAX bytes when emit_header && red_distance > 0 (2 otherwise).
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
        // Usable blocks (RFC 2198 field-range filtered) by history index; the
        // distance is clamped at ingest, so a fixed array avoids any allocation.
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
            buf[0] = 0x01; // audio chunk tag
            buf[1] = n as u8; // n_red (always >= 1 here)
            // Primary timestamp (low 32 bits, big-endian) for client-side RED recovery.
            buf[2..6].copy_from_slice(&(primary_pts as u32).to_be_bytes());
            let mut i = 6;
            for &k in &idx[..n] {
                let (data, pts) = &history[k];
                let offset = primary_pts.saturating_sub(*pts) as u32;
                buf[i] = 0x80 | (RED_BLOCK_PT & 0x7F); // F bit set: another block follows
                let word = ((offset & 0x3FFF) << 10) | (data.len() as u32 & 0x3FF);
                buf[i + 1] = (word >> 16) as u8;
                buf[i + 2] = (word >> 8) as u8;
                buf[i + 3] = word as u8;
                i += 4;
            }
            buf[i] = RED_BLOCK_PT & 0x7F; // primary (final) block header, F bit clear
            i += 1;
            for &k in &idx[..n] {
                let data = &history[k].0;
                buf[i..i + data.len()].copy_from_slice(data);
                i += data.len();
            }
            return i;
        }
    }
    // Legacy 2-byte framing (red_distance == 0, or no usable redundancy yet).
    buf[0] = 0x01;
    buf[1] = 0x00;
    2
}

/// Capture/encode settings, snapshotted from the Python `AudioCaptureSettings`.
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

fn valid_opus_duration(ms: f64) -> bool {
    // Opus frame sizes: 2.5, 5, 10, 20, 40, 60 ms. Compare in tenths to accept the
    // fractional 2.5 without float-equality pitfalls.
    matches!((ms * 10.0).round() as i64, 25 | 50 | 100 | 200 | 400 | 600)
}

// device_name: str | bytes | None  ("" => default). Shared by capture + playback.
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
        // Redundant Opus copies per frame; clamped to the RFC 2198 history depth.
        red_distance: s.getattr("red_distance")?.extract::<i32>()?.clamp(0, RED_MAX_DISTANCE),
    })
}

/// Playback settings, snapshotted from the Python `AudioPlaybackSettings`.
#[derive(Clone)]
struct PbSettings {
    device_name: Option<String>,
    sample_rate: u32,
    channels: i32,
    latency_ms: i32,
    max_buffer_bytes: usize,
    debug_logging: bool,
}

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

// ============================================================================
// AudioCaptureSettings: capture/encode configuration read by start_capture.
// Declared `dict` so callers may stash extra attributes; the fields below are
// read by attribute name (see extract_settings).
// ============================================================================
#[pyclass(dict)]
struct AudioCaptureSettings {
    #[pyo3(get, set)]
    device_name: Py<PyAny>, // str | bytes | None
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
    // Informational parity field with pixelflux: frames always own their buffers
    // (freed/recycled on last reference), so holding a frame past the callback is safe.
    #[pyo3(get, set)]
    deferred_free: bool,
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
            deferred_free: false,
        }
    }
}

// ============================================================================
// AudioPlaybackSettings: mic-playback configuration read by AudioPlayback.start.
// Defaults match the client mic wire (s16le / mono / 24 kHz); max_buffer_bytes is
// the single bytes bound for the drop-oldest queue (~2s @24k mono s16).
// ============================================================================
#[pyclass]
struct AudioPlaybackSettings {
    #[pyo3(get, set)]
    device_name: Py<PyAny>, // str | bytes | None
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

// ============================================================================
// AudioFrame: zero-copy buffer-protocol result type (owns the Vec; when Python
// frees it, a pooled buffer is recycled back to the capture thread).
// ============================================================================
#[pyclass]
struct AudioFrame {
    data: Vec<u8>,
    pts: u64,
    // Set when the buffer came from a capture's BufferPool; recycled on drop.
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

    // PyBuffer_FillInfo INCREFs `slf` into view->obj, pinning the Vec until every
    // memoryview/slice is released (zero-copy, readonly).
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
            1, // readonly
            flags,
        );
        if r != 0 {
            return Err(PyErr::fetch(slf.py()));
        }
        Ok(())
    }

    unsafe fn __releasebuffer__(&self, _view: *mut pyo3::ffi::Py_buffer) {}
}

// ============================================================================
// Shared state + lifecycle.
// ============================================================================
struct Inner {
    // One source of truth for the lifecycle: STOP_NONE (running), STOP_EXTERNAL, or a
    // positive tid meaning that capture thread self-stopped. A re-entrant start clears
    // only its own self-stop via compare-exchange, so it can't clobber an external stop
    // that raced in mid-join (which would strand the join forever).
    stop_state: AtomicI64,
    started_ok: AtomicBool,
    start_state: AtomicU8,
    capture_tid: AtomicI64, // OS tid of the running capture thread, 0 = none
    // Lock-free per-frame settings mirrors (published by start/update_bitrate).
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
            opus_bitrate: AtomicI32::new(128000),
            use_silence_gate: AtomicBool::new(true),
            debug_logging: AtomicBool::new(false),
            emit_audio_header: AtomicBool::new(true),
        }
    }

    // The stop_state protocol lives here as the single source of truth. An external
    // stop is authoritative and wins any race; a self-stop/self-start only mutates
    // state it owns, so it can never clobber an in-flight external stop (whose join
    // would then hang forever).

    // Authoritative external stop, published before join. Stored unconditionally.
    fn request_external_stop(&self) {
        self.stop_state.store(STOP_EXTERNAL, Ordering::Release);
    }

    // Re-entrant self-stop from the capture thread's own callback. Only transitions
    // from running, so it never overwrites a pending external stop.
    fn request_self_stop(&self, me: i64) {
        let _ = self
            .stop_state
            .compare_exchange(STOP_NONE, me, Ordering::AcqRel, Ordering::Acquire);
    }

    // Re-entrant self-start: undo only our own self-stop. If an external stop landed
    // in between, the CAS fails and that stop stands (never cleared).
    fn undo_self_stop(&self, me: i64) {
        let _ = self
            .stop_state
            .compare_exchange(me, STOP_NONE, Ordering::AcqRel, Ordering::Acquire);
    }

    // Clear to running. Only ever called under the lifecycle lock (after join, before
    // spawn), where no external stop can be in flight -- the lost-stop invariant.
    fn clear_stop(&self) {
        self.stop_state.store(STOP_NONE, Ordering::Release);
    }

    // True once any stop (external or self) is pending; the capture loop uses this.
    fn stop_pending(&self) -> bool {
        self.stop_state.load(Ordering::Acquire) != STOP_NONE
    }

    // True while a worker thread is (or is still becoming) live: the startup
    // handshake is in flight, or the hot loop is running with no stop pending.
    // False once the worker failed, was stopped, or died mid-run (the hot loop
    // clears started_ok before exiting on error), so producers can surface a dead
    // stream instead of feeding state nothing services.
    fn worker_alive(&self) -> bool {
        if self.start_state.load(Ordering::Acquire) == ST_STARTING {
            return true;
        }
        self.started_ok.load(Ordering::Acquire) && !self.stop_pending()
    }
}

struct Shared {
    inner: Arc<Inner>,
    // The lifecycle lock: serializes joinable/join/reassign of the capture thread.
    thread: Mutex<Option<JoinHandle<()>>>,
}

// Registry of live captures for the atexit sweep (weak so it keeps nothing alive).
static REGISTRY: OnceLock<Mutex<Vec<Weak<Shared>>>> = OnceLock::new();
fn registry() -> &'static Mutex<Vec<Weak<Shared>>> {
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

// Locked takeover + spawn, shared by the capture and playback starts: stop/join any
// prior worker, reset the lifecycle atomics, spawn `body` on a named thread, and
// publish its JoinHandle. The external stop is set INSIDE the lock immediately
// before the join, and cleared to STOP_NONE ONLY here under the lock (after join,
// before spawn) -- the lost-stop invariant. Returns the spawned thread's id (None if
// the spawn failed), the identity a failed start hands to join_failed_start.
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
        // Best-effort nice boost: audio must not stutter when the captured workload
        // saturates the CPU. EPERM without CAP_SYS_NICE -> silently a no-op.
        unsafe {
            let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
            let _ = libc::setpriority(libc::PRIO_PROCESS, tid, -15);
        }
        t_inner.capture_tid.store(gettid(), Ordering::Release);
        body();
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

// Failed-start teardown: stop + join the thread `spawned` by THIS start attempt,
// but only if it still owns the slot. A concurrent start may have already joined it
// and published a live replacement, which must not be torn down (ThreadIds are
// never reused within a process, so the identity check cannot false-match).
fn join_failed_start(slot: &Mutex<Option<JoinHandle<()>>>, inner: &Inner, spawned: ThreadId) {
    let mut guard = slot.lock().unwrap();
    if guard.as_ref().map(|h| h.thread().id()) != Some(spawned) {
        return;
    }
    if let Some(handle) = guard.take() {
        // Set the stop INSIDE the lock before join, matching every other join site
        // (the set-before-join / lost-stop invariant).
        inner.request_external_stop();
        let _ = handle.join();
        inner.capture_tid.store(0, Ordering::Release);
    }
}

// ============================================================================
// Playback: mic-PCM handoff into the virtual "input" sink.
// ============================================================================

// Bounded byte queue with drop-oldest overflow, collapsing selkies' two-stage
// (chunk queue + reassembly bytearray) mic buffering into one bound. push runs on
// the Python side (GIL released); drain_upto runs on the playback thread.
struct PlayQueue {
    buf: Mutex<VecDeque<u8>>,
    // Bounds are (re)published by start(); atomics so start can reconfigure the
    // Arc-shared queue without swapping it. frame_bytes >= 1 (set at new()).
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

    // Apply this run's bounds and drop any stale audio from a prior run.
    fn configure(&self, max_bytes: usize, frame_bytes: usize) {
        let fb = frame_bytes.max(1);
        self.frame_bytes.store(fb, Ordering::Relaxed);
        self.max_bytes.store(max_bytes.max(fb), Ordering::Relaxed);
        self.buf.lock().unwrap().clear();
    }

    // Append client PCM; drop the OLDEST bytes past the bound (drift-tolerant).
    fn push(&self, data: &[u8]) {
        let max = self.max_bytes.load(Ordering::Relaxed);
        let mut q = self.buf.lock().unwrap();
        q.extend(data.iter().copied());
        while q.len() > max {
            q.pop_front();
        }
    }

    // Drain up to `n` bytes, clamped to what's queued and floored to a whole frame
    // (PA write requires a multiple of the sample-spec frame size).
    fn drain_upto(&self, n: usize, out: &mut Vec<u8>) {
        let fb = self.frame_bytes.load(Ordering::Relaxed);
        let mut q = self.buf.lock().unwrap();
        let mut take = n.min(q.len());
        take -= take % fb;
        out.clear();
        out.extend(q.drain(..take));
    }
}

// Opus mic-uplink decoder: decodes one packet to interleaved S16LE PCM, reusing a
// scratch buffer. Lives behind a Mutex on PbShared and is driven from write().
struct OpusPlaybackDecoder {
    dec: opus::Decoder,
    channels: usize,
    pcm: Vec<i16>,
    // RED recovery cursor (RFC 2198): timestamp of the last frame decoded, so a redundant
    // copy of a dropped frame is decoded once, in order. None until the first RED frame.
    last_ts: Option<i64>,
}

impl OpusPlaybackDecoder {
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

    fn decode_to_pcm(&mut self, packet: &[u8]) -> Option<Vec<u8>> {
        if packet.is_empty() {
            return None;
        }
        // An Opus packet decodes to at most 120 ms; 5760 samples/channel covers 48 kHz.
        let cap = 5760 * self.channels;
        if self.pcm.len() < cap {
            self.pcm.resize(cap, 0);
        }
        let samples = self.dec.decode(packet, &mut self.pcm[..cap], false).ok()?;
        let n = samples * self.channels;
        let mut out = Vec::with_capacity(n * 2);
        for &s in &self.pcm[..n] {
            out.extend_from_slice(&s.to_le_bytes());
        }
        Some(out)
    }

    // RFC 2198 RED: de-frame the payload, use the redundant copies to recover frames the
    // sender dropped, and decode each new frame once into `queue` -- all off the GIL. Blocks
    // are ordered oldest-first (redundant, then primary); each carries a 14-bit timestamp
    // offset back from `primary_ts`, so a block strictly newer than the last decoded frame
    // fills a gap left by a dropped packet.
    fn decode_red_into_queue(&mut self, payload: &[u8], primary_ts: i64, queue: &PlayQueue) {
        let n = payload.len();
        let mut i = 0usize;
        let mut offs = [0i64; RED_MAX_DISTANCE as usize];
        let mut lens = [0usize; RED_MAX_DISTANCE as usize];
        let mut nh = 0usize;
        while i < n && (payload[i] & 0x80) != 0 {
            if i + 4 > n || nh >= RED_MAX_DISTANCE as usize {
                return; // malformed, or more redundancy than the contract allows
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
        i += 1; // primary (F=0) header byte

        // Block boundaries as (ts, start, len), oldest-first; the primary is the rest.
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

        // First packet: anchor on the primary; don't replay its trailing redundancy.
        if self.last_ts.is_none() {
            let (ts, start, len) = frames[nf - 1];
            if len > 0 {
                if let Some(pcm) = self.decode_to_pcm(&payload[start..start + len]) {
                    queue.push(&pcm);
                }
            }
            self.last_ts = Some(ts);
            return;
        }
        let mut last = self.last_ts.unwrap();
        for &(ts, start, len) in frames.iter().take(nf) {
            if len > 0 && ts > last {
                if let Some(pcm) = self.decode_to_pcm(&payload[start..start + len]) {
                    queue.push(&pcm);
                }
                last = ts;
            }
        }
        self.last_ts = Some(last);
    }
}

struct PbShared {
    // Reuse the capture lifecycle core: stop_state protocol, start handshake, and
    // the worker-thread tid guard. The opus/silence mirrors on Inner are unused here.
    inner: Arc<Inner>,
    thread: Mutex<Option<JoinHandle<()>>>,
    queue: Arc<PlayQueue>,
    // Mic uplink is always Opus; write() decodes each packet through this before
    // enqueuing PCM. Set at start(); None only before a successful start.
    opus_dec: Mutex<Option<OpusPlaybackDecoder>>,
}

// Registry of live playbacks, swept by the same atexit sweep as captures.
static PLAYBACK_REGISTRY: OnceLock<Mutex<Vec<Weak<PbShared>>>> = OnceLock::new();
fn playback_registry() -> &'static Mutex<Vec<Weak<PbShared>>> {
    PLAYBACK_REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

// ============================================================================
// Capture thread.
// ============================================================================
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

/// Chromium's multistream-Opus surround layouts: (streams, coupled, mapping).
/// The same tables are advertised in the WebRTC SDP (`multiopus`), so the browser's
/// decoder inverts exactly what this encoder applies.
fn multiopus_layout(channels: i32) -> Option<(i32, i32, &'static [u8])> {
    match channels {
        6 => Some((4, 2, &[0, 4, 1, 2, 3, 5])),
        8 => Some((5, 3, &[0, 6, 1, 2, 3, 4, 5, 7])),
        _ => None,
    }
}

/// One encode surface over both Opus APIs: the `opus` crate for mono/stereo and the
/// raw multistream API for 6/8-channel surround.
enum PcmEncoder {
    Stereo(opus::Encoder),
    Multi(MultiOpus),
}

struct MultiOpus {
    st: *mut audiopus_sys::OpusMSEncoder,
}

// The raw encoder is only ever touched from the capture thread that owns RunState.
unsafe impl Send for MultiOpus {}

impl Drop for MultiOpus {
    fn drop(&mut self) {
        unsafe { audiopus_sys::opus_multistream_encoder_destroy(self.st) }
    }
}

impl PcmEncoder {
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

/// Bounded drop-oldest hand-off from the capture thread to the Python delivery
/// thread, so a slow or GIL-blocked callback can never stall the PulseAudio pump
/// (parity with the pixelflux delivery-thread model). Stale audio is worthless, so
/// overflow discards the OLDEST frame; capacity is a few frames of slack.
type FrameQueue = Option<VecDeque<(Vec<u8>, u64)>>;

struct DeliveryRing {
    q: Mutex<FrameQueue>,
    cv: Condvar,
    dropped: AtomicU64,
    capacity: usize,
}

impl DeliveryRing {
    fn new(capacity: usize) -> Self {
        Self {
            q: Mutex::new(Some(VecDeque::with_capacity(capacity))),
            cv: Condvar::new(),
            dropped: AtomicU64::new(0),
            capacity,
        }
    }

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

    fn close(&self) {
        *self.q.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.cv.notify_all();
    }
}

/// Recycles outgoing frame buffers from dropped AudioFrames back to the capture
/// thread, so the steady-state emit path allocates nothing. INVARIANT: every buffer
/// is born in `take` as `vec![0u8; buf_size]`, so bytes [0, buf_size) stay
/// initialized for the allocation's lifetime (truncate only shortens `len`), which
/// makes the `set_len` restore on reuse sound.
struct BufferPool {
    bufs: Mutex<Vec<Vec<u8>>>,
    buf_size: usize,
}

impl BufferPool {
    // Outstanding frames rarely exceed the delivery-ring capacity plus a few
    // Python-held references; anything past this cap goes back to the allocator.
    const MAX_POOLED: usize = 16;

    fn new(buf_size: usize) -> Self {
        Self { bufs: Mutex::new(Vec::new()), buf_size }
    }

    /// A fully initialized buffer of exactly `buf_size` length (the runtime path
    /// takes through PoolTaker; this direct form serves the unit tests).
    #[cfg(test)]
    fn take(&self) -> Vec<u8> {
        let recycled = self.bufs.lock().unwrap_or_else(|e| e.into_inner()).pop();
        match recycled {
            Some(v) => self.restore(v),
            None => vec![0u8; self.buf_size],
        }
    }

    /// Restore a recycled buffer to full length.
    fn restore(&self, mut v: Vec<u8>) -> Vec<u8> {
        debug_assert!(v.capacity() >= self.buf_size);
        // Sound per the pool invariant: the bytes were written at allocation and
        // truncation does not de-initialize them.
        unsafe { v.set_len(self.buf_size) };
        v
    }

    fn put(&self, v: Vec<u8>) {
        if v.capacity() < self.buf_size {
            return;
        }
        let mut g = self.bufs.lock().unwrap_or_else(|e| e.into_inner());
        if g.len() < Self::MAX_POOLED {
            g.push(v);
        }
    }

    /// Move every pooled buffer into `into` under a single lock (batched refill for
    /// the sole-consumer PoolTaker; `into` is the taker's empty local stash).
    fn drain_into(&self, into: &mut Vec<Vec<u8>>) {
        let mut g = self.bufs.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::swap(&mut *g, into);
    }
}

/// Sole-consumer view over the shared pool for the capture thread: refills are
/// batched, so the per-frame take is lock-free in the steady state while returns
/// (AudioFrame drops on the delivery/Python side) still go through the pool.
struct PoolTaker {
    pool: Arc<BufferPool>,
    local: Vec<Vec<u8>>,
}

impl PoolTaker {
    fn new(pool: Arc<BufferPool>) -> Self {
        Self { pool, local: Vec::new() }
    }

    fn take(&mut self) -> Vec<u8> {
        if self.local.is_empty() {
            self.pool.drain_into(&mut self.local);
        }
        match self.local.pop() {
            Some(v) => self.pool.restore(v),
            None => vec![0u8; self.pool.buf_size],
        }
    }

    /// Error-path return that never locks (stays in the local stash).
    fn put(&mut self, v: Vec<u8>) {
        if v.capacity() >= self.pool.buf_size && self.local.len() < BufferPool::MAX_POOLED {
            self.local.push(v);
        }
    }
}

// Per-run encode/deliver state living on the capture thread's stack.
struct RunState<'a> {
    inner: &'a Inner,
    ring: &'a DeliveryRing,
    encoder: PcmEncoder,
    frame_size_per_channel: usize,
    channels: usize,
    // Reassembly buffer sized to one Opus frame (i16 samples; byte-filled).
    accum: Vec<i16>,
    // Zeroed reference of the same length; slice equality against it lowers to a
    // single vectorized memcmp for the silence gate (vs a scalar per-sample scan).
    silence_ref: Vec<i16>,
    pcm_fill_bytes: usize,
    // Outgoing-buffer recycler shared with delivered AudioFrames (their drop refills it).
    pool: PoolTaker,
    // RFC 2198 redundancy: the last `red_distance` emitted (opus, pts) frames, kept
    // oldest-first. Per-run (reset on start; frame size is fixed for a run).
    red_history: VecDeque<(Vec<u8>, u64)>,
    red_distance: usize,
    total_samples_processed: u64,
    first_sound_detected: bool,
    last_requested_bitrate: i32,
    current_applied_bitrate: i32,
    // stats
    chunks_read: u64,
    chunks_silent: u64,
    chunks_encoded: u64,
    bytes_encoded: u64,
}

impl<'a> RunState<'a> {
    // Feed one PulseAudio fragment; emit each complete frame as it fills.
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

    fn emit_frame(&mut self) {
        self.chunks_read += 1;

        // Apply the requested bitrate only when it changed, to avoid re-configuring
        // the encoder on every frame.
        let requested = self.inner.opus_bitrate.load(Ordering::Relaxed);
        if requested != self.last_requested_bitrate {
            self.last_requested_bitrate = requested;
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
            return;
        }
        if !self.first_sound_detected {
            plog!("[pcmflux] First non-silent audio chunk detected! Encoding...");
            self.first_sound_detected = true;
        }

        let n = self.frame_size_per_channel * self.channels;
        // Frame body: legacy [0x01,0x00]+opus when red_distance==0 (byte-identical),
        // RFC 2198 RED framing when >0, or raw opus when the header is omitted. The
        // prefix depends only on pts + history, so it is written first and the packet
        // is encoded DIRECTLY into its final buffer — no assembly copy, and the
        // buffer recycles through the pool (no steady-state allocation).
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
        self.chunks_encoded += 1;
        self.bytes_encoded += encoded as u64;
        if encoded == 0 {
            self.pool.put(data);
            return;
        }
        // Retain the primary as future redundancy (bounded to red_distance, oldest-first).
        if self.red_distance > 0 {
            self.red_history
                .push_back((data[prefix..prefix + encoded].to_vec(), pts));
            while self.red_history.len() > self.red_distance {
                self.red_history.pop_front();
            }
        }
        data.truncate(prefix + encoded);

        // Hand off to the delivery thread; the capture thread never touches the GIL.
        self.ring.push(data, pts);
    }
}

// Drive the whole capture run. Sets start_state RUNNING on entering the hot loop,
// FAILED on any startup error. Returns when stop_state leaves STOP_NONE or on fatal error.
fn capture_run(inner: &Inner, settings: &Settings, callback: &Py<PyAny>) {
    // Seed lock-free mirrors from the snapshot.
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

    // Buffer attr: configured latency -> ADJUST_LATENCY + fragsize=latency; default
    // -> fragsize floored at ~20ms (prompt first frame; avoids PipeWire's ~2s default).
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
    plog!(
        "[pcmflux] Attempting to connect to PulseAudio device: {} ({})",
        device.unwrap_or("system_default"),
        if adjust_latency {
            format!("latency {}ms", settings.latency_ms)
        } else {
            "default latency".to_string()
        }
    );

    let mut mainloop = match pulse::mainloop::standard::Mainloop::new() {
        Some(m) => m,
        None => {
            elog!("[pcmflux] ERROR: pa_mainloop_new() failed.");
            fail();
            return;
        }
    };
    let mut context = match Context::new(&mainloop, "pcmflux") {
        Some(c) => c,
        None => {
            elog!("[pcmflux] ERROR: pa_context_new() failed.");
            fail();
            return;
        }
    };
    if context.connect(None, CtxFlags::NOFLAGS, None).is_err() {
        elog!("[pcmflux] ERROR: pa_context_connect() failed.");
        fail();
        return;
    }

    // Drive context -> Ready (honoring stop_state + the bounded pump).
    loop {
        let st = context.get_state();
        if st == pulse::context::State::Ready {
            break;
        }
        if !st.is_good() {
            elog!("[pcmflux] ERROR: PulseAudio context connection failed.");
            fail();
            return;
        }
        if inner.stop_pending() {
            elog!("[pcmflux] audio capture start aborted: stop during startup (context).");
            fail();
            return;
        }
        if !pump(&mut mainloop, PUMP_TIMEOUT_US) {
            elog!("[pcmflux] ERROR: mainloop iterate failed during connect.");
            fail();
            return;
        }
    }
    plog!("[pcmflux] SUCCESS: Connected to PulseAudio.");

    // Validate a NAMED device up front (async connect_record won't fail synchronously).
    if let Some(dev) = device {
        let probe = Arc::new(Mutex::new((false, false))); // (found, done)
        let p2 = probe.clone();
        let op = context.introspect().get_source_info_by_name(dev, move |res| {
            // This closure is invoked by libpulse from C (inside mainloop dispatch);
            // a panic must not unwind across that FFI boundary, so contain it here.
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
                elog!("[pcmflux] audio capture start aborted: stop during source probe.");
                drop(op);
                fail();
                return;
            }
            if !context.get_state().is_good() {
                elog!("[pcmflux] ERROR: context failed during source probe.");
                drop(op);
                fail();
                return;
            }
            if !pump(&mut mainloop, PUMP_TIMEOUT_US) {
                elog!("[pcmflux] ERROR: mainloop iterate failed during source probe.");
                drop(op);
                fail();
                return;
            }
        }
        drop(op);
        if !probe.lock().unwrap().0 {
            elog!("[pcmflux] ERROR: PulseAudio source not found: '{dev}'");
            fail();
            return;
        }
    }

    // Opus encoder (mono/stereo, or multistream surround for 6/8 channels).
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

    let mut stream = match Stream::new(&mut context, "Audio Capture", &spec, None) {
        Some(s) => s,
        None => {
            elog!("[pcmflux] ERROR: pa_stream_new() failed.");
            fail();
            return;
        }
    };
    let flags = if adjust_latency {
        StreamFlags::ADJUST_LATENCY
    } else {
        StreamFlags::NOFLAGS
    };
    if stream.connect_record(device, Some(&attr), flags).is_err() {
        elog!(
            "[pcmflux] ERROR: pa_stream_connect_record() failed (device '{}').",
            device.unwrap_or("default")
        );
        fail();
        return;
    }

    // Drive stream -> Ready.
    loop {
        let st = stream.get_state();
        if st == pulse::stream::State::Ready {
            break;
        }
        if !st.is_good() {
            elog!(
                "[pcmflux] ERROR: PulseAudio record stream failed (device '{}').",
                device.unwrap_or("default")
            );
            fail();
            return;
        }
        if inner.stop_pending() {
            elog!("[pcmflux] audio capture start aborted: stop during stream connect.");
            fail();
            return;
        }
        if !pump(&mut mainloop, PUMP_TIMEOUT_US) {
            elog!("[pcmflux] ERROR: mainloop iterate failed during stream connect.");
            fail();
            return;
        }
    }

    plog!(
        "[pcmflux] Capture loop started. Device: {}, Rate: {}, Channels: {}, Bitrate: {} kbps, \
         VBR: {}, Silence Gate: {}",
        device.unwrap_or("system_default"),
        settings.sample_rate,
        settings.channels,
        settings.opus_bitrate / 1000,
        if settings.use_vbr { "On" } else { "Off" },
        if settings.use_silence_gate { "On" } else { "Off" }
    );

    // Delivery thread: consumes encoded frames from a bounded drop-oldest ring and
    // runs the Python callback there, so GIL stalls can't back up the PA pump.
    let ring = Arc::new(DeliveryRing::new(8));
    // One buffer holds the worst-case body: RED prefix + a max-size packet (surround
    // multistream is one self-delimited packet per stream, so scale with streams).
    let max_pkt = if settings.channels > 2 { 4 * MAX_OPUS_PACKET } else { MAX_OPUS_PACKET };
    let pool = Arc::new(BufferPool::new(RED_PREFIX_MAX + max_pkt));
    let deliver_ring = Arc::clone(&ring);
    let deliver_pool = Arc::clone(&pool);
    let deliver_cb: Py<PyAny> = Python::attach(|py| callback.clone_ref(py));
    let deliver_join = std::thread::Builder::new()
        .name("pcmflux-deliver".into())
        .spawn(move || {
            unsafe {
                let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
                let _ = libc::setpriority(libc::PRIO_PROCESS, tid, -10);
            }
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
                        // Report as an unraisable exception; never propagate a callback
                        // error into the delivery loop (it must keep running).
                        e.write_unraisable(py, Some(deliver_cb.bind(py)));
                    }
                });
            }
        })
        .ok();
    if deliver_join.is_none() {
        elog!("[pcmflux] ERROR: delivery thread spawn failed.");
        fail();
        return;
    }

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
        red_distance: settings.red_distance.max(0) as usize,
        total_samples_processed: 0,
        first_sound_detected: false,
        last_requested_bitrate: settings.opus_bitrate,
        current_applied_bitrate: settings.opus_bitrate,
        chunks_read: 0,
        chunks_silent: 0,
        chunks_encoded: 0,
        bytes_encoded: 0,
    };

    inner.started_ok.store(true, Ordering::Release);
    inner.start_state.store(ST_RUNNING, Ordering::Release);

    let mut last_log = Instant::now();

    // Hot loop: bounded pump, then drain all available fragments (no read callback;
    // polling via peek/discard keeps the borrow model simple and still drains every
    // <=20ms). A stop is observed within the pump bound even when wedged.
    while !inner.stop_pending() {
        if !pump(&mut mainloop, PUMP_TIMEOUT_US) {
            elog!("[pcmflux] ERROR: mainloop iterate failed; stopping capture.");
            inner.started_ok.store(false, Ordering::Release);
            break;
        }
        let sstate = stream.get_state();
        if sstate != pulse::stream::State::Ready {
            elog!("[pcmflux] ERROR: record stream entered a non-ready state; stopping.");
            inner.started_ok.store(false, Ordering::Release);
            break;
        }

        // Drain every fragment currently buffered.
        loop {
            let mut discard = false;
            let mut done = false;
            match stream.peek() {
                Ok(PeekResult::Empty) => done = true,
                Ok(PeekResult::Hole(_)) => discard = true, // xrun: advance read index
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
                let _ = stream.discard();
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

    plog!("[pcmflux] Stop requested. Cleaning up capture loop...");
    inner.started_ok.store(false, Ordering::Release);
    let _ = stream.disconnect();
    drop(run); // encoder
    ring.close();
    if let Some(j) = deliver_join {
        let _ = j.join();
    }
    let dropped = ring.dropped.load(Ordering::Relaxed);
    if dropped > 0 {
        plog!("[pcmflux] Delivery ring dropped {dropped} stale frame(s) to a slow consumer.");
    }
    drop(stream);
    drop(context);
    drop(mainloop);
    plog!("[pcmflux] Audio capture loop finished. Resources released.");
}

// ============================================================================
// Playback thread. Owns the PA playback stream; the only thread that touches it,
// so writes are serialized structurally (no executor). Mirrors capture_run's
// startup + bounded-pump lifecycle. Sets start_state RUNNING on entering the hot
// loop, FAILED on any startup error.
// ============================================================================
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

    let mut mainloop = match pulse::mainloop::standard::Mainloop::new() {
        Some(m) => m,
        None => {
            elog!("[pcmflux] ERROR: pa_mainloop_new() failed (playback).");
            fail();
            return;
        }
    };
    let mut context = match Context::new(&mainloop, "pcmflux") {
        Some(c) => c,
        None => {
            elog!("[pcmflux] ERROR: pa_context_new() failed (playback).");
            fail();
            return;
        }
    };
    if context.connect(None, CtxFlags::NOFLAGS, None).is_err() {
        elog!("[pcmflux] ERROR: pa_context_connect() failed (playback).");
        fail();
        return;
    }

    // Drive context -> Ready (honoring stop_state + the bounded pump).
    loop {
        let st = context.get_state();
        if st == pulse::context::State::Ready {
            break;
        }
        if !st.is_good() {
            elog!("[pcmflux] ERROR: PulseAudio context connection failed (playback).");
            fail();
            return;
        }
        if inner.stop_pending() {
            elog!("[pcmflux] audio playback start aborted: stop during startup (context).");
            fail();
            return;
        }
        if !pump(&mut mainloop, PUMP_TIMEOUT_US) {
            elog!("[pcmflux] ERROR: mainloop iterate failed during connect (playback).");
            fail();
            return;
        }
    }
    plog!("[pcmflux] SUCCESS: Connected to PulseAudio (playback).");

    // tlength = target latency. prebuf must NOT be 0: with prebuf=0 playback starts
    // instantly and the realtime read pointer runs ahead of the write index, so every
    // Relative write lands "in the past" and the server silently discards it forever
    // (verified: bytes flowed at exactly realtime rate while the sink monitor stayed
    // silent). A quarter-buffer prebuf makes the stream wait for data and re-prebuffer
    // after underruns, so late chunks play (slightly delayed) instead of vanishing.
    let tlength =
        spec.usec_to_bytes(MicroSeconds(settings.latency_ms.max(0) as u64 * 1000)) as u32;
    let attr = BufferAttr {
        maxlength: u32::MAX,
        tlength,
        prebuf: (tlength / 4).max(spec.frame_size() as u32),
        minreq: u32::MAX,
        fragsize: u32::MAX,
    };

    let mut stream = match Stream::new(&mut context, "Microphone Playback", &spec, None) {
        Some(s) => s,
        None => {
            elog!("[pcmflux] ERROR: pa_stream_new() failed (playback).");
            fail();
            return;
        }
    };
    if stream
        .connect_playback(device, Some(&attr), StreamFlags::ADJUST_LATENCY, None, None)
        .is_err()
    {
        elog!(
            "[pcmflux] ERROR: pa_stream_connect_playback() failed (device '{}').",
            device.unwrap_or("default")
        );
        fail();
        return;
    }

    // Drive stream -> Ready.
    loop {
        let st = stream.get_state();
        if st == pulse::stream::State::Ready {
            break;
        }
        if !st.is_good() {
            elog!(
                "[pcmflux] ERROR: PulseAudio playback stream failed (device '{}').",
                device.unwrap_or("default")
            );
            fail();
            return;
        }
        if inner.stop_pending() {
            elog!("[pcmflux] audio playback start aborted: stop during stream connect.");
            fail();
            return;
        }
        if !pump(&mut mainloop, PUMP_TIMEOUT_US) {
            elog!("[pcmflux] ERROR: mainloop iterate failed during stream connect (playback).");
            fail();
            return;
        }
    }

    plog!(
        "[pcmflux] Playback loop started. Device: {}, Rate: {}, Channels: {}, Latency: {}ms",
        device.unwrap_or("system_default"),
        settings.sample_rate,
        settings.channels,
        settings.latency_ms
    );

    inner.started_ok.store(true, Ordering::Release);
    inner.start_state.store(ST_RUNNING, Ordering::Release);

    // Hot loop: bounded pump, then write as much queued PCM as the server will take.
    // A stop is observed within the pump bound; newly queued bytes are picked up on
    // the next pump (no cross-thread wakeup needed, mirroring capture's poll style).
    let mut scratch: Vec<u8> = Vec::new();
    let mut bytes_written: u64 = 0;
    let mut writable_hits: u64 = 0;
    let mut last_pb_log = Instant::now();
    while !inner.stop_pending() {
        if !pump(&mut mainloop, PUMP_TIMEOUT_US) {
            elog!("[pcmflux] ERROR: mainloop iterate failed; stopping playback.");
            inner.started_ok.store(false, Ordering::Release);
            break;
        }
        if stream.get_state() != pulse::stream::State::Ready {
            elog!("[pcmflux] ERROR: playback stream entered a non-ready state; stopping.");
            inner.started_ok.store(false, Ordering::Release);
            break;
        }
        if let Some(can) = stream.writable_size() {
            if can > 0 {
                writable_hits += 1;
                queue.drain_upto(can, &mut scratch); // clamped to writable, frame-aligned
                if !scratch.is_empty() {
                    // free_cb=None => PA copies; scratch is reused next iteration.
                    if let Err(e) = stream.write(&scratch, None, 0, pulse::stream::SeekMode::Relative)
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

    plog!("[pcmflux] Stop requested. Cleaning up playback loop...");
    inner.started_ok.store(false, Ordering::Release);
    let _ = stream.disconnect();
    drop(stream);
    drop(context);
    drop(mainloop);
    plog!("[pcmflux] Audio playback loop finished. Resources released.");
}

// ============================================================================
// AudioCapture pyclass.
// ============================================================================
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

    fn start_capture(
        &self,
        py: Python<'_>,
        settings: &Bound<'_, PyAny>,
        callback: Py<PyAny>,
    ) -> PyResult<()> {
        let inner = self.inner().clone();

        // Re-entrant start from the capture thread (callback called start): can't
        // join/recreate ourselves -- just undo a nested SELF-stop and return. The CAS
        // clears the stop ONLY if this same thread still owns it; if an external stop
        // stored STOP_EXTERNAL in the meantime the CAS fails and that stop stands, or
        // its mid-flight join would hang forever.
        let me = gettid();
        if inner.capture_tid.load(Ordering::Acquire) == me {
            inner.undo_self_stop(me);
            return Ok(());
        }

        let parsed = extract_settings(settings)?;

        // Stop any prior thread + spawn the new one with the GIL RELEASED. The
        // lifecycle lock + join must not be held while holding the GIL (the capture
        // thread's last callback needs the GIL); the stop/clear ordering lives in
        // spawn_worker (the lost-stop invariant).
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

        // Register for the atexit sweep (best-effort).
        if let Ok(mut reg) = registry().lock() {
            reg.retain(|w| w.strong_count() > 0);
            reg.push(Arc::downgrade(&self.shared));
        }

        // Startup handshake: wait <=~2s for the thread to publish RUNNING/FAILED.
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
            // Join the failed thread (GIL released), then surface the error. Only
            // the thread THIS call spawned is torn down; a concurrent start may
            // already own the slot with a live run that must survive.
            let shared2 = &self.shared;
            let inner2 = &inner;
            py.detach(move || join_failed_start(&shared2.thread, inner2, my_thread));
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "audio capture failed to start (see stderr for details)",
            ));
        }
        Ok(())
    }

    fn stop_capture(&self, py: Python<'_>) {
        let inner = self.inner();
        // Re-entrant stop from the capture thread itself: record a self-stop (our own
        // tid), do NOT join (can't self-join; would deadlock another thread joining us).
        // Don't clobber an external stop already in effect -- it must win the join.
        let me = gettid();
        if inner.capture_tid.load(Ordering::Acquire) == me {
            inner.request_self_stop(me);
            return;
        }
        // Take the lifecycle lock AND join with the GIL released: the capture thread's
        // last callback needs the GIL, so we must not hold it while blocking on the
        // lock or in join() (else: us holding GIL + blocked on lock, the lock-holder
        // joining a thread that wants the GIL -> deadlock).
        let shared = &self.shared;
        py.detach(|| {
            let mut guard = shared.thread.lock().unwrap();
            if let Some(handle) = guard.take() {
                // Authoritative external stop, INSIDE the lock before join; it must
                // win over any concurrent self-stop.
                inner.request_external_stop();
                let _ = handle.join();
                inner.capture_tid.store(0, Ordering::Release);
            }
        });
    }

    fn update_audio_bitrate(&self, bps: i32) {
        // Atomic mirror; the capture loop applies it on the next frame.
        self.inner().opus_bitrate.store(bps, Ordering::Relaxed);
    }

    #[getter]
    fn is_capturing(&self) -> bool {
        let inner = self.inner();
        inner.started_ok.load(Ordering::Acquire) && !inner.stop_pending()
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        // Best-effort stop on GC/dealloc. If we ARE the capture thread (re-entrant),
        // don't join; otherwise take the lifecycle lock + join with the GIL released.
        let inner = &self.shared.inner;
        let me = gettid();
        if inner.capture_tid.load(Ordering::Acquire) == me {
            // Self-stop only if still running; never clobber a pending external stop.
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

// ============================================================================
// AudioPlayback pyclass. Symmetric to AudioCapture: same lifecycle protocol, a
// playback stream instead of a record stream, and a bounded drop-oldest queue
// (write) instead of a Python callback. Since Python never holds the PA handle,
// the close-vs-inflight-write UAF the pasimple path guards is structurally gone.
// ============================================================================
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

    fn start(&self, py: Python<'_>, settings: &Bound<'_, PyAny>) -> PyResult<()> {
        let inner = self.inner().clone();

        // Re-entrant start from the playback thread: just undo a nested self-stop.
        let me = gettid();
        if inner.capture_tid.load(Ordering::Acquire) == me {
            inner.undo_self_stop(me);
            return Ok(());
        }

        let parsed = extract_pb_settings(settings)?;
        // Bytes bound + frame alignment for this run (drops any stale audio).
        let frame_bytes = (parsed.channels.max(1) as usize) * 2;
        self.shared.queue.configure(parsed.max_buffer_bytes, frame_bytes);

        // Mic uplink is always Opus (both transports): create the decoder up front so
        // write() decodes packets to PCM (GIL released) for the same playback loop.
        let decoder = OpusPlaybackDecoder::new(parsed.sample_rate, parsed.channels);
        if decoder.is_none() {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "failed to create Opus decoder for playback",
            ));
        }
        *self.shared.opus_dec.lock().unwrap_or_else(|e| e.into_inner()) = decoder;

        // Stop any prior thread + spawn the new one with the GIL RELEASED; the
        // stop/clear ordering lives in spawn_worker (the lost-stop invariant).
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

        // Register for the atexit sweep (best-effort).
        if let Ok(mut reg) = playback_registry().lock() {
            reg.retain(|w| w.strong_count() > 0);
            reg.push(Arc::downgrade(&self.shared));
        }

        // Startup handshake: wait <=~2s for the thread to publish RUNNING/FAILED.
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
            // Tear down only the thread THIS call spawned; a concurrent start may
            // already own the slot with a live run that must survive.
            let shared2 = &self.shared;
            let inner2 = &inner;
            py.detach(move || join_failed_start(&shared2.thread, inner2, my_thread));
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "audio playback failed to start (see stderr for details)",
            ));
        }
        Ok(())
    }

    // Hot path: one copy into the bounded queue with the GIL released; never blocks
    // on PA, returns immediately. Drop-oldest happens inside push. Raises once no
    // playback thread services the queue (start failure, stop, or a mid-run PA
    // death) so the caller's reopen-on-error path engages instead of the audio
    // being swallowed silently.
    fn write(&self, py: Python<'_>, data: &Bound<'_, PyBytes>) -> PyResult<()> {
        if !self.inner().worker_alive() {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "audio playback is not running (stream failed, stopped, or never started)",
            ));
        }
        let b = data.as_bytes();
        py.detach(|| {
            // Opus uplink only: decode one packet to PCM and enqueue that; a bad packet is
            // dropped rather than corrupting the stream.
            let mut dec = self.shared.opus_dec.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(d) = dec.as_mut() {
                if let Some(pcm) = d.decode_to_pcm(b) {
                    self.shared.queue.push(&pcm);
                }
            }
        });
        Ok(())
    }

    // Like write(), but the payload is an RFC 2198 RED frame (mic uplink over WebRTC/UDP):
    // de-frame + loss-recover + decode entirely off the GIL. `primary_ts` is the packet's
    // (monotonic) RTP timestamp; the redundant blocks carry offsets back from it.
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

    fn stop(&self, py: Python<'_>) {
        let inner = self.inner();
        // Re-entrant stop from the playback thread itself: record a self-stop, don't
        // join (can't self-join). Don't clobber an external stop already in effect.
        let me = gettid();
        if inner.capture_tid.load(Ordering::Acquire) == me {
            inner.request_self_stop(me);
            return;
        }
        // Join with the GIL released so a slow PA disconnect can't stall the
        // interpreter; stop() returns only once the thread is joined (sink released).
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

    #[getter]
    fn is_running(&self) -> bool {
        let inner = self.inner();
        inner.started_ok.load(Ordering::Acquire) && !inner.stop_pending()
    }
}

impl Drop for AudioPlayback {
    fn drop(&mut self) {
        // Best-effort stop on GC/dealloc; symmetric to AudioCapture::drop.
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

// ============================================================================
// atexit sweep: stop every live capture + playback before interpreter shutdown.
// ============================================================================
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

    #[test]
    fn multiopus_surround_roundtrip() {
        // Encode 5.1 with a tone only on FC (input channel 2); decode with the same
        // layout and verify the energy lands on the same channel — proving the
        // layout tables are self-consistent end to end.
        let channels = 6usize;
        let frame = 480usize; // 10 ms at 48 kHz
        let mut enc = PcmEncoder::new(48000, channels as i32, true, 256000).expect("encoder");
        let mut pcm = vec![0i16; frame * channels];
        for i in 0..frame {
            let v = (8000.0 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 48000.0).sin())
                as i16;
            pcm[i * channels + 2] = v;
        }
        let mut out = vec![0u8; 4 * MAX_OPUS_PACKET];
        // A couple of frames so the codec state settles.
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

    #[test]
    fn opus_durations() {
        for ms in [2.5, 5.0, 10.0, 20.0, 40.0, 60.0] {
            assert!(valid_opus_duration(ms));
        }
        for ms in [0.0, 1.0, 3.0, 15.0, 25.0, 30.0, 50.0, 100.0] {
            assert!(!valid_opus_duration(ms));
        }
    }

    #[test]
    fn frame_geometry() {
        // 48 kHz, 20 ms, stereo: 960 samples/ch, 3840 PCM bytes.
        let fspc = (48000usize * 20) / 1000;
        assert_eq!(fspc, 960);
        assert_eq!(fspc * 2 * 2, 3840);
        // mono 10 ms @ 24 kHz
        let m = (24000usize * 10) / 1000;
        assert_eq!(m, 240);
        assert_eq!(m * 2, 480);
    }

    // red_distance == 0 must be byte-identical to the legacy [0x01,0x00]+opus frame,
    // and the omit-header path must return the raw opus with no prefix (unchanged).
    #[test]
    fn red_zero_is_byte_identical_legacy() {
        let opus = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x42];
        let hist: VecDeque<(Vec<u8>, u64)> = VecDeque::new();

        let legacy_expected = {
            let mut v = vec![0x01u8, 0x00];
            v.extend_from_slice(&opus);
            v
        };
        assert_eq!(build_ws_body(&opus, 4096, &hist, 0, true), legacy_expected);
        // Header omitted: raw opus, no [0x01,n] prefix (pre-RED behavior).
        assert_eq!(build_ws_body(&opus, 4096, &hist, 0, false), opus);
    }

    // red_distance == 2: parse the emitted body back (n_red 4-byte headers, 1-byte
    // primary header, split datas by their lengths) and assert the primary and the two
    // redundant blocks round-trip with the expected oldest-first offsets 1920 & 960.
    #[test]
    fn red_two_roundtrips() {
        let f_n2 = vec![0xA0, 0xA1, 0xA2]; // frame N-2, pts 0
        let f_n1 = vec![0xB0, 0xB1, 0xB2, 0xB3]; // frame N-1, pts 960
        let primary = vec![0xC0, 0xC1]; // frame N, pts 1920
        let mut hist: VecDeque<(Vec<u8>, u64)> = VecDeque::new();
        hist.push_back((f_n2.clone(), 0)); // front = oldest
        hist.push_back((f_n1.clone(), 960));

        let body = build_ws_body(&primary, 1920, &hist, 2, true);
        assert_eq!(body[0], 0x01);
        let n_red = body[1] as usize;
        assert_eq!(n_red, 2);
        // Primary pts (low 32 bits, big-endian) precedes the redundant headers.
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

        // Oldest-first: block 0 is N-2 (offset 1920), block 1 is N-1 (offset 960).
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

    // Build an RFC 2198 RED payload: redundant blocks (oldest-first, each with a 14-bit
    // timestamp offset back from the primary) then the primary. Mirrors the wire format
    // decode_red_into_queue parses (the mic-uplink counterpart of build_ws_body).
    fn build_red_payload(reds: &[(u64, &[u8])], primary: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        for (off, blk) in reds {
            let field = (((*off as u32) & 0x3FFF) << 10) | ((blk.len() as u32) & 0x3FF);
            v.push(0x80 | (RED_BLOCK_PT & 0x7F)); // F=1: another block follows
            v.push((field >> 16) as u8);
            v.push((field >> 8) as u8);
            v.push(field as u8);
        }
        v.push(RED_BLOCK_PT & 0x7F); // primary header, F=0
        for (_, blk) in reds {
            v.extend_from_slice(blk);
        }
        v.extend_from_slice(primary);
        v
    }

    // `n` distinct valid 20 ms mono Opus packets at 24 kHz (480 samples/frame).
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

    // Each decoded 20 ms mono frame is 480 samples * 2 bytes.
    const FRAME_PCM_BYTES: usize = 480 * 2;

    // A dropped middle packet is recovered from the next packet's redundancy: the redundant
    // copy of the gap frame is decoded, while the redundant copy of an already-played frame
    // is not (dedup by timestamp). This is the off-GIL RED path that replaced the Python
    // de-framer in rtc.py.
    #[test]
    fn red_playback_recovers_lost_frame() {
        let f = opus_frames(3); // ts 1000, 1480, 1960 (480-sample steps at 24 kHz)
        let mut dec = OpusPlaybackDecoder::new(24000, 1).unwrap();
        let q = PlayQueue::new();
        q.configure(1 << 20, 2);

        // Packet 1 (primary ts=1000, no redundancy): anchor, one frame.
        dec.decode_red_into_queue(&build_red_payload(&[], &f[0]), 1000, &q);
        // Packet 2 (ts=1480) is dropped in transit.
        // Packet 3 (primary ts=1960, redundant copies of 1000 and 1480).
        let pkt3 = build_red_payload(&[(960, &f[0]), (480, &f[1])], &f[2]);
        dec.decode_red_into_queue(&pkt3, 1960, &q);

        let mut out = Vec::new();
        q.drain_upto(1 << 20, &mut out);
        // 1000 (anchor) + 1480 (recovered) + 1960 (primary); the redundant 1000 is deduped.
        assert_eq!(out.len(), 3 * FRAME_PCM_BYTES);
        assert_eq!(dec.last_ts, Some(1960));
    }

    // With no loss, redundancy is pure overhead: every frame decodes exactly once and the
    // redundant copies are dropped, so three packets yield three frames -- not more.
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

    // A too-old (offset > 16383) and an oversize (len > 1023) history block are both
    // skipped, so n_red counts only the block that fit the RFC 2198 fields.
    #[test]
    fn red_skips_oversize_and_too_old() {
        let too_old = vec![0x11u8; 3];
        let oversize = vec![0x22u8; 1100];
        let good = vec![0x33u8; 5];
        let primary = vec![0x44u8; 2];
        let primary_pts = 100_000u64;

        let mut hist: VecDeque<(Vec<u8>, u64)> = VecDeque::new();
        hist.push_back((too_old, primary_pts - 20_000)); // offset 20000 > 16383 -> skip
        hist.push_back((oversize, primary_pts - 1920)); // len 1100 > 1023 -> skip
        hist.push_back((good.clone(), primary_pts - 960)); // fits -> included

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

    // red_distance > 0 with no usable history is primary-only RED: [0x01,0x00] + a
    // 1-byte primary header + primary data (distinct from the legacy no-header form).
    #[test]
    fn red_empty_history_falls_back_to_legacy() {
        // With RED enabled but no usable redundancy (first frame after a (re)start), the WS
        // wire must collapse to the legacy 2-byte [0x01,0x00]+opus so the client's n_red==0
        // path strips exactly 2 bytes -- a lone primary-only RED header would be mis-stripped.
        let opus = vec![0x77u8; 4];
        let hist: VecDeque<(Vec<u8>, u64)> = VecDeque::new();
        let body = build_ws_body(&opus, 960, &hist, 2, true);
        assert_eq!(body[0], 0x01);
        assert_eq!(body[1], 0x00);
        assert_eq!(&body[2..], &opus[..]);
        assert_eq!(body.len(), 2 + opus.len());
    }

    #[test]
    fn silence_detection() {
        let silent = vec![0i16; 960 * 2];
        assert!(silent.iter().all(|&s| s == 0));
        let mut not = silent.clone();
        not[123] = 7;
        assert!(!not.iter().all(|&s| s == 0));
    }

    // Deterministic lost-stop probe against the real stop_state protocol. A "capture
    // thread" hammers request_self_stop + undo_self_stop (the re-entrant callback
    // pattern) while another thread issues request_external_stop and waits for the
    // capture loop's stop_pending() to observe it. The external stop must never be
    // cleared by a self-start, so the thread always observes it and the join returns
    // (no hang). The bounded spin count is the in-test watchdog.
    #[test]
    fn external_stop_never_lost_to_self_restart() {
        use std::sync::Arc;
        let me: i64 = 987654; // stand-in capture-thread tid
        for _ in 0..5000 {
            let inner = Arc::new(Inner::new());
            let inner_c = inner.clone();
            let observed = Arc::new(AtomicBool::new(false));
            let observed_c = observed.clone();
            let h = std::thread::spawn(move || {
                let mut spins: u64 = 0;
                loop {
                    // Re-entrant self-stop then self-start (undo), mirroring the callback.
                    inner_c.request_self_stop(me);
                    inner_c.undo_self_stop(me);
                    // Capture-loop exit condition.
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
            inner.request_external_stop(); // authoritative external stop
            h.join().unwrap();
            assert!(observed.load(Ordering::Acquire));
            // External stop stands: a self-start never cleared it back to STOP_NONE.
            assert_eq!(inner.stop_state.load(Ordering::Acquire), STOP_EXTERNAL);
            assert!(inner.stop_pending());
        }
    }

    // Pushing past the byte bound drops the OLDEST bytes, keeping the newest window.
    #[test]
    fn playqueue_drop_oldest_keeps_newest() {
        let q = PlayQueue::new();
        q.configure(8, 2); // 8-byte bound, 2-byte (mono s16) frames
        q.push(&[1, 2, 3, 4, 5, 6]);
        q.push(&[7, 8, 9, 10, 11, 12]); // 12 bytes -> drop oldest 4
        let mut out = Vec::new();
        q.drain_upto(100, &mut out);
        assert_eq!(out, vec![5, 6, 7, 8, 9, 10, 11, 12]);
        // Fully drained now.
        q.drain_upto(100, &mut out);
        assert!(out.is_empty());
    }

    // drain_upto clamps to the requested count AND floors to a whole frame, so a PA
    // write never gets a partial frame regardless of frame size.
    #[test]
    fn playqueue_drain_is_frame_aligned() {
        let q = PlayQueue::new();
        q.configure(1000, 4); // 4-byte (stereo s16) frames
        q.push(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]); // 10 bytes
        let mut out = Vec::new();
        q.drain_upto(7, &mut out); // clamp 7 -> floor to 4
        assert_eq!(out, vec![0, 1, 2, 3]);
        q.drain_upto(100, &mut out); // 6 left -> floor to 4
        assert_eq!(out, vec![4, 5, 6, 7]);
        q.drain_upto(100, &mut out); // 2 left -> floor to 0 (partial frame withheld)
        assert!(out.is_empty());
    }

    // configure() applies the new bounds and drops stale audio from a prior run.
    #[test]
    fn playqueue_configure_resets() {
        let q = PlayQueue::new();
        q.push(&[1, 2, 3, 4]);
        q.configure(96000, 2);
        let mut out = Vec::new();
        q.drain_upto(100, &mut out);
        assert!(out.is_empty(), "configure must clear stale audio");
    }

    // worker_alive gates AudioPlayback::write. It must hold through the startup
    // handshake and the healthy run, and go false the moment the hot loop's error
    // exit clears started_ok -- even with no stop pending and start_state still
    // RUNNING (the silent mid-run death case).
    #[test]
    fn worker_alive_tracks_loop_error_exit() {
        let inner = Inner::new();
        assert!(!inner.worker_alive(), "no worker yet");

        inner.start_state.store(ST_STARTING, Ordering::Release);
        assert!(inner.worker_alive(), "startup handshake counts as alive");

        // Hot-loop entry.
        inner.started_ok.store(true, Ordering::Release);
        inner.start_state.store(ST_RUNNING, Ordering::Release);
        assert!(inner.worker_alive());

        // Mid-run death: the loop stores started_ok=false and breaks; stop_state
        // stays STOP_NONE and start_state stays RUNNING.
        inner.started_ok.store(false, Ordering::Release);
        assert_eq!(inner.stop_state.load(Ordering::Acquire), STOP_NONE);
        assert!(!inner.worker_alive(), "dead loop must be observable");

        // A pending external stop also reads as not-alive.
        inner.started_ok.store(true, Ordering::Release);
        inner.request_external_stop();
        assert!(!inner.worker_alive());
    }

    // A LOSING start's failed-start cleanup must never tear down a WINNING start's
    // freshly spawned thread. Reproduces the bad interleaving: L's worker fails, W
    // takes the slot over (joins dead L, spawns live W) BEFORE L runs its ST_FAILED
    // cleanup; the late cleanup must be an identity-mismatch no-op, leaving W
    // running and still externally stoppable.
    #[test]
    fn failed_start_cleanup_spares_concurrent_winner() {
        let inner = Arc::new(Inner::new());
        let slot: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

        // L: fails during startup (a bad PA connect).
        let li = inner.clone();
        let l_id = spawn_worker(&slot, &inner, "loser", move || {
            li.started_ok.store(false, Ordering::Release);
            li.start_state.store(ST_FAILED, Ordering::Release);
        })
        .unwrap();
        while inner.start_state.load(Ordering::Acquire) != ST_FAILED {
            std::thread::yield_now();
        }

        // W: takes over the slot and reaches the hot loop.
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

        // L's cleanup fires late: the slot now belongs to W, so nothing may change.
        join_failed_start(&slot, &inner, l_id);
        assert!(slot.lock().unwrap().is_some(), "winner's handle must survive");
        assert_eq!(inner.stop_state.load(Ordering::Acquire), STOP_NONE);
        assert!(inner.worker_alive(), "winner must still be running");

        // The ordinary external stop path must still take W down.
        {
            let mut g = slot.lock().unwrap();
            let h = g.take().expect("winner handle present");
            inner.request_external_stop();
            h.join().unwrap();
        }
        assert!(!inner.worker_alive());
    }

    // Two threads race the full start sequence (spawn_worker + handshake poll +
    // conditional failed-start cleanup). spawn_worker joins the prior worker before
    // spawning, so bodies run in spawn order: the first fails, the second serves
    // until stopped. Whatever the interleaving, the second run must end RUNNING
    // with no stray stop -- a loser cleanup that killed the winner would leave an
    // empty slot and STOP_EXTERNAL behind.
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
                            // First run: startup failure.
                            bi.started_ok.store(false, Ordering::Release);
                            bi.start_state.store(ST_FAILED, Ordering::Release);
                        } else {
                            // Second run: hot loop until stopped.
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
                    // Startup handshake, as in start_capture/start.
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

            // External stop still reaches the winner.
            {
                let mut g = slot.lock().unwrap();
                let h = g.take().expect("winner handle present");
                inner.request_external_stop();
                h.join().unwrap();
            }
            assert!(!inner.worker_alive());
        }
    }

    #[test]
    fn prefix_writer_matches_build_ws_body() {
        let primary: Vec<u8> = (0u8..200).collect();
        let mut mixed: VecDeque<(Vec<u8>, u64)> = VecDeque::new();
        mixed.push_back((vec![1u8; 100], 0)); // aged out of the 14-bit offset at high pts
        mixed.push_back((vec![2u8; RED_MAX_LEN + 1], 960)); // oversized, always skipped
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
        // Undersized foreign buffers are rejected, not pooled.
        pool.put(Vec::new());
        assert_eq!(pool.take().len(), 64);
    }

    #[test]
    fn audio_frame_drop_refills_pool() {
        let pool = Arc::new(BufferPool::new(32));
        let buf = pool.take();
        let ptr = buf.as_ptr() as usize;
        drop(AudioFrame { data: buf, pts: 0, pool: Some(Arc::clone(&pool)) });
        let recycled = pool.take();
        assert_eq!(recycled.as_ptr() as usize, ptr);
    }

    // Not a correctness test: isolates the emit-path assembly cost (copy-based
    // build_ws_body + per-frame alloc vs pooled in-place prefix) with the encoder
    // stubbed to a memcpy. Run with:
    //   cargo test --release bench_emit_assembly -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bench_emit_assembly() {
        use std::hint::black_box;
        use std::time::Instant;

        const ITERS: u32 = 500_000;
        const PAYLOAD: usize = 200; // typical Opus packet at 128 kbps / 10 ms
        let src = vec![0xA5u8; PAYLOAD];
        let mut hist: VecDeque<(Vec<u8>, u64)> = VecDeque::new();
        hist.push_back((vec![6u8; 180], 0));
        hist.push_back((vec![7u8; 180], 480));

        for (red, label) in [(0usize, "red=0"), (2usize, "red=2")] {
            let old = Instant::now();
            let mut out = vec![0u8; MAX_OPUS_PACKET];
            for i in 0..ITERS {
                out[..PAYLOAD].copy_from_slice(&src); // encode stub
                let data = build_ws_body(&out[..PAYLOAD], 960 + i as u64, &hist, red, true);
                black_box(&data);
            }
            let old_ns = old.elapsed().as_nanos() / ITERS as u128;

            // Circulate at the delivery-ring depth so refill batching engages the
            // way it does live (several returns per drain, not one).
            let pool = Arc::new(BufferPool::new(RED_PREFIX_MAX + MAX_OPUS_PACKET));
            let mut taker = PoolTaker::new(Arc::clone(&pool));
            let mut inflight: VecDeque<Vec<u8>> = VecDeque::new();
            let new = Instant::now();
            for i in 0..ITERS {
                let mut data = taker.take();
                let prefix =
                    write_ws_prefix_into(&mut data, 960 + i as u64, &hist, red, true);
                data[prefix..prefix + PAYLOAD].copy_from_slice(&src); // encode stub
                data.truncate(prefix + PAYLOAD);
                black_box(&data);
                inflight.push_back(data);
                if inflight.len() > 8 {
                    pool.put(inflight.pop_front().unwrap()); // stands in for AudioFrame drop
                }
            }
            let new_ns = new.elapsed().as_nanos() / ITERS as u128;
            println!("{label}: old {old_ns} ns/frame -> pooled in-place {new_ns} ns/frame");
        }
    }
}

#[pymodule]
fn pcmflux(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<AudioCapture>()?;
    m.add_class::<AudioCaptureSettings>()?;
    m.add_class::<AudioFrame>()?;
    m.add_class::<AudioPlayback>()?;
    m.add_class::<AudioPlaybackSettings>()?;
    m.add_function(wrap_pyfunction!(_stop_all_captures, m)?)?;
    // Stop all live captures at interpreter exit so a still-running capture thread
    // isn't calling into Python during finalization.
    if let Ok(atexit) = m.py().import("atexit") {
        let _ = atexit.call_method1("register", (m.getattr("_stop_all_captures")?,));
    }
    Ok(())
}

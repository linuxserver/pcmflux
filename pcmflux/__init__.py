import ctypes
import os
from typing import Callable

# --- CTypes Structure and Type Definitions ---
# These structures mirror the definitions in the C++ source code, allowing
# Python to interface directly with the shared library.

class AudioCaptureSettings(ctypes.Structure):
    """Maps to the C++ AudioCaptureSettings struct."""
    _fields_ = [
        ("device_name", ctypes.c_char_p),
        ("sample_rate", ctypes.c_uint32),
        ("channels", ctypes.c_int),
        ("opus_bitrate", ctypes.c_int),
        ("frame_duration_ms", ctypes.c_int),
        ("use_vbr", ctypes.c_bool),
        ("use_silence_gate", ctypes.c_bool),
        ("debug_logging", ctypes.c_bool),
        ("latency_ms", ctypes.c_int),
        # Append-only fields (must stay last to match the C++ struct order/ABI).
        #   omit_audio_header: False (default) = C++ prepends the 2-byte header
        #     [0x01, 0x00] natively; True = raw Opus (WebRTC).
        #   deferred_free: False (default) = wrapper copies bytes and frees the C
        #     buffer; True = zero-copy OwnedAudioFrame hand-off (Python frees later).
        ("omit_audio_header", ctypes.c_bool),
        ("deferred_free", ctypes.c_bool),
    ]

class AudioChunkEncodeResult(ctypes.Structure):
    """Maps to the C++ AudioChunkEncodeResult struct."""
    _fields_ = [
        ("size", ctypes.c_int),
        ("data", ctypes.POINTER(ctypes.c_ubyte)),
        ("pts", ctypes.c_uint64),
    ]

# Defines the function signature for the callback passed to the C++ library.
AudioChunkCallback = ctypes.CFUNCTYPE(
    None, ctypes.POINTER(AudioChunkEncodeResult), ctypes.c_void_p
)


# Fallback so OwnedAudioFrame.close()/take() never raise NameError; rebound to the
# real C function once the library loads (below).
_free_result_data = None


class OwnedAudioFrame:
    """Refcount-owned, zero-copy view over a C-allocated encoded audio chunk.

    Mirrors pixelflux's OwnedFrame. In deferred-free mode C++ hands the encoded
    buffer (header+Opus, or raw Opus) to Python; this exposes it as a zero-copy
    memoryview and frees it exactly once when finalized.

    memoryview() aliases the C buffer with no copy and back-references self, so the
    view (and any slice a transport retains mid-write) keeps self -- and thus the C
    buffer -- alive until the last view releases. Works on every Python version (no
    PEP 688): safe to pass straight to ws.send_bytes.
    """
    # Pin contract: memoryview -> ctypes array -> self (via arr._pf_owner), so while any
    # view or slice lives, self can't be collected and __del__ (the single free) can't run.
    __slots__ = ("_data_ptr", "_ptr_value", "size", "pts", "_freed", "__weakref__")

    def __init__(self, data_ptr, size, pts):
        self._data_ptr = data_ptr
        # Integer address for from_address(); kept separate from the typed ctypes pointer
        # so the free path (which needs the typed pointer) and memoryview() don't share storage.
        self._ptr_value = ctypes.cast(data_ptr, ctypes.c_void_p).value
        self.size = size
        self.pts = pts
        self._freed = False

    def memoryview(self):
        if self._freed:
            raise ValueError("OwnedAudioFrame already freed")
        arr = (ctypes.c_char * self.size).from_address(self._ptr_value)  # aliases the C Opus buffer, zero-copy
        arr._pf_owner = self   # pin: view -> arr -> self; __del__ (free) can't run under a live view
        return memoryview(arr).cast('B')

    @staticmethod
    def take(result_ptr):
        """Take ownership of result_ptr.contents.data, returning an OwnedAudioFrame.

        Returns None when there's no data to own. Leak-safe: if construction fails
        it frees the C buffer and re-raises (in deferred mode the wrapper callback
        won't free it). Call this BEFORE any other failable/early-return step.
        """
        result = result_ptr.contents
        if not (result.data and result.size > 0):
            return None
        # A pointer fetched from a ctypes field aliases the field's storage, so
        # nulling result.data below would null it in the frame too. Snapshot the
        # address into an INDEPENDENT pointer first.
        data_ptr = ctypes.cast(
            ctypes.cast(result.data, ctypes.c_void_p).value, ctypes.POINTER(ctypes.c_ubyte)
        )
        try:
            frame = OwnedAudioFrame(data_ptr, result.size, result.pts)
            # Null result.data to signal the wrapper callback not to free it: the
            # frame now owns the single free. Keying on the actual transfer keeps
            # deferred mode leak-safe if a consumer returns/raises before take().
            result.data = ctypes.cast(None, ctypes.POINTER(ctypes.c_ubyte))
            return frame
        except BaseException:
            try:
                if _free_result_data is not None:
                    _free_result_data(result_ptr)
            except Exception:
                pass
            raise

    def _free(self):
        # The single guarded free, exactly once. Only reached from __del__: the pin means
        # this can't run while any view aliases the buffer, so the free is never a UAF.
        if not self._freed and self._data_ptr and _free_result_data is not None:
            self._freed = True
            r = AudioChunkEncodeResult()
            r.data = self._data_ptr
            r.size = self.size
            _free_result_data(ctypes.byref(r))
            self._data_ptr = None

    def close(self):
        """No-op kept for API compatibility.

        Without a PEP 688 export count we can't tell whether a view is live, so close()
        must NOT free: a caller holding both this frame and a still-live memoryview/slice
        would otherwise hit a use-after-free. The buffer is freed by __del__ once the pin's
        last view drops and this object is collected (prompt under CPython refcounting in
        the normal path where the consumer releases both the view and the owner).
        """
        pass

    def __del__(self):
        # Reached only after every pinning view is released; free the C buffer exactly once.
        # Swallow any error so finalization can't raise.
        try:
            self._free()
        except Exception:
            pass


# --- Shared Library Loading and Function Prototyping ---

def _load_shared_library():
    """Locates, loads, and prototypes functions from the C++ shared library."""
    lib_name = 'audio_capture_module.so'
    lib_dir = os.path.dirname(__file__)
    lib_path = os.path.join(lib_dir, lib_name)

    try:
        lib = ctypes.CDLL(lib_path)
    except OSError as e:
        raise OSError(
            f"Could not load shared library at '{lib_path}'. "
            f"Ensure the library has been compiled and is in the correct directory. "
            f"Original error: {e}"
        ) from e

    # create_audio_capture_module
    lib.create_audio_capture_module.restype = ctypes.c_void_p
    lib.create_audio_capture_module.argtypes = []

    # destroy_audio_capture_module
    lib.destroy_audio_capture_module.restype = None
    lib.destroy_audio_capture_module.argtypes = [ctypes.c_void_p]

    # start_audio_capture
    lib.start_audio_capture.restype = None
    lib.start_audio_capture.argtypes = [
        ctypes.c_void_p,
        AudioCaptureSettings,
        AudioChunkCallback,
        ctypes.c_void_p,
    ]

    # stop_audio_capture
    lib.stop_audio_capture.restype = None
    lib.stop_audio_capture.argtypes = [ctypes.c_void_p]

    # free_audio_chunk_encode_result_data
    lib.free_audio_chunk_encode_result_data.restype = None
    lib.free_audio_chunk_encode_result_data.argtypes = [
        ctypes.POINTER(AudioChunkEncodeResult)
    ]

    lib.update_audio_bitrate.restype = None
    lib.update_audio_bitrate.argtypes = [ctypes.c_void_p, ctypes.c_int]

    lib.is_audio_capture_running.restype = ctypes.c_int
    lib.is_audio_capture_running.argtypes = [ctypes.c_void_p]

    # Fail fast on a ctypes/C++ AudioCaptureSettings ABI mismatch. hasattr-guarded
    # so an older .so without the symbol just skips the check.
    if hasattr(lib, "pcmflux_audio_capture_settings_size"):
        lib.pcmflux_audio_capture_settings_size.restype = ctypes.c_int
        lib.pcmflux_audio_capture_settings_size.argtypes = []
        _c_size = lib.pcmflux_audio_capture_settings_size()
        if _c_size != ctypes.sizeof(AudioCaptureSettings):
            raise RuntimeError(
                f"AudioCaptureSettings ABI mismatch: C++={_c_size} "
                f"ctypes={ctypes.sizeof(AudioCaptureSettings)}"
            )

    return lib

# Load the library and assign functions to module-level variables.
_lib = _load_shared_library()
_create_module = _lib.create_audio_capture_module
_destroy_module = _lib.destroy_audio_capture_module
_start_capture = _lib.start_audio_capture
_stop_capture = _lib.stop_audio_capture
# Rebinds the module-level fallback declared above (None) to the real C function,
# so OwnedAudioFrame.take()/close() (which reference this global) free correctly.
_free_result_data = _lib.free_audio_chunk_encode_result_data
_update_bitrate = _lib.update_audio_bitrate
_is_running = _lib.is_audio_capture_running

# --- Main Python Wrapper Class ---

class AudioCapture:
    """A Pythonic wrapper for the C++ audio capture module."""

    def __init__(self):
        # Bind teardown/dispatch C entry points as instance attributes: __del__ and
        # the C++ thread can run at interpreter shutdown when module globals are
        # already None, but instance attributes survive (avoids a TypeError there).
        self._stop_capture = _stop_capture
        self._destroy_module = _destroy_module
        self._free_result_data = _free_result_data
        self._is_running = _is_running

        self._module_handle = _create_module()
        if not self._module_handle:
            raise RuntimeError("Failed to create the underlying audio capture module.")

        # Store the C callback object to prevent it from being garbage collected.
        self._c_callback = None
        self._python_callback = None
        self._is_capturing = False
        # True when the session uses the zero-copy hand-off (settings.deferred_free):
        # the user callback must take ownership via OwnedAudioFrame.take().
        self._deferred_free = False
        # Strong ref to the device-name bytes; C++ holds the raw pointer and the
        # capture thread reads it asynchronously, so it must outlive start_capture().
        self._device_name_buf = None

    def __del__(self):
        # At interpreter shutdown the module globals may already be None, so use
        # the instance-bound C entry points and never raise from __del__.
        try:
            handle = getattr(self, '_module_handle', None)
            if not handle:
                return
            stop = getattr(self, '_stop_capture', None)
            destroy = getattr(self, '_destroy_module', None)
            if getattr(self, '_is_capturing', False) and stop is not None:
                stop(handle)
                self._is_capturing = False
            # Clear the handle only after it's actually destroyed; if destroy is
            # unavailable, keep it so a later attempt (or process exit) can reclaim it.
            if destroy is not None:
                destroy(handle)
                self._module_handle = None
        except Exception:
            pass

    @property
    def is_capturing(self) -> bool:
        """True only while the capture thread is actually running (not merely requested)."""
        if not (self._is_capturing and self._module_handle):
            return False
        is_running = getattr(self, '_is_running', None)
        if is_running is None:
            return False
        return bool(is_running(self._module_handle))

    def start_capture(
        self,
        settings: AudioCaptureSettings,
        chunk_callback: Callable,
    ):
        """Starts capture. chunk_callback is a plain function (result_ptr, user_data)."""
        if self._is_capturing:
            self.stop_capture()

        if not callable(chunk_callback):
            raise TypeError("The provided 'chunk_callback' must be a callable function.")

        # Re-assign the c_char_p field with our own retained copy so the pointer
        # handed to C++ refers to a buffer we keep alive (the caller may drop theirs).
        name = settings.device_name
        self._device_name_buf = name
        settings.device_name = name

        # Record whether this session uses the zero-copy hand-off. Default (False)
        # keeps the copy-and-free path.
        self._deferred_free = bool(getattr(settings, 'deferred_free', False))

        self._python_callback = chunk_callback
        self._c_callback = AudioChunkCallback(self._internal_c_callback)
        _start_capture(self._module_handle, settings, self._c_callback, None)
        self._is_capturing = True

    def stop_capture(self):
        """Stops the audio capture process if it is running."""
        if self._is_capturing:
            # _stop_capture joins the C++ capture thread before returning, so it
            # is safe to release the pinned device-name buffer afterwards.
            _stop_capture(self._module_handle)
            self._is_capturing = False
            self._deferred_free = False
            self._python_callback = None
            self._c_callback = None
            self._device_name_buf = None

    def _internal_c_callback(self, result_ptr, user_data):
        # Free the buffer here unless an OwnedAudioFrame took it (take() nulls
        # result.data; a non-null pointer means nobody took it). Runs on the C++
        # thread, possibly at interpreter shutdown, so use the instance-bound free
        # function and guard it rather than risk a TypeError there.
        try:
            cb = self._python_callback
            if cb:
                cb(result_ptr, user_data)
        finally:
            # result_ptr.contents.data is NULL once an OwnedAudioFrame took the buffer.
            if result_ptr.contents.data:
                free_result_data = getattr(self, '_free_result_data', None)
                if free_result_data is not None:
                    free_result_data(result_ptr)

    def update_audio_bitrate(self, new_bitrate: int):
        """Updates the Opus bitrate during an active capture session."""
        # Gate on the is_capturing property (confirms the C++ thread is alive) so we
        # refuse on a session whose thread already died, not the stale flag.
        if not self.is_capturing:
            raise RuntimeError("Cannot update bitrate when capture is not active.")
        _update_bitrate(self._module_handle, new_bitrate)
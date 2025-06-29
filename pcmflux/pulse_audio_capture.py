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
    ]

class AudioChunkEncodeResult(ctypes.Structure):
    """Maps to the C++ AudioChunkEncodeResult struct."""
    _fields_ = [
        ("size", ctypes.c_int),
        ("data", ctypes.POINTER(ctypes.c_ubyte)),
    ]

# Defines the function signature for the callback passed to the C++ library.
AudioChunkCallback = ctypes.CFUNCTYPE(
    None, ctypes.POINTER(AudioChunkEncodeResult), ctypes.c_void_p
)


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

    return lib

# Load the library and assign functions to module-level variables.
_lib = _load_shared_library()
_create_module = _lib.create_audio_capture_module
_destroy_module = _lib.destroy_audio_capture_module
_start_capture = _lib.start_audio_capture
_stop_capture = _lib.stop_audio_capture
_free_result_data = _lib.free_audio_chunk_encode_result_data


# --- Main Python Wrapper Class ---

class AudioCapture:
    """A Pythonic wrapper for the C++ audio capture module."""

    def __init__(self):
        self._module_handle = _create_module()
        if not self._module_handle:
            raise RuntimeError("Failed to create the underlying audio capture module.")

        # Store the C callback object to prevent it from being garbage collected.
        self._c_callback = None
        self._python_callback = None
        self._is_capturing = False

    def __del__(self):
        """Ensures resources are released when the object is destroyed."""
        if hasattr(self, '_module_handle') and self._module_handle:
            self.stop_capture()
            _destroy_module(self._module_handle)
            self._module_handle = None

    @property
    def is_capturing(self) -> bool:
        """Returns True if audio capture is currently active."""
        return self._is_capturing

    def start_capture(
        self,
        settings: AudioCaptureSettings,
        chunk_callback: Callable[[ctypes.POINTER(AudioChunkEncodeResult), ctypes.c_void_p], None],
    ):
        """Starts the audio capture process."""
        if self._is_capturing:
            self.stop_capture()

        if not callable(chunk_callback):
            raise TypeError("The provided 'chunk_callback' must be a callable function.")

        self._python_callback = chunk_callback
        self._c_callback = AudioChunkCallback(self._internal_c_callback)
        _start_capture(self._module_handle, settings, self._c_callback, None)
        self._is_capturing = True

    def stop_capture(self):
        """Stops the audio capture process if it is running."""
        if self._is_capturing:
            _stop_capture(self._module_handle)
            self._is_capturing = False
            self._python_callback = None
            self._c_callback = None

    def _internal_c_callback(self, result_ptr, user_data):
        """
        Internal callback that bridges C++ calls to the user's Python function.

        This method ensures that the memory allocated by the C++ module is
        always freed, even if the user's Python callback raises an exception.
        """
        if self._python_callback:
            try:
                self._python_callback(result_ptr, user_data)
            finally:
                # The C++ module allocates the result data; this wrapper is
                # responsible for ensuring it is freed via the C API.
                _free_result_data(result_ptr)

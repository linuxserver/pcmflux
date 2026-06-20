"""pcmflux: PulseAudio -> Opus capture as a native CPython extension.

The capture/encode core and the zero-copy result type live in the compiled
``pcmflux._capture`` module. This package exposes them plus a plain-Python
settings holder and interpreter-exit safety.
"""

import atexit
import weakref

from ._capture import AudioCapture as _AudioCapture, AudioFrame

__all__ = ["AudioCapture", "AudioCaptureSettings", "AudioFrame"]


class AudioCaptureSettings:
    """Capture/encode configuration. Defaults mirror the C++ defaults.

    Set fields as attributes, e.g. ``s = AudioCaptureSettings(); s.device_name = ...``.
    The native ``AudioCapture.start_capture(settings, callback)`` reads these 11
    fields by attribute name.
    """

    __slots__ = (
        "device_name",       # str | bytes | None (None / "" => system default)
        "sample_rate",       # Hz
        "channels",          # 1 mono / 2 stereo
        "opus_bitrate",      # bits/sec
        "frame_duration_ms", # one of 5/10/20/40/60
        "use_vbr",
        "use_silence_gate",
        "debug_logging",
        "latency_ms",
        "omit_audio_header", # False => C++ prepends 2-byte header [0x01,0x00]
        "deferred_free",     # ignored by the C-API (it always takes ownership)
    )

    def __init__(self):
        self.device_name = None
        self.sample_rate = 48000
        self.channels = 2
        self.opus_bitrate = 128000
        self.frame_duration_ms = 20
        self.use_vbr = True
        self.use_silence_gate = True
        self.debug_logging = False
        self.latency_ms = 0
        self.omit_audio_header = False
        self.deferred_free = False


# Track live captures so atexit can stop them deterministically: __del__ ordering
# at interpreter shutdown is unreliable, and a still-running C++ thread would
# otherwise block/abort. WeakSet => no instance is kept alive by registration (the
# AudioCapture subclass is weak-referenceable: it has no __slots__, so it carries __weakref__).
_live_captures = weakref.WeakSet()


class AudioCapture(_AudioCapture):
    """Native AudioCapture, registered for interpreter-exit cleanup."""

    def start_capture(self, settings, callback):
        super().start_capture(settings, callback)
        _live_captures.add(self)


@atexit.register
def _stop_all_captures():
    # Snapshot to a list first: a WeakSet entry can be GC'd mid-iteration. (stop_capture
    # itself does not remove from the set; weakref finalization does when an instance dies.)
    for cap in list(_live_captures):
        try:
            cap.stop_capture()
        except Exception:
            pass

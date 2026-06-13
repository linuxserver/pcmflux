# pcmflux

pcmflux is a high-performance audio capture and encoding module for Python.

It is designed to capture system audio using PulseAudio, encode it into the Opus format, and stream it with low latency. A key optimization is its ability to detect and discard silent audio chunks, significantly reducing network traffic and CPU usage during periods of no sound.

## Prerequisites

This package compiles a C++ extension and requires the development headers for PulseAudio and Opus to be installed on your system.

On Debian/Ubuntu, you can install them with:
```bash
sudo apt-get install libpulse-dev libopus-dev
```

## Core Features

- **PulseAudio Capture:** Uses the `pa_simple` API for efficient, low-level audio capture.
- **Opus Encoding:** Integrates the high-quality, low-latency Opus codec.
- **Silence Detection:** Intelligently skips encoding and sending silent audio chunks.
- **Native Audio Header:** With `omit_audio_header=False` (the default), the encoder prepends a 2-byte `[0x01, 0x00]` header to each chunk natively, so WebSocket transports avoid an extra Python copy. Set it to `True` for raw Opus (WebRTC/RTP).
- **Zero-copy Frames:** With `deferred_free=True`, encoded chunks are exposed via `OwnedAudioFrame`, avoiding the C-to-Python copy of the Opus data. On Python 3.12+ (PEP 688 buffer protocol) a `memoryview` of the frame keeps its buffer alive until every view is released, so it can be sent with no copy; `close()` is guarded against use-after-free while a view is live. (Mirrors pixelflux's `OwnedFrame`.)
- **Tunable Capture:** Configurable `latency_ms`, validated `frame_duration_ms` (5/10/20/40/60 ms, default 20), VBR/CBR, and a toggleable silence gate.
- **Live Bitrate Updates:** Thread-safe `update_audio_bitrate()` adjusts the Opus bitrate during an active session.
- **Python `ctypes` Wrapper:** Provides a clean and simple Python API over a high-performance C++ core.
- **Python Build System:** Uses a robust Python build setup for compiling the C++ module and its dependencies.

## Example Usage

The `example` directory contains a standalone demo that captures system audio, broadcasts it over a WebSocket, and plays it back in a web browser using the WebCodecs API.

To run the example:

1.  Install the module: `pip3 install .`
2.  Run the server: `cd example && python3 audio_to_browser.py`
3.  Open `http://localhost:9001` in a modern web browser (Chrome, Edge, etc.).

The example client (`index.html`) strips the 2-byte `[0x01, 0x00]` header before decoding, and its `FRAME_DURATION_US` constant must match the server's `frame_duration_ms` (the value is not announced over the wire).

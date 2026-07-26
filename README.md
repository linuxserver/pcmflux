# pcmflux

[![PyPI version](https://badge.fury.io/py/pcmflux.svg)](https://badge.fury.io/py/pcmflux)
[![License: MPL 2.0](https://img.shields.io/badge/License-MPL%202.0-brightgreen.svg)](https://opensource.org/licenses/MPL-2.0)
[![Docs](https://img.shields.io/badge/docs-GitHub%20Pages-blue)](https://linuxserver.github.io/pcmflux/)

pcmflux is a high-performance audio capture and encoding module for Python.

It is designed to capture system audio using PulseAudio, encode it into the Opus format, and stream it with low latency. A key optimization is its ability to detect and discard silent audio chunks, significantly reducing network traffic and CPU usage during periods of no sound.

## Prerequisites

This package builds a native Rust extension (via `setuptools-rust`/PyO3). It requires a Rust toolchain (`cargo`/`rustc`) plus the development headers for PulseAudio and Opus on your system.

On Debian/Ubuntu, you can install them with:
```bash
sudo apt-get install libpulse-dev libopus-dev
```

## Core Features

- **PulseAudio Capture:** Captures system audio via PulseAudio using the asynchronous `Context`/`Stream` record API with a manually-pumped mainloop.
- **Opus Encoding:** Integrates the high-quality, low-latency Opus codec.
- **Silence Detection:** Intelligently skips encoding and sending silent audio chunks.
- **Native Audio Header:** With `omit_audio_header=False` (the default), the encoder prepends a 2-byte `[0x01, 0x00]` header to each chunk natively, so WebSocket transports avoid an extra Python copy. Set it to `True` for raw Opus (WebRTC/RTP).
- **Optional RED redundancy (RFC 2198):** `red_distance` (0–4, default 0) prepends redundant copies of recent Opus payloads for lossy/unreliable transports; `0` disables it (the default for reliable WebSocket/TCP).
- **Zero-copy Frames:** Each callback receives a native `AudioFrame` that owns the encoded chunk and supports the buffer protocol — `bytes(frame)` / `memoryview(frame)` / `len(frame)` — on **every supported Python version (3.9–3.14)**. `memoryview(frame)` aliases the buffer with no copy, and the frame keeps it alive until every view is released, so the hand-off is memory-safe.
- **Tunable Capture:** Configurable `latency_ms`, validated `frame_duration_ms` (2.5/5/10/20/40/60 ms, default 20), VBR/CBR, and a toggleable silence gate.
- **Multichannel Opus:** Mono, stereo, and 5.1 / 7.1 surround (via the Opus multistream API with Chromium-compatible channel layouts); `channels` accepts 1, 2, 6, or 8.
- **Mic-Uplink Playback:** An `AudioPlayback` class decodes an inbound Opus stream (with optional RED recovery via `write_red`) and plays it into a PulseAudio sink — the reverse of capture, for client microphone audio. Playback is mono/stereo.
- **Live Bitrate Updates:** Thread-safe `update_audio_bitrate()` adjusts the Opus bitrate during an active session.
- **PyO3 Extension Module:** A native Rust `pcmflux` extension module (full CPython API, not Limited/abi3) provides PulseAudio capture + Opus encoding.
- **Python Build System:** Uses `setuptools-rust` to build and package the `pcmflux` PyO3 extension.

## Usage

`AudioCapture.start_capture(settings, callback)` spawns a capture thread and
invokes `callback(frame)` once per *encoded* chunk. When the silence gate is on
(`use_silence_gate=True`, the default), silent chunks are dropped before
encoding and the callback is simply **not** called for them — it never receives
an empty frame, so there's no silence to filter out. The `frame` is a zero-copy
`AudioFrame` (buffer protocol + a `.pts` presentation timestamp in samples).
Copy it out with `bytes(frame)` if it must outlive the callback, or pass
`memoryview(frame)` for a zero-copy hand-off (keep the frame referenced for the
duration of the send so its buffer stays alive).

```python
from pcmflux import AudioCapture, AudioCaptureSettings

def on_chunk(frame):
    # Silence-gated chunks are never delivered (the callback is skipped for
    # them), so there's no empty/"silence" frame to filter out here.
    data = bytes(frame)          # copy out (header+Opus, or raw Opus per settings)
    pts = frame.pts              # presentation timestamp, in samples
    # send `data` to your client...

settings = AudioCaptureSettings()
settings.device_name = None      # None / "" => system default source
settings.frame_duration_ms = 20  # one of 2.5/5/10/20/40/60

capture = AudioCapture()
capture.start_capture(settings, on_chunk)
# capture.update_audio_bitrate(96000)  # adjust the Opus bitrate while running
# ...
capture.stop_capture()
```

### API notes

- `update_audio_bitrate(bps)` stores the new Opus bitrate
  atomically; the capture thread re-reads it on the next frame, so it only takes
  effect during an **active** capture session. Calling it while no capture is
  active is **not** an error — it is a silent no-op store, but the value does
  **not** persist into the next session: the next `start_capture(settings, ...)`
  snapshots the passed settings object and re-seeds the atomic bitrate mirror from
  `settings.opus_bitrate`. To change the bitrate for a new
  session, set `settings.opus_bitrate` before `start_capture()`; use
  `update_audio_bitrate()` only to adjust a session that is already running.

## Example Usage

The `example` directory contains a standalone demo that captures system audio, broadcasts it over a WebSocket, and plays it back in a web browser using the WebCodecs API.

To run the example:

1.  Install the module: `pip3 install .`
2.  Run the server: `cd example && python3 audio_to_browser.py`
3.  Open `http://localhost:9001` in a modern web browser (Chrome, Edge, etc.).

The example client (`index.html`) strips the 2-byte `[0x01, 0x00]` header before decoding, and its `FRAME_DURATION_US` constant must match the server's `frame_duration_ms` (the value is not announced over the wire).

## License

This project is licensed under the **Mozilla Public License Version 2.0**.
A copy of the MPL 2.0 can be found at https://mozilla.org/MPL/2.0/.

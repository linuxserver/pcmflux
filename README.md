# pcmflux

pcmflux is a high-performance audio capture and encoding module for Python.

It is designed to capture system audio using PulseAudio, encode it into the Opus format, and stream it with low latency. A key optimization is its ability to detect and discard silent audio chunks, significantly reducing network traffic and CPU usage during periods of no sound.

## Core Features

- **PulseAudio Capture:** Uses the `pa_simple` API for efficient, low-level audio capture.
- **Opus Encoding:** Integrates the high-quality, low-latency Opus codec.
- **Silence Detection:** Intelligently skips encoding and sending silent audio chunks.
- **Python `ctypes` Wrapper:** Provides a clean and simple Python API over a high-performance C++ core.
- **CMake Build System:** Uses a robust CMake setup for compiling the C++ module and its dependencies.

## Example Usage

The `example` directory contains a standalone demo that captures system audio, broadcasts it over a WebSocket, and plays it back in a web browser using the WebCodecs API.

To run the example:

1.  Install the module: `python3 setup.py install`
2.  Run the server: `python3 example/audio_to_browser.py`
3.  Open `http://localhost:9001` in a modern web browser (Chrome, Edge, etc.).

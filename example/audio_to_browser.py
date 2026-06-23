# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

import asyncio
import mimetypes
import os
import urllib.parse
import websockets
import websockets.asyncio.server as ws_async

from pcmflux import AudioCapture, AudioCaptureSettings

# --- Global Shared Context ---
# These variables manage the server's shared state across different asynchronous
# tasks and threads.
g_loop = None           # The main asyncio event loop.
g_settings = None       # The audio capture configuration.
g_callback = None       # The C-compatible callback function pointer.
g_module = None         # The pcmflux.AudioCapture module instance.
g_clients = set()       # A set of currently connected WebSocket clients.
g_is_capturing = False  # A flag to track the audio capture state.
g_audio_queue = None    # A bounded asyncio.Queue for passing audio between threads.
g_send_task = None      # The asyncio.Task that broadcasts audio to clients.
g_status_task = None    # The asyncio.Task that periodically logs queue/drop stats.
g_dropped = 0           # Audio frames dropped because the queue was full.
# --- End Global Context ---

# ~4s of 20ms frames; bounds memory if a client stalls and stops draining.
AUDIO_QUEUE_MAXSIZE = 200

async def send_audio_chunks():
    """
    An asynchronous task that runs continuously to broadcast audio.

    It retrieves encoded Opus audio chunks from the thread-safe queue and sends
    them to all currently connected WebSocket clients concurrently.
    """
    global g_audio_queue, g_clients
    print("Audio chunk broadcasting task started.")
    try:
        while True:
            # Wait for an Opus chunk to arrive from the audio capture thread.
            opus_bytes = await g_audio_queue.get()

            # If no clients are connected, just clear the queue item and wait.
            if not g_clients:
                g_audio_queue.task_done()
                continue

            # pcmflux prepends the 2-byte header [0x01, 0x00] natively
            # (omit_audio_header=False), so forward as-is (no per-frame Python copy).
            message_to_send = opus_bytes

            # Fire-and-forget fan-out: writes into each client's buffer without
            # awaiting per-connection backpressure, so one slow client can't
            # stall delivery to the others. Skips non-open connections.
            ws_async.broadcast(g_clients, message_to_send)

            g_audio_queue.task_done()
    except asyncio.CancelledError:
        print("Audio chunk broadcasting task cancelled.")
    finally:
        print("Audio chunk broadcasting task finished.")

async def status_logger():
    """Periodically logs queue depth and dropped-frame count so backpressure
    drops are visible rather than silent."""
    try:
        while True:
            await asyncio.sleep(5)
            q = g_audio_queue
            print(f"[server] queued={q.qsize() if q else 0}, "
                  f"dropped={g_dropped}, clients={len(g_clients)}")
    except asyncio.CancelledError:
        pass

async def health_check(connection, request):
    """
    A pre-processor for incoming connections to the WebSocket port.

    This function intercepts plain HTTP requests, which browsers often send
    (e.g., for /favicon.ico), and handles them gracefully. This prevents
    WebSocket handshake errors from cluttering the console.
    """
    if request.path == "/favicon.ico":
        # Return a "204 No Content" response for favicon requests.
        return connection.respond(204, headers=[], body=b"")
    # Allow all other requests to proceed to the WebSocket handler.
    return None

async def ws_handler(websocket, path=None):
    """
    Handles the lifecycle of each WebSocket client connection.

    This function is responsible for starting the audio capture when the first
    client connects and stopping it when the last client disconnects, ensuring
    that system resources are only used when needed.
    """
    global g_clients, g_is_capturing, g_audio_queue, g_module, g_send_task
    global g_settings, g_callback, g_status_task

    # Register the new client.
    g_clients.add(websocket)
    print(f"Client connected: {websocket.remote_address}. "
          f"Total clients: {len(g_clients)}")

    # If this is the first client, start the audio capture process.
    if not g_is_capturing and g_module:
        print("First client connected. Starting audio capture...")
        g_audio_queue = asyncio.Queue(maxsize=AUDIO_QUEUE_MAXSIZE)
        g_module.start_capture(g_settings, g_callback)
        g_is_capturing = True

        # Ensure the broadcasting and status tasks are running.
        if g_send_task is None or g_send_task.done():
            g_send_task = asyncio.create_task(send_audio_chunks())
        if g_status_task is None or g_status_task.done():
            g_status_task = asyncio.create_task(status_logger())
        print("Audio capture process initiated.")

    try:
        # Wait for messages from the client. In this demo, we don't expect
        # any, so this loop effectively just waits for the client to close.
        async for _ in websocket:
            pass
    except websockets.exceptions.ConnectionClosed:
        pass
    finally:
        # Unregister the client upon disconnection.
        if websocket in g_clients:
            g_clients.remove(websocket)
        print(f"Client disconnected. Remaining clients: {len(g_clients)}")

        # If this was the last client, stop the audio capture to save resources.
        if g_is_capturing and not g_clients and g_module:
            print("Last client disconnected. Stopping audio capture...")
            g_module.stop_capture()
            g_is_capturing = False
            if g_send_task:
                g_send_task.cancel()
                g_send_task = None
            if g_status_task:
                g_status_task.cancel()
                g_status_task = None
            g_audio_queue = None
            print("Audio capture process stopped.")

def py_audio_callback(frame):
    """Per-chunk callback invoked from the native capture thread.

    `frame` is a zero-copy AudioFrame (buffer protocol + `.pts`). Copy it to
    `bytes` here so nothing outlives this call, then hand off to the loop thread
    for a non-blocking, drop-oldest enqueue. Silence-gated chunks are never
    delivered (the callback is skipped for them), so `frame` always carries an
    encoded payload — no empty-frame check is needed.
    """
    global g_is_capturing, g_audio_queue, g_loop

    if g_is_capturing and frame is not None and g_audio_queue is not None:
        # bytes(frame) copies the payload (header+Opus, or raw Opus per settings).
        data_bytes = bytes(frame)
        if g_loop and not g_loop.is_closed():
            g_loop.call_soon_threadsafe(_enqueue_audio, data_bytes)

def _enqueue_audio(data_bytes):
    """Enqueue one chunk on the loop thread, dropping the oldest if the bounded
    queue is full, so a stalled client can't grow memory without bound."""
    global g_dropped
    q = g_audio_queue
    if q is None:
        return
    if q.full():
        try:
            q.get_nowait()
            q.task_done()  # keep unfinished-task accounting balanced
            g_dropped += 1
        except asyncio.QueueEmpty:
            pass
    try:
        q.put_nowait(data_bytes)
    except asyncio.QueueFull:
        pass

def _resolve_static_path(script_dir, request_path):
    """Return the real path of request_path under script_dir, or None if it
    escapes. realpath canonicalizes '..'/symlinks and the root+os.sep boundary
    rejects sibling-prefix dirs; the path is URL-decoded first."""
    decoded = urllib.parse.unquote(request_path)
    root = os.path.realpath(script_dir)
    try:
        requested = os.path.realpath(os.path.join(root, decoded.lstrip('/')))
    except ValueError:
        return None  # e.g. embedded NUL byte ("%00")
    if requested != root and not requested.startswith(root + os.sep):
        return None
    return requested

async def handle_http_request(reader, writer):
    """Handle HTTP requests by serving static files from the script directory."""
    try:
        request_line = await reader.readline()
        if not request_line:
            return

        parts = request_line.split()
        if len(parts) < 2 or parts[0] != b'GET':
            writer.write(b'HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
            return

        path = parts[1].decode()
        if path == '/':
            path = '/index.html'

        script_dir = os.path.dirname(os.path.abspath(__file__))
        full_path = _resolve_static_path(script_dir, path)

        # Security check: reject directory traversal / escapes outside script_dir.
        if full_path is None:
            writer.write(b'HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
            return

        if os.path.isfile(full_path):
            with open(full_path, 'rb') as f:
                content = f.read()

            content_type = mimetypes.guess_type(full_path)[0] or 'application/octet-stream'

            headers = f'HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {len(content)}\r\nConnection: close\r\n\r\n'
            writer.write(headers.encode())
            writer.write(content)
        else:
            writer.write(b'HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')

    except Exception as e:
        print(f"[HTTP Error] {e}")
        writer.write(b'HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
    finally:
        # The client may already be gone; draining/closing a dead connection
        # must not raise out of the handler.
        try:
            await writer.drain()
        except (ConnectionError, OSError):
            pass
        writer.close()

async def main_async():
    """The main routine to initialize and run the servers."""
    global g_loop, g_settings, g_callback, g_module

    g_loop = asyncio.get_running_loop()

    # --- Configure Audio Capture Parameters ---
    g_settings = AudioCaptureSettings()
    # To capture desktop audio on Linux with PulseAudio, you may need to find
    # the name of your output's ".monitor" source. Use `pactl list sources`
    # in a terminal to find available source names.
    # To use the system's default microphone, set device_name to None or b''.
    # The same variables selkies accepts override the template values here (the
    # library reads no SELKIES_* environment itself — each knob is a settings field).
    g_settings.device_name = os.environ.get(
        "SELKIES_AUDIO_DEVICE_NAME",
        "alsa_output.pci-0000_2b_00.1.hdmi-stereo.monitor",
    ).encode("utf-8")
    #g_settings.device_name = None
    g_settings.sample_rate = 48000
    g_settings.channels = int(os.environ.get("SELKIES_AUDIO_CHANNELS", "2"))
    g_settings.opus_bitrate = int(os.environ.get("SELKIES_AUDIO_BITRATE", "128000"))
    g_settings.frame_duration_ms = int(
        os.environ.get("SELKIES_AUDIO_FRAME_DURATION_MS", "20")
    )
    g_settings.use_vbr = True
    g_settings.use_silence_gate = False
    g_settings.debug_logging = True
    # Emit the 2-byte audio header [0x01, 0x00] natively in the extension (the default).
    # Set True only for a raw-Opus/WebRTC transport that adds no header.
    g_settings.omit_audio_header = False
    # --- End Configuration ---

    # Pass the plain Python callback; the wrapper marshals it once internally.
    g_callback = py_audio_callback
    g_module = AudioCapture()
    print("pcmflux audio capture module initialized.")

    # Start HTTP server using asyncio
    http_server = await asyncio.start_server(
        handle_http_request, 'localhost', 9001
    )
    print("HTTP server is serving files from current directory")
    print("-> Open http://localhost:9001/index.html in your browser.")

    # Start the WebSocket server.
    ws_server = await ws_async.serve(
        ws_handler,
        'localhost',
        9000,
        process_request=health_check
    )
    print("WebSocket server started on ws://localhost:9000")

    global g_send_task, g_status_task
    try:
        # Keep the main coroutine running indefinitely.
        await asyncio.Event().wait()
    except KeyboardInterrupt:
        pass
    finally:
        # Stop capture first (joins the native capture thread, stops enqueuing), then cancel
        # the consumer tasks, then close the servers.
        print("\nShutting down...")
        if g_module:
            g_module.stop_capture()
        for task in (g_send_task, g_status_task):
            if task:
                task.cancel()
        # Await the cancellations together; CancelledError is expected per task.
        pending = [t for t in (g_send_task, g_status_task) if t]
        if pending:
            await asyncio.gather(*pending, return_exceptions=True)
        g_send_task = None
        g_status_task = None
        for server in (ws_server, http_server):
            if server:
                server.close()
                await server.wait_closed()
        # Drop the last reference so AudioCapture.__del__ releases the native capture resources.
        if g_module:
            g_module = None
        print("Cleanup complete.")

if __name__ == "__main__":
    try:
        asyncio.run(main_async())
    except KeyboardInterrupt:
        print("\nApplication exiting.")

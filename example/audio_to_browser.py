import asyncio
import ctypes
import http.server
import os
import socketserver
import threading
import websockets

from pcmflux import AudioCapture, AudioCaptureSettings, AudioChunkCallback

# --- Global Shared Context ---
# These variables manage the server's shared state across different asynchronous
# tasks and threads.
g_loop = None           # The main asyncio event loop.
g_settings = None       # The audio capture configuration.
g_callback = None       # The C-compatible callback function pointer.
g_module = None         # The pcmflux.AudioCapture module instance.
g_clients = set()       # A set of currently connected WebSocket clients.
g_is_capturing = False  # A flag to track the audio capture state.
g_audio_queue = None    # An asyncio.Queue for passing audio data between threads.
g_send_task = None      # The asyncio.Task that broadcasts audio to clients.
# --- End Global Context ---

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

            # We define a simple protocol: a 1-byte header (0x01) indicates
            # that the payload is an Opus audio chunk.
            message_to_send = b'\x01' + opus_bytes

            # Broadcast the message to all clients concurrently.
            active_clients = list(g_clients)
            tasks = [client.send(message_to_send) for client in active_clients]
            if tasks:
                # asyncio.gather runs all send operations in parallel.
                # `return_exceptions=True` prevents one failed send (e.g., a
                # disconnected client) from stopping the entire broadcast.
                await asyncio.gather(*tasks, return_exceptions=True)

            g_audio_queue.task_done()
    except asyncio.CancelledError:
        print("Audio chunk broadcasting task cancelled.")
    finally:
        print("Audio chunk broadcasting task finished.")

async def health_check(path, request_headers):
    """
    A pre-processor for incoming connections to the WebSocket port.

    This function intercepts plain HTTP requests, which browsers often send
    (e.g., for /favicon.ico), and handles them gracefully. This prevents
    WebSocket handshake errors from cluttering the console.
    """
    if path == "/favicon.ico":
        # Return a "204 No Content" response for favicon requests.
        return http.HTTPStatus.NO_CONTENT, [], b""
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
    global g_settings, g_callback

    # Register the new client.
    g_clients.add(websocket)
    print(f"Client connected: {websocket.remote_address}. "
          f"Total clients: {len(g_clients)}")

    # If this is the first client, start the audio capture process.
    if not g_is_capturing and g_module:
        print("First client connected. Starting audio capture...")
        g_audio_queue = asyncio.Queue()
        g_module.start_capture(g_settings, g_callback)
        g_is_capturing = True

        # Ensure the broadcasting task is running.
        if g_send_task is None or g_send_task.done():
            g_send_task = asyncio.create_task(send_audio_chunks())
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
            g_audio_queue = None
            print("Audio capture process stopped.")

def py_audio_callback(result_ptr, user_data):
    """
    A C-style callback function that bridges the C++ and Python worlds.

    This function is not called directly by Python. It is passed as a function
    pointer to the C++ `pcmflux` library, which calls it from a separate
    thread whenever a new Opus audio chunk is encoded.
    """
    global g_is_capturing, g_audio_queue, g_loop

    if g_is_capturing and result_ptr and g_audio_queue is not None:
        # Dereference the C pointer to access the result struct.
        result = result_ptr.contents
        if result.data and result.size > 0:
            # Convert the raw C data (unsigned char*) into a Python `bytes` object.
            data_bytes = bytes(ctypes.cast(
                result.data, ctypes.POINTER(ctypes.c_ubyte * result.size)
            ).contents)

            # Since this callback runs in a different thread, we must use a
            # thread-safe method to put data onto the asyncio queue.
            if g_loop and not g_loop.is_closed():
                asyncio.run_coroutine_threadsafe(
                    g_audio_queue.put(data_bytes), g_loop)

    # The pcmflux Python wrapper automatically frees the underlying C++ memory
    # after this function returns.

def start_http_server(port=9001):
    """
    Serves the demo's HTML file (`index.html`) on the specified port.

    This runs in a separate thread so it does not block the main asyncio loop.
    """
    script_dir = os.path.dirname(os.path.abspath(__file__))

    class Handler(http.server.SimpleHTTPRequestHandler):
        def __init__(self, *args, **kwargs):
            super().__init__(*args, directory=script_dir, **kwargs)
        # Suppress the default HTTP request logging to keep the console clean.
        def log_message(self, format, *args):
            pass

    with socketserver.TCPServer(("localhost", port), Handler) as httpd:
        print(f"HTTP server serving on http://localhost:{port}/index.html")
        httpd.serve_forever()

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
    g_settings.device_name = b"alsa_output.pci-0000_2b_00.1.hdmi-stereo.monitor"
    #g_settings.device_name = None
    g_settings.sample_rate = 48000
    g_settings.channels = 2
    g_settings.opus_bitrate = 128000
    g_settings.frame_duration_ms = 20
    # --- End Configuration ---

    # Create the C-compatible callback object.
    g_callback = AudioChunkCallback(py_audio_callback)
    g_module = AudioCapture()
    print("pcmflux audio capture module initialized.")

    # Start the simple HTTP server in a daemon thread.
    http_thread = threading.Thread(target=start_http_server, daemon=True)
    http_thread.start()

    # Start the WebSocket server.
    ws_server = await websockets.serve(
        ws_handler,
        'localhost',
        9000,
        process_request=health_check
    )
    print("WebSocket server started on ws://localhost:9000")

    try:
        # Keep the main coroutine running indefinitely.
        await asyncio.Event().wait()
    except KeyboardInterrupt:
        pass
    finally:
        # Perform a graceful shutdown on Ctrl+C.
        print("\nShutting down...")
        if g_is_capturing and g_module:
            g_module.stop_capture()
        if g_send_task:
            g_send_task.cancel()
        ws_server.close()
        await ws_server.wait_closed()
        if g_module:
            del g_module
        print("Cleanup complete.")

if __name__ == "__main__":
    try:
        asyncio.run(main_async())
    except KeyboardInterrupt:
        print("\nApplication exiting.")

/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */


/*
       ▐▘▜     
▛▌▛▘▛▛▌▜▘▐ ▌▌▚▘
▙▌▙▖▌▌▌▐ ▐▖▙▌▞▖
▌              
*/

#include <atomic>
#include <chrono>
#include <cstdint>
#include <cstring>
#include <iomanip>
#include <iostream>
#include <memory>
#include <mutex>
#include <stdexcept>
#include <string>
#include <thread>
#include <vector>

#include <opus/opus.h>
#include <pulse/error.h>
#include <pulse/simple.h>

/**
 * @brief Holds settings for audio capture and encoding.
 * This struct aggregates all configurable parameters for the audio capture process,
 * including the PulseAudio device, sample rate, channels, and Opus encoder settings.
 */
struct AudioCaptureSettings {
  const char* device_name;
  uint32_t sample_rate;
  int channels;
  int opus_bitrate;
  int frame_duration_ms;
  bool use_vbr;
  bool use_silence_gate;
  bool debug_logging;
  int latency_ms;
  // Append-only fields (keep ABI offsets above stable). Mirror pixelflux.
  // false (default) = C++ prepends the 2-byte header [0x01,0x00] (WebSocket);
  // true = raw Opus, no header (WebRTC).
  bool omit_audio_header;
  // false (default) = C++ callback frees result.data; true = ownership handed to
  // Python (zero-copy OwnedAudioFrame) and the callback must NOT free it.
  bool deferred_free;

  /**
   * @brief Default constructor for AudioCaptureSettings.
   * Initializes settings with common default values (48kHz, stereo, 128kbps, VBR, Silence Gate on).
   */
  AudioCaptureSettings()
    : device_name(nullptr),
      sample_rate(48000),
      channels(2),
      opus_bitrate(128000),
      frame_duration_ms(20),
      use_vbr(true),
      use_silence_gate(true),
      debug_logging(false),
      latency_ms(0),
      omit_audio_header(false),
      deferred_free(false) {}

  /**
   * @brief Parameterized constructor for AudioCaptureSettings.
   * Allows initializing all settings with specific values.
   * @param dev The name of the PulseAudio source device (monitor). Null for default.
   * @param sr The sample rate in Hz (e.g., 48000).
   * @param ch The number of channels (1 for mono, 2 for stereo).
   * @param br The target bitrate for the Opus encoder in bits per second.
   * @param dur The duration of each audio frame in milliseconds (e.g., 20, 40, 60).
   * @param vbr Flag to enable Variable Bitrate (true) or Constant Bitrate (false).
   * @param gate Flag to enable the silence detection gate (true) or disable it (false).
   * @param omit_hdr Flag to omit the native 2-byte audio header (true) or emit it (false).
   * @param deferred Flag to hand buffer ownership to Python (true) or free here (false).
   */
  AudioCaptureSettings(const char* dev, uint32_t sr, int ch, int br, int dur, bool vbr, bool gate, bool debug_logging, int lat,
                       bool omit_hdr = false, bool deferred = false)
    : device_name(dev),
      sample_rate(sr),
      channels(ch),
      opus_bitrate(br),
      frame_duration_ms(dur),
      use_vbr(vbr),
      use_silence_gate(gate),
      debug_logging(debug_logging),
      latency_ms(lat),
      omit_audio_header(omit_hdr),
      deferred_free(deferred) {}
};

/**
 * @brief Represents the result of encoding a single chunk of audio.
 * Contains the encoded Opus data and its size. This struct uses move semantics
 * for efficient data transfer, preventing unnecessary copies.
 */
struct AudioChunkEncodeResult {
  int size;
  unsigned char* data;
  uint64_t pts;

  /**
   * @brief Default constructor for AudioChunkEncodeResult.
   * Initializes members to default/null values.
   */
  AudioChunkEncodeResult() : size(0), data(nullptr), pts(0) {}

  /**
   * @brief Move constructor for AudioChunkEncodeResult.
   * Transfers ownership of the data buffer from the 'other' object.
   * @param other The AudioChunkEncodeResult to move from.
   */
  AudioChunkEncodeResult(AudioChunkEncodeResult&& other) noexcept
    : size(other.size), data(other.data), pts(other.pts) {
    other.size = 0;
    other.data = nullptr;
    other.pts = 0;
  }

  /**
   * @brief Move assignment operator for AudioChunkEncodeResult.
   * Transfers ownership of data, freeing any existing data in this object.
   * @param other The AudioChunkEncodeResult to move assign from.
   * @return Reference to this object.
   */
  AudioChunkEncodeResult& operator=(AudioChunkEncodeResult&& other) noexcept {
    if (this != &other) {
      delete[] data;
      size = other.size;
      data = other.data;
      pts = other.pts;
      other.size = 0;
      other.data = nullptr;
      other.pts = 0;
    }
    return *this;
  }

  // Frees the buffer if the consumer didn't. free_audio_chunk_encode_result_data
  // nulls data first, so this is a no-op on the normal path.
  ~AudioChunkEncodeResult() { delete[] data; }

private:
  // Disallow copy and copy assignment to prevent double-freeing the data buffer.
  AudioChunkEncodeResult(const AudioChunkEncodeResult&) = delete;
  AudioChunkEncodeResult& operator=(const AudioChunkEncodeResult&) = delete;
};

/**
 * @brief Callback function type for processing encoded audio chunks.
 * @param result Pointer to the AudioChunkEncodeResult with the encoded data.
 * @param user_data User-defined data passed to the callback.
 */
typedef void (*AudioChunkCallback)(AudioChunkEncodeResult* result,
                                   void* user_data);

extern "C" {
/**
 * @brief Frees the data buffer within an AudioChunkEncodeResult.
 * This function is intended to be called by the consumer of the
 * AudioChunkEncodeResult once the data is no longer needed.
 * @param result Pointer to the result whose data needs freeing.
 */
void free_audio_chunk_encode_result_data(AudioChunkEncodeResult* result);
}

// Legal Opus frame durations (ms) that are expressible as an int. opus_encode()
// only accepts 2.5/5/10/20/40/60 ms of the encoder rate; 2.5 isn't an int.
static bool is_valid_opus_frame_duration_ms(int ms) {
  return ms == 5 || ms == 10 || ms == 20 || ms == 40 || ms == 60;
}

/**
 * @brief Manages the audio capture process from PulseAudio and Opus encoding.
 * This class encapsulates the logic for capturing raw PCM audio, encoding it
 * into the Opus format, and invoking a callback with the encoded data. It
* supports dynamic modification of capture settings.
 */
class AudioCaptureModule {
public:
  std::atomic<bool> stop_requested;
  std::thread capture_thread;
  AudioChunkCallback chunk_callback = nullptr;
  void* user_data = nullptr;
  std::atomic<bool> started_ok{false};
  mutable std::mutex settings_mutex;
  AudioCaptureSettings current_settings;
  // Lock-free mirror of !omit_audio_header so the hot path reads it without the
  // mutex. (deferred_free needs no mirror; it's resolved entirely Python-side.)
  std::atomic<bool> emit_audio_header_{true};

  /**
   * @brief Default constructor for AudioCaptureModule.
   */
  AudioCaptureModule() : stop_requested(false) {}

  /**
   * @brief Destructor for AudioCaptureModule.
   * Ensures the capture thread is stopped and resources are released.
   */
  ~AudioCaptureModule() {
    stop_capture();
  }

  /**
   * @brief Starts the audio capture process in a new thread.
   * If a capture thread is already running, it is stopped first.
   */
  void start_capture() {
    if (capture_thread.joinable() &&
        capture_thread.get_id() == std::this_thread::get_id()) {
      // Re-entrant start from the callback (on the capture thread): can't
      // join/recreate the running thread, so just keep it running and undo any
      // stop_requested a nested stop_capture() set.
      stop_requested = false;
      return;
    }
    if (capture_thread.joinable()) {
      stop_capture();
    }
    stop_requested = false;
    started_ok = false;
    capture_thread = std::thread(&AudioCaptureModule::capture_loop, this);
  }

  /**
   * @brief Stops the audio capture process.
   * Sets a flag to signal the capture thread to terminate and waits for it to join.
   */
  void stop_capture() {
    stop_requested = true;
    // Don't join from within the capture thread itself (re-entrant stop from the
    // callback): join() on the current thread throws. stop_requested suffices.
    if (capture_thread.joinable() &&
        capture_thread.get_id() != std::this_thread::get_id()) {
      capture_thread.join();
    }
  }

  /**
   * @brief Modifies the audio capture settings.
   * This function is thread-safe. The new settings will be applied when the
   * capture loop is next started.
   * @param new_settings An AudioCaptureSettings struct with the new parameters.
   */
  void modify_settings(const AudioCaptureSettings& new_settings) {
    std::lock_guard<std::mutex> lock(settings_mutex);
    current_settings = new_settings;
    // Publish the header toggle to its lock-free mirror so the capture thread
    // reads it without taking the mutex each frame.
    emit_audio_header_.store(!new_settings.omit_audio_header, std::memory_order_relaxed);
  }

  // Atomically updates only the bitrate (under the mutex), so it can't lose a
  // concurrent modify_settings() the way a get/modify/set of the whole struct would.
  void set_bitrate(int new_bitrate) {
    std::lock_guard<std::mutex> lock(settings_mutex);
    current_settings.opus_bitrate = new_bitrate;
  }

  /**
   * @brief Retrieves the current audio capture settings.
   * This function is thread-safe.
   * @return An AudioCaptureSettings struct with the current settings.
   */
  AudioCaptureSettings get_current_settings() const {
    std::lock_guard<std::mutex> lock(settings_mutex);
    return current_settings;
  }

private:
  /**
   * @brief Main loop for the audio capture thread.
   * This loop handles:
   * - Connecting to the PulseAudio server and the specified source device.
   * - Initializing the Opus encoder with the configured settings.
   * - Continuously reading raw PCM audio chunks from PulseAudio.
   * - Detecting and skipping silent chunks to save encoding work (if enabled).
   * - Encoding non-silent audio chunks into the Opus format.
   * - Invoking the user-provided callback with the encoded data.
   * - Periodically logging capture and encoding statistics.
   * The loop runs until stop_requested is set to true, then cleans up all resources.
   */
  void capture_loop() {
    AudioCaptureSettings local_settings = get_current_settings();
    pa_simple* s = nullptr;
    OpusEncoder* encoder = nullptr;
    int pa_error;

    const pa_sample_spec ss = {.format = PA_SAMPLE_S16LE,
                               .rate = local_settings.sample_rate,
                               .channels = (uint8_t)local_settings.channels};

    pa_buffer_attr attr;
    attr.maxlength = (uint32_t)-1;
    attr.tlength = (uint32_t)-1;
    attr.prebuf = (uint32_t)-1;
    attr.minreq = (uint32_t)-1;
    attr.fragsize  = (uint32_t)-1;
    if (local_settings.latency_ms > 0) {
        // Cast before multiplying to avoid signed-int overflow (result is pa_usec_t).
        attr.fragsize = pa_usec_to_bytes((pa_usec_t)local_settings.latency_ms * 1000, &ss);
    }

    const char* device_to_use = local_settings.device_name;
    if (device_to_use && std::strlen(device_to_use) == 0) {
      device_to_use = nullptr;
    }

    std::cout << "[pcmflux] Attempting to connect to PulseAudio device: "
              << (device_to_use ? device_to_use : "system_default");
    if (local_settings.latency_ms > 0) {
        std::cout << " with latency: " << local_settings.latency_ms << "ms" << std::endl;
    } else {
        std::cout << " (default latency)" << std::endl;
    }

    s = pa_simple_new(NULL, "pcmflux", PA_STREAM_RECORD, device_to_use,
                      "Audio Capture", &ss, NULL, &attr, &pa_error);

    if (!s) {
      std::cerr << "[pcmflux] ERROR: pa_simple_new() failed: "
                << pa_strerror(pa_error) << std::endl;
      if (device_to_use) {
        std::cerr << "  (Could not find the device named: '" << device_to_use
                  << "')" << std::endl;
      }
      return;
    }
    std::cout << "[pcmflux] SUCCESS: Connected to PulseAudio." << std::endl;

    int opus_error;
    encoder = opus_encoder_create(local_settings.sample_rate,
                                  local_settings.channels,
                                  OPUS_APPLICATION_RESTRICTED_LOWDELAY,
                                  &opus_error);
    if (opus_error != OPUS_OK) {
      std::cerr << "[pcmflux] ERROR: opus_encoder_create() failed: "
                << opus_strerror(opus_error) << std::endl;
      pa_simple_free(s);
      return;
    }
    std::cout << "[pcmflux] SUCCESS: Opus encoder created." << std::endl;

    if (opus_encoder_ctl(encoder, OPUS_SET_BITRATE(local_settings.opus_bitrate)) != OPUS_OK) {
      std::cerr << "[pcmflux] WARNING: failed to apply initial bitrate ("
                << local_settings.opus_bitrate
                << "); encoder will use its default." << std::endl;
    }
    if (opus_encoder_ctl(encoder, OPUS_SET_VBR(local_settings.use_vbr ? 1 : 0)) != OPUS_OK) {
      std::cerr << "[pcmflux] WARNING: failed to apply VBR mode." << std::endl;
    }

    // An illegal duration makes opus_encode() fail on every frame; fail fast.
    if (!is_valid_opus_frame_duration_ms(local_settings.frame_duration_ms)) {
      std::cerr << "[pcmflux] ERROR: invalid frame_duration_ms ("
                << local_settings.frame_duration_ms
                << "). Must be one of 5, 10, 20, 40, 60." << std::endl;
      opus_encoder_destroy(encoder);
      pa_simple_free(s);
      return;
    }

    const int frame_size_per_channel =
        (local_settings.sample_rate * local_settings.frame_duration_ms) / 1000;
    const int pcm_chunk_size_bytes =
        frame_size_per_channel * local_settings.channels * sizeof(int16_t);
    std::vector<int16_t> pcm_buffer(frame_size_per_channel *
                                    local_settings.channels);
    const std::vector<int16_t> silence_ref(pcm_buffer.size(), 0);
    const int max_opus_packet_size = 4000;
    std::vector<unsigned char> opus_buffer(max_opus_packet_size);

    std::cout << "[pcmflux] Capture loop started. Device: "
              << (device_to_use ? device_to_use : "system_default")
              << ", Rate: " << local_settings.sample_rate
              << ", Channels: " << local_settings.channels
              << ", Bitrate: " << local_settings.opus_bitrate / 1000 << " kbps"
              << ", VBR: " << (local_settings.use_vbr ? "On" : "Off (CBR)")
              << ", Silence Gate: " << (local_settings.use_silence_gate ? "On" : "Off")
              << ", Debug Logging: " << (local_settings.debug_logging ? "On" : "Off")
              << ", PCM Chunk: " << pcm_chunk_size_bytes << " bytes"
              << std::endl;

    auto last_log_time = std::chrono::steady_clock::now();
    long chunks_read = 0;
    long chunks_silent = 0;
    long chunks_encoded = 0;
    long bytes_encoded = 0;
    bool first_sound_detected = false;
    uint64_t total_samples_processed = 0;
    int current_applied_bitrate = local_settings.opus_bitrate;
    int last_requested_bitrate = local_settings.opus_bitrate;

    started_ok = true;
    while (!stop_requested) {
      // Structural fields stay frozen in local_settings; only the live flags
      // (bitrate, silence gate, debug logging) are re-read here.
      AudioCaptureSettings latest_settings = get_current_settings();
      // Re-apply only when the requested bitrate changes, so a value opus keeps
      // rejecting doesn't re-log every frame.
      if (latest_settings.opus_bitrate != last_requested_bitrate) {
          last_requested_bitrate = latest_settings.opus_bitrate;
          int ret = opus_encoder_ctl(encoder, OPUS_SET_BITRATE(latest_settings.opus_bitrate));
          if (ret == OPUS_OK) {
              std::cout << "[pcmflux] Dynamic Bitrate Update: "
                        << (current_applied_bitrate / 1000) << " -> "
                        << (latest_settings.opus_bitrate / 1000) << " kbps" << std::endl;
              current_applied_bitrate = latest_settings.opus_bitrate;
          } else {
              std::cerr << "[pcmflux] Failed to update bitrate ("
                        << latest_settings.opus_bitrate << "): " << opus_strerror(ret) << std::endl;
          }
      }

      if (pa_simple_read(s, pcm_buffer.data(), pcm_chunk_size_bytes,
                         &pa_error) < 0) {
        std::cerr << "[pcmflux] ERROR: pa_simple_read() failed: "
                  << pa_strerror(pa_error) << std::endl;
        // Mark not-running before teardown so is_audio_capture_running() can't
        // transiently report 1 while the thread is dying after a read error.
        started_ok = false;
        break;
      }
      chunks_read++;
      
      uint64_t current_pts = total_samples_processed;
      total_samples_processed += frame_size_per_channel;

      bool is_silent = false;
      if (latest_settings.use_silence_gate) {
        // All int16 samples zero <=> all bytes zero; memcmp is ~12x faster than
        // a scalar early-exit scan on the common all-silent frame.
        is_silent = std::memcmp(pcm_buffer.data(), silence_ref.data(),
                                pcm_chunk_size_bytes) == 0;
      }

      if (is_silent) {
        chunks_silent++;
      } else {
        if (!first_sound_detected) {
          std::cout << "[pcmflux] First non-silent audio chunk detected! "
                       "Encoding..." << std::endl;
          first_sound_detected = true;
        }
        int encoded_bytes =
            opus_encode(encoder, pcm_buffer.data(), frame_size_per_channel,
                        opus_buffer.data(), max_opus_packet_size);

        if (encoded_bytes < 0) {
          std::cerr << "[pcmflux] ERROR: opus_encode() failed: "
                    << opus_strerror(encoded_bytes) << std::endl;
          continue;
        }

        chunks_encoded++;
        bytes_encoded += encoded_bytes;

        if (encoded_bytes > 0 && chunk_callback) {
          // When emit is on, prepend the 2-byte header [0x01,0x00] so the payload
          // is header+opus; when omit (WebRTC), header_sz is 0 and it's raw opus.
          const int header_sz =
              emit_audio_header_.load(std::memory_order_relaxed) ? 2 : 0;
          const int total_sz = header_sz + encoded_bytes;
          AudioChunkEncodeResult result;
          result.size = total_sz;
          result.data = new unsigned char[total_sz];
          result.pts = current_pts;
          if (header_sz) {
            result.data[0] = 0x01;  // audio chunk tag
            result.data[1] = 0x00;  // reserved (matches selkies' b'\x01\x00')
          }
          std::memcpy(result.data + header_sz, opus_buffer.data(), encoded_bytes);
          chunk_callback(&result, user_data);
          // Ownership invariant: whoever leaves result.data non-null owns the
          // single free. The Python wrapper (copy path) or OwnedAudioFrame.take()
          // (deferred) nulls it; if nobody took it, the dtor below frees it.
        }
      }

      auto now = std::chrono::steady_clock::now();
      auto elapsed_ms =
          std::chrono::duration_cast<std::chrono::milliseconds>(now -
                                                                last_log_time)
              .count();
      if (latest_settings.debug_logging && elapsed_ms >= 2000) {
        double seconds = elapsed_ms / 1000.0;
        double kbps = (bytes_encoded * 8) / (seconds * 1000.0);
        double silent_percent =
            (chunks_read > 0 ? (100.0 * chunks_silent / chunks_read) : 0.0);

        std::cout << "[pcmflux] Status | Read: " << chunks_read
                  << ", Silent: " << chunks_silent << " (" << std::fixed
                  << std::setprecision(1) << silent_percent << "%)"
                  << ", Encoded: " << chunks_encoded << ", Rate: " << std::fixed
                  << std::setprecision(2) << kbps << " kbps" << std::endl;

        last_log_time = now;
        chunks_read = chunks_silent = chunks_encoded = bytes_encoded = 0;
      }
    }

    std::cout << "[pcmflux] Stop requested. Cleaning up capture loop..."
              << std::endl;
    started_ok = false;
    if (encoder)
      opus_encoder_destroy(encoder);
    if (s)
      pa_simple_free(s);
    std::cout
        << "[pcmflux] Audio capture loop finished. Resources released."
        << std::endl;
  }
};

/**
 * @brief C-compatible wrapper for the C++ AudioCaptureModule.
 * These functions provide a stable ABI for using the module from other languages.
 */
extern "C" {
  typedef void* AudioCaptureModuleHandle;

  /**
   * @brief Creates a new instance of the AudioCaptureModule.
   * @return A handle to the created instance. Must be freed with
   *         destroy_audio_capture_module.
   */
  AudioCaptureModuleHandle create_audio_capture_module() {
    return static_cast<AudioCaptureModuleHandle>(new AudioCaptureModule());
  }

  /**
   * @brief Destroys an AudioCaptureModule instance and releases its resources.
   * @param handle Handle to the instance to destroy.
   */
  void destroy_audio_capture_module(AudioCaptureModuleHandle handle) {
    if (handle) {
      delete static_cast<AudioCaptureModule*>(handle);
    }
  }

  /**
   * @brief Configures and starts the audio capture process.
   * @param handle Handle to the AudioCaptureModule instance.
   * @param settings The initial capture and encoding settings.
   * @param callback A function pointer to be called with each encoded audio chunk.
   * @param user_data User-defined data to be passed to the callback.
   */
  void start_audio_capture(AudioCaptureModuleHandle handle,
                           AudioCaptureSettings settings,
                           AudioChunkCallback callback, void* user_data) {
    if (handle) {
      auto module = static_cast<AudioCaptureModule*>(handle);
      // Join any running capture before mutating the fields the capture thread
      // reads, so a re-entrant start can't race callback/user_data.
      module->stop_capture();
      module->modify_settings(settings);
      module->chunk_callback = callback;
      module->user_data = user_data;
      module->start_capture();
    }
  }

  /**
   * @brief Updates the Opus encoder bitrate during an active capture session.
   * @param handle Handle to the AudioCaptureModule instance.
   * @param new_bitrate The new target bitrate in bits per second.
   */
  void update_audio_bitrate(AudioCaptureModuleHandle handle, int new_bitrate) {
    if (handle) {
      static_cast<AudioCaptureModule*>(handle)->set_bitrate(new_bitrate);
    }
  }

  /**
   * @brief Stops the audio capture process.
   * This is a blocking call that waits for the capture thread to terminate.
   * @param handle Handle to the AudioCaptureModule instance.
   */
  void stop_audio_capture(AudioCaptureModuleHandle handle) {
    if (handle) {
      static_cast<AudioCaptureModule*>(handle)->stop_capture();
    }
  }

  // 1 while the capture thread is up (after PulseAudio/Opus init succeeded);
  // 0 if it never started, already failed, or was stopped.
  int is_audio_capture_running(AudioCaptureModuleHandle handle) {
    if (!handle) return 0;
    auto module = static_cast<AudioCaptureModule*>(handle);
    return (module->started_ok.load() && !module->stop_requested.load()) ? 1 : 0;
  }

  /**
   * @brief Frees the data buffer within an AudioChunkEncodeResult.
   * @param result Pointer to the result whose data should be freed.
   */
  void free_audio_chunk_encode_result_data(AudioChunkEncodeResult* result) {
    if (result && result->data) {
      delete[] result->data;
      result->data = nullptr;
    }
  }

  // Struct size so Python can assert ctypes/C ABI agreement.
  int pcmflux_audio_capture_settings_size() {
    return static_cast<int>(sizeof(AudioCaptureSettings));
  }
}

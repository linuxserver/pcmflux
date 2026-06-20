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

// Python.h must be first so its feature macros win over libc/libstdc++ headers.
#define PY_SSIZE_T_CLEAN
#include <Python.h>

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
// Async PulseAudio API (pa_mainloop + pa_context + pa_stream record). Replaces
// the blocking, non-cancellable pa_simple_read() path: a bounded
// pa_mainloop_prepare() timeout lets the capture loop re-check stop_requested
// every <=20ms and break out promptly even when the source is WEDGED (no data
// flowing), where pa_simple_read() would block indefinitely.
#include <pulse/context.h>
#include <pulse/error.h>
#include <pulse/introspect.h>
#include <pulse/mainloop.h>
#include <pulse/operation.h>
#include <pulse/sample.h>
#include <pulse/stream.h>

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

  // Safety net: frees the buffer only if the callback didn't take it (it nulls
  // data on take), so this is a no-op on the normal path.
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
// Thread startup result, published by capture_loop so start_capture() can report
// a failed pa_simple_new/opus_encoder_create instead of silently "succeeding".
enum class CaptureStartState { IDLE, STARTING, RUNNING, FAILED };

class AudioCaptureModule {
public:
  std::atomic<bool> stop_requested;
  std::thread capture_thread;
  // Serializes the joinable()/join()/reassign sequence on capture_thread so
  // concurrent start/stop on the same instance can't double-join or reassign it
  // while joinable (-> std::terminate/deadlock). DISTINCT from settings_mutex,
  // which must never be held across join().
  std::mutex thread_lifecycle_mutex;
  // Snapshot of capture_thread's id, published under thread_lifecycle_mutex when
  // the thread is created/cleared. Lets a re-entrant stop_capture() from the
  // capture thread detect itself and short-circuit WITHOUT reading the
  // non-atomic capture_thread or blocking on the lifecycle lock (a blocking
  // self-join would deadlock another thread already joining this one).
  std::atomic<std::thread::id> capture_thread_id_{};
  AudioChunkCallback chunk_callback = nullptr;
  void* user_data = nullptr;
  std::atomic<bool> started_ok{false};
  std::atomic<CaptureStartState> start_state{CaptureStartState::IDLE};
  mutable std::mutex settings_mutex;
  AudioCaptureSettings current_settings;
  // Lock-free mirror of !omit_audio_header so the hot path reads it without the
  // mutex. (deferred_free needs no mirror; it's resolved entirely Python-side.)
  std::atomic<bool> emit_audio_header_{true};
  // Lock-free mirrors of the per-frame-mutable settings so the capture loop reads
  // them without taking settings_mutex every frame. Published in modify_settings()
  // (and opus_bitrate_ in set_bitrate()).
  std::atomic<int> opus_bitrate_{128000};
  std::atomic<bool> use_silence_gate_{true};
  std::atomic<bool> debug_logging_{false};

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
    // Hold the lifecycle lock across the whole joinable/join/reassign sequence so
    // a concurrent start/stop can't double-join or reassign capture_thread while
    // it's joinable. Never take settings_mutex while holding this.
    if (capture_thread_id_.load(std::memory_order_acquire) ==
        std::this_thread::get_id()) {
      // Re-entrant start from the callback (on the capture thread): can't
      // join/recreate the running thread, so just keep it running and undo any
      // stop_requested a nested stop_capture() set. Short-circuits before the
      // lock to avoid self-deadlocking against another thread joining us.
      stop_requested = false;
      return;
    }
    std::lock_guard<std::mutex> lifecycle(thread_lifecycle_mutex);
    if (capture_thread.joinable()) {
      // Inline the stop+join here (don't call stop_capture(), which would
      // re-lock this non-recursive mutex and self-deadlock).
      stop_requested = true;
      capture_thread.join();
      capture_thread_id_.store(std::thread::id{}, std::memory_order_release);
    }
    // Clear stop_requested ONLY here -- under the lifecycle lock, after the old
    // thread is joined and before the new thread is spawned/published. Keep this
    // clear inside the lock: stop_capture() sets+joins under the same lock, so an
    // external stop's flag (set for the thread it is about to join) can never be
    // reset out from under it for a newer thread. Moving this clear (or stop's
    // set) outside the lock would reopen the lost-stop livelock.
    stop_requested = false;
    started_ok = false;
    start_state.store(CaptureStartState::STARTING, std::memory_order_release);
    capture_thread = std::thread(&AudioCaptureModule::capture_loop, this);
    capture_thread_id_.store(capture_thread.get_id(), std::memory_order_release);
  }

  /**
   * @brief Stops the audio capture process.
   * Sets a flag to signal the capture thread to terminate and waits for it to join.
   */
  void stop_capture() {
    // Re-entrant stop from the capture thread itself (the user's chunk callback
    // calling stop): it must NOT join itself (join() on the current thread
    // throws) and never touches the non-atomic capture_thread, so it sets the
    // stop flag and returns WITHOUT taking the lifecycle lock. Taking it would
    // deadlock if another thread already holds it while blocked in
    // capture_thread.join() waiting for this very thread to exit. Setting
    // stop_requested (atomic) makes the loop exit once the callback returns.
    // Compared against the atomic id snapshot so this reads no non-atomic state.
    if (capture_thread_id_.load(std::memory_order_acquire) ==
        std::this_thread::get_id()) {
      stop_requested = true;
      return;
    }
    // External stop. Hold the lifecycle lock across joinable/join so a concurrent
    // start/stop can't double-join the same thread. Never take settings_mutex
    // while holding this (join() would then block under settings_mutex).
    //
    // Lost-stop fix: set stop_requested INSIDE this locked region, immediately
    // before joining the thread we observed joinable -- NOT before the lock. The
    // earlier unconditional set raced start_capture(): a stop targeting the
    // running thread set stop_requested=true outside the lock, then a concurrent
    // start_capture() joined that thread, cleared stop_requested=false, and
    // spawned a NEW thread; this stop then took the lock, found the new thread
    // joinable, and joined it with no stop pending -> the new thread ran forever
    // and the joiner blocked forever (livelock). Because start_capture() only
    // ever clears stop_requested under this SAME lock (after joining the old
    // thread, before spawning/publishing the new one), pairing our set+join
    // atomically under the lock means a clear can never land between them: we
    // always set the flag for exactly the thread we are about to join, and that
    // flag cannot be reset for a different (newer) thread until after our join
    // completes and releases the lock.
    std::lock_guard<std::mutex> lifecycle(thread_lifecycle_mutex);
    if (capture_thread.joinable()) {
      stop_requested = true;
      capture_thread.join();
      capture_thread_id_.store(std::thread::id{}, std::memory_order_release);
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
    // Publish the per-frame-mutable flags to their lock-free mirrors too.
    opus_bitrate_.store(new_settings.opus_bitrate, std::memory_order_relaxed);
    use_silence_gate_.store(new_settings.use_silence_gate, std::memory_order_relaxed);
    debug_logging_.store(new_settings.debug_logging, std::memory_order_relaxed);
  }

  // Atomically updates only the bitrate (under the mutex), so it can't lose a
  // concurrent modify_settings() the way a get/modify/set of the whole struct would.
  void set_bitrate(int new_bitrate) {
    std::lock_guard<std::mutex> lock(settings_mutex);
    current_settings.opus_bitrate = new_bitrate;
    // Mirror so the capture loop picks up the live bitrate without the mutex.
    opus_bitrate_.store(new_bitrate, std::memory_order_relaxed);
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
  // Per-run state for the async capture loop, threaded into the PulseAudio read
  // callback via the stream's userdata. All members are owned by capture_loop()'s
  // stack and live only for the duration of one run (single capture thread, so no
  // locking is needed between the loop body and the synchronously-dispatched
  // read callback). Carries the encode+deliver context so that work stays on the
  // capture thread exactly as before.
  struct CaptureRunState {
    AudioCaptureModule* self;
    OpusEncoder* encoder;
    // Frame geometry (fixed for the run; mirrors the old pcm_chunk math).
    int frame_size_per_channel;
    int pcm_chunk_size_bytes;
    int channels;
    // Reassembly buffer: pa_stream_peek() hands back fragments of arbitrary size
    // (less or more than one Opus frame), so accumulate bytes here and encode one
    // exact pcm_chunk_size_bytes frame at a time -- pa_simple_read() did this
    // fragment-to-frame reassembly internally; now we do it explicitly.
    std::vector<unsigned char> pcm_accum;
    size_t pcm_fill;  // bytes currently buffered in pcm_accum
    const std::vector<int16_t>* silence_ref;
    std::vector<unsigned char>* opus_buffer;
    int max_opus_packet_size;
    // Live-tunable bitrate tracking (mirrors the old loop locals).
    int current_applied_bitrate;
    int last_requested_bitrate;
    // pts: strictly monotonic, +frame_size_per_channel per emitted frame
    // (=> +960 at 48k/20ms), unchanged from the blocking path.
    uint64_t total_samples_processed;
    bool first_sound_detected;
    // Stats for the optional 2s debug log.
    long chunks_read;
    long chunks_silent;
    long chunks_encoded;
    long bytes_encoded;
  };

  // Encode + deliver exactly one pcm_chunk_size_bytes frame from the front of the
  // reassembly buffer. Identical silence-gate / Opus / 2-byte-header / pts /
  // ownership semantics to the old blocking loop; only the data source changed.
  void encode_and_deliver_frame(CaptureRunState& st, const unsigned char* pcm,
                                bool use_silence_gate) {
    st.chunks_read++;

    // Apply a pending dynamic bitrate change once per actual frame (cheap; the
    // old loop re-checked every read). Re-apply only when it changes so a value
    // Opus keeps rejecting doesn't re-log every frame.
    const int requested_bitrate = opus_bitrate_.load(std::memory_order_relaxed);
    if (requested_bitrate != st.last_requested_bitrate) {
      st.last_requested_bitrate = requested_bitrate;
      int ret = opus_encoder_ctl(st.encoder, OPUS_SET_BITRATE(requested_bitrate));
      if (ret == OPUS_OK) {
        std::cout << "[pcmflux] Dynamic Bitrate Update: "
                  << (st.current_applied_bitrate / 1000) << " -> "
                  << (requested_bitrate / 1000) << " kbps" << std::endl;
        st.current_applied_bitrate = requested_bitrate;
      } else {
        std::cerr << "[pcmflux] Failed to update bitrate (" << requested_bitrate
                  << "): " << opus_strerror(ret) << std::endl;
      }
    }

    const uint64_t current_pts = st.total_samples_processed;
    st.total_samples_processed += st.frame_size_per_channel;

    bool is_silent = false;
    if (use_silence_gate) {
      // All int16 samples zero <=> all bytes zero; memcmp is ~12x faster than a
      // scalar early-exit scan on the common all-silent frame.
      is_silent = std::memcmp(pcm, st.silence_ref->data(),
                              st.pcm_chunk_size_bytes) == 0;
    }

    if (is_silent) {
      st.chunks_silent++;
      return;
    }
    if (!st.first_sound_detected) {
      std::cout << "[pcmflux] First non-silent audio chunk detected! Encoding..."
                << std::endl;
      st.first_sound_detected = true;
    }
    int encoded_bytes = opus_encode(
        st.encoder, reinterpret_cast<const opus_int16*>(pcm),
        st.frame_size_per_channel, st.opus_buffer->data(),
        st.max_opus_packet_size);
    if (encoded_bytes < 0) {
      std::cerr << "[pcmflux] ERROR: opus_encode() failed: "
                << opus_strerror(encoded_bytes) << std::endl;
      return;
    }
    st.chunks_encoded++;
    st.bytes_encoded += encoded_bytes;

    if (encoded_bytes > 0 && chunk_callback) {
      // When emit is on, prepend the 2-byte header [0x01,0x00] so the payload is
      // header+opus; when omit (WebRTC), header_sz is 0 and it's raw opus.
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
      std::memcpy(result.data + header_sz, st.opus_buffer->data(), encoded_bytes);
      chunk_callback(&result, user_data);
      // Ownership invariant: the callback nulls result.data when it takes the
      // buffer; if it didn't (NULL callback / error), the dtor frees it.
    }
  }

  // Result of the pre-connect source-existence probe. found stays false unless
  // the server reports a matching source in the info callback below.
  struct SourceProbe {
    bool found;
    bool done;
  };

  // pa_context_get_source_info_by_name() callback. Called once per matching
  // source then once with eol>0; an unknown name yields only the eol call (with
  // i==NULL), so found stays false. Runs on the capture thread inside our pump.
  static void source_info_cb(pa_context* /*c*/, const pa_source_info* i, int eol,
                             void* userdata) {
    SourceProbe* p = static_cast<SourceProbe*>(userdata);
    if (eol < 0) {  // query error (e.g. no such entity)
      p->done = true;
      return;
    }
    if (eol > 0) {  // end of list
      p->done = true;
      return;
    }
    if (i && i->name) p->found = true;
  }

  // PulseAudio record read callback. Runs synchronously inside
  // pa_mainloop_dispatch() ON THE CAPTURE THREAD (single-threaded pa_mainloop, so
  // no extra locking vs. the rest of the loop). Drains every fragment currently
  // available with pa_stream_peek/pa_stream_drop, reassembles them into whole
  // frames, and encodes+delivers each. Draining fully here keeps server-side
  // buffering from growing when fragments arrive in bursts.
  static void stream_read_cb(pa_stream* s, size_t /*nbytes*/, void* userdata) {
    CaptureRunState* st = static_cast<CaptureRunState*>(userdata);
    const bool use_silence_gate =
        st->self->use_silence_gate_.load(std::memory_order_relaxed);
    while (true) {
      const void* data = nullptr;
      size_t len = 0;
      if (pa_stream_peek(s, &data, &len) < 0) {
        std::cerr << "[pcmflux] ERROR: pa_stream_peek() failed: "
                  << pa_strerror(pa_context_errno(pa_stream_get_context(s)))
                  << std::endl;
        return;
      }
      if (len == 0) {
        // Buffer empty: nothing to drop, drain done.
        break;
      }
      if (data == nullptr) {
        // Hole in the stream (xrun): no data but the read index must advance.
        // Drop it (peek then drop is required) and keep going; don't feed the
        // reassembly buffer or the pts/frame alignment would drift.
        pa_stream_drop(s);
        continue;
      }
      // Append this fragment to the reassembly buffer, emitting whole frames as
      // they complete.
      const unsigned char* src = static_cast<const unsigned char*>(data);
      size_t remaining = len;
      while (remaining > 0) {
        const size_t want = (size_t)st->pcm_chunk_size_bytes - st->pcm_fill;
        const size_t take = remaining < want ? remaining : want;
        std::memcpy(st->pcm_accum.data() + st->pcm_fill, src, take);
        st->pcm_fill += take;
        src += take;
        remaining -= take;
        if (st->pcm_fill == (size_t)st->pcm_chunk_size_bytes) {
          st->self->encode_and_deliver_frame(*st, st->pcm_accum.data(),
                                              use_silence_gate);
          st->pcm_fill = 0;
        }
      }
      pa_stream_drop(s);
    }
  }

  // Pump the mainloop one bounded iteration. timeout_us caps the underlying
  // poll() so the caller re-gains control (to re-check stop_requested) within
  // that bound even when the source is WEDGED and no fd ever becomes readable --
  // this is what makes stop_capture() return promptly on a wedged source.
  // Returns false if the mainloop signalled error/quit.
  static bool pump_mainloop(pa_mainloop* m, int timeout_us) {
    if (pa_mainloop_prepare(m, timeout_us) < 0) return false;
    if (pa_mainloop_poll(m) < 0) return false;
    if (pa_mainloop_dispatch(m) < 0) return false;
    return true;
  }

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
   * The loop runs until stop_requested is set to true, then cleans up all
   * resources. Uses the async PulseAudio API (pa_mainloop + pa_context +
   * pa_stream record) driven by bounded pa_mainloop iterations so stop_requested
   * is observed within ~20ms even when the source delivers no data.
   */
  void capture_loop() {
    AudioCaptureSettings local_settings = get_current_settings();
    // Seed the lock-free mirrors from the startup snapshot so the hot path is
    // correct even if start_capture() ran without a prior modify_settings().
    opus_bitrate_.store(local_settings.opus_bitrate, std::memory_order_relaxed);
    use_silence_gate_.store(local_settings.use_silence_gate, std::memory_order_relaxed);
    debug_logging_.store(local_settings.debug_logging, std::memory_order_relaxed);

    OpusEncoder* encoder = nullptr;
    pa_mainloop* mainloop = nullptr;
    pa_context* context = nullptr;
    pa_stream* stream = nullptr;

    // Bounded poll timeout (~20ms): the loop wakes at least this often to observe
    // stop_requested, so stop is prompt (<100ms) even on a WEDGED source where no
    // data ever arrives. The blocking pa_simple_read() could stall indefinitely.
    const int kPumpTimeoutUs = 20 * 1000;

    // Marks the run failed, tears down whatever was created (in reverse order),
    // and returns from capture_loop. Used for every startup-failure path so the
    // CaptureStartState handshake still turns a connect failure into a Python
    // RuntimeError.
    auto fail_and_cleanup = [&]() {
      if (stream) {
        pa_stream_set_read_callback(stream, nullptr, nullptr);
        pa_stream_set_state_callback(stream, nullptr, nullptr);
        pa_stream_disconnect(stream);
        pa_stream_unref(stream);
        stream = nullptr;
      }
      if (encoder) {
        opus_encoder_destroy(encoder);
        encoder = nullptr;
      }
      if (context) {
        pa_context_set_state_callback(context, nullptr, nullptr);
        pa_context_disconnect(context);
        pa_context_unref(context);
        context = nullptr;
      }
      if (mainloop) {
        pa_mainloop_free(mainloop);
        mainloop = nullptr;
      }
      started_ok = false;
      start_state.store(CaptureStartState::FAILED, std::memory_order_release);
    };

    const pa_sample_spec ss = {.format = PA_SAMPLE_S16LE,
                               .rate = local_settings.sample_rate,
                               .channels = (uint8_t)local_settings.channels};

    pa_buffer_attr attr;
    attr.maxlength = (uint32_t)-1;
    attr.tlength = (uint32_t)-1;
    attr.prebuf = (uint32_t)-1;
    attr.minreq = (uint32_t)-1;
    attr.fragsize = (uint32_t)-1;
    bool adjust_latency = false;
    if (local_settings.latency_ms > 0) {
        // Cast before multiplying to avoid signed-int overflow (result is pa_usec_t).
        // ADJUST_LATENCY asks the server to honor this fragment-sized latency, matching
        // the configured-latency behaviour of the old pa_simple buffer attr.
        attr.fragsize = pa_usec_to_bytes((pa_usec_t)local_settings.latency_ms * 1000, &ss);
        adjust_latency = true;
    } else {
        // latency_ms<=0 (default): floor the fragment at ~20ms so the FIRST opus frame
        // is delivered promptly (~60ms). Leaving fragsize=(uint32_t)-1 lets the server
        // pick its huge default fragment (PipeWire ~2s), which delayed first-DELIVERY by
        // ~2s even though the bounded mainloop pump (kPumpTimeoutUs) already guarantees
        // prompt STOP regardless of fragment size. This restores the pre-A2/WF8 floor;
        // do NOT request PA_STREAM_ADJUST_LATENCY here (adjust_latency stays false), so
        // only the fragment hint changes and the configured-latency path is untouched.
        attr.fragsize = pa_usec_to_bytes((pa_usec_t)20 * 1000, &ss);
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

    mainloop = pa_mainloop_new();
    if (!mainloop) {
      std::cerr << "[pcmflux] ERROR: pa_mainloop_new() failed." << std::endl;
      fail_and_cleanup();
      return;
    }
    pa_mainloop_api* mainloop_api = pa_mainloop_get_api(mainloop);
    context = pa_context_new(mainloop_api, "pcmflux");
    if (!context) {
      std::cerr << "[pcmflux] ERROR: pa_context_new() failed." << std::endl;
      fail_and_cleanup();
      return;
    }
    // No state callback needed: the single-threaded mainloop dispatches state
    // transitions synchronously inside our pump, so we read the state directly.
    if (pa_context_connect(context, NULL, PA_CONTEXT_NOFLAGS, NULL) < 0) {
      std::cerr << "[pcmflux] ERROR: pa_context_connect() failed: "
                << pa_strerror(pa_context_errno(context)) << std::endl;
      fail_and_cleanup();
      return;
    }

    // Drive the connection to PA_CONTEXT_READY (or fail). Honors stop_requested so
    // a stop during connect breaks out promptly instead of waiting out the server.
    for (;;) {
      pa_context_state_t cstate = pa_context_get_state(context);
      if (cstate == PA_CONTEXT_READY) break;
      if (!PA_CONTEXT_IS_GOOD(cstate)) {
        std::cerr << "[pcmflux] ERROR: PulseAudio context connection failed: "
                  << pa_strerror(pa_context_errno(context)) << std::endl;
        fail_and_cleanup();
        return;
      }
      if (stop_requested) {  // stop before we ever got going
        // Distinct, non-alarming line so the resulting RuntimeError ("...see stderr
        // for details") isn't backed by empty stderr when a start races a stop.
        std::cerr << "[pcmflux] audio capture start aborted: stop requested during "
                     "startup (context connect)." << std::endl;
        fail_and_cleanup();
        return;
      }
      if (!pump_mainloop(mainloop, kPumpTimeoutUs)) {
        std::cerr << "[pcmflux] ERROR: mainloop iterate failed during connect."
                  << std::endl;
        fail_and_cleanup();
        return;
      }
    }
    std::cout << "[pcmflux] SUCCESS: Connected to PulseAudio." << std::endl;

    // Validate a NAMED device up front. The old pa_simple_new() failed
    // synchronously when the requested source didn't exist; the async
    // pa_stream_connect_record() does not (some servers, e.g. PipeWire's pulse
    // shim, accept the connect and silently bind a fallback), which would turn a
    // bad device_name into a no-data capture instead of the documented
    // connect-failure -> Python RuntimeError. Probe the source by name and fail
    // if the server doesn't know it. NULL device (system default) is not probed,
    // matching pa_simple_new(NULL).
    if (device_to_use) {
      SourceProbe probe = {false, false};
      pa_operation* op = pa_context_get_source_info_by_name(
          context, device_to_use, &AudioCaptureModule::source_info_cb, &probe);
      if (!op) {
        std::cerr << "[pcmflux] ERROR: failed to query source '" << device_to_use
                  << "': " << pa_strerror(pa_context_errno(context)) << std::endl;
        fail_and_cleanup();
        return;
      }
      while (!probe.done) {
        if (stop_requested) {  // stop during the probe: bail promptly
          std::cerr << "[pcmflux] audio capture start aborted: stop requested during "
                       "startup (source probe)." << std::endl;
          pa_operation_unref(op);
          fail_and_cleanup();
          return;
        }
        // A dead/failed context would never complete the op; bail if it drops.
        if (!PA_CONTEXT_IS_GOOD(pa_context_get_state(context))) {
          pa_operation_unref(op);
          std::cerr << "[pcmflux] ERROR: context failed during source probe."
                    << std::endl;
          fail_and_cleanup();
          return;
        }
        if (!pump_mainloop(mainloop, kPumpTimeoutUs)) {
          pa_operation_unref(op);
          std::cerr << "[pcmflux] ERROR: mainloop iterate failed during source probe."
                    << std::endl;
          fail_and_cleanup();
          return;
        }
      }
      pa_operation_unref(op);
      if (!probe.found) {
        std::cerr << "[pcmflux] ERROR: PulseAudio source not found." << std::endl;
        std::cerr << "  (Could not find the device named: '" << device_to_use
                  << "')" << std::endl;
        fail_and_cleanup();
        return;
      }
    }

    int opus_error;
    encoder = opus_encoder_create(local_settings.sample_rate,
                                  local_settings.channels,
                                  OPUS_APPLICATION_RESTRICTED_LOWDELAY,
                                  &opus_error);
    if (opus_error != OPUS_OK) {
      std::cerr << "[pcmflux] ERROR: opus_encoder_create() failed: "
                << opus_strerror(opus_error) << std::endl;
      fail_and_cleanup();
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
      fail_and_cleanup();
      return;
    }

    const int frame_size_per_channel =
        (local_settings.sample_rate * local_settings.frame_duration_ms) / 1000;
    const int pcm_chunk_size_bytes =
        frame_size_per_channel * local_settings.channels * sizeof(int16_t);
    const std::vector<int16_t> silence_ref(
        frame_size_per_channel * local_settings.channels, 0);
    const int max_opus_packet_size = 4000;
    std::vector<unsigned char> opus_buffer(max_opus_packet_size);

    // Per-run state handed to the read callback via the stream userdata. Lives on
    // this stack frame for the whole run (the read callback only fires while we
    // pump below, so no lifetime gap).
    CaptureRunState run_state;
    run_state.self = this;
    run_state.encoder = encoder;
    run_state.frame_size_per_channel = frame_size_per_channel;
    run_state.pcm_chunk_size_bytes = pcm_chunk_size_bytes;
    run_state.channels = local_settings.channels;
    run_state.pcm_accum.resize(pcm_chunk_size_bytes);
    run_state.pcm_fill = 0;
    run_state.silence_ref = &silence_ref;
    run_state.opus_buffer = &opus_buffer;
    run_state.max_opus_packet_size = max_opus_packet_size;
    run_state.current_applied_bitrate = local_settings.opus_bitrate;
    run_state.last_requested_bitrate = local_settings.opus_bitrate;
    run_state.total_samples_processed = 0;
    run_state.first_sound_detected = false;
    run_state.chunks_read = 0;
    run_state.chunks_silent = 0;
    run_state.chunks_encoded = 0;
    run_state.bytes_encoded = 0;

    stream = pa_stream_new(context, "Audio Capture", &ss, NULL);
    if (!stream) {
      std::cerr << "[pcmflux] ERROR: pa_stream_new() failed: "
                << pa_strerror(pa_context_errno(context)) << std::endl;
      fail_and_cleanup();
      return;
    }
    pa_stream_set_read_callback(stream, &AudioCaptureModule::stream_read_cb,
                                &run_state);

    pa_stream_flags_t stream_flags = (pa_stream_flags_t)(
        PA_STREAM_ADJUST_LATENCY * (adjust_latency ? 1 : 0));
    if (pa_stream_connect_record(stream, device_to_use, &attr, stream_flags) < 0) {
      std::cerr << "[pcmflux] ERROR: pa_stream_connect_record() failed: "
                << pa_strerror(pa_context_errno(context)) << std::endl;
      if (device_to_use) {
        std::cerr << "  (Could not find the device named: '" << device_to_use
                  << "')" << std::endl;
      }
      fail_and_cleanup();
      return;
    }

    // Drive the stream to PA_STREAM_READY (or fail). Honors stop_requested.
    for (;;) {
      pa_stream_state_t sstate = pa_stream_get_state(stream);
      if (sstate == PA_STREAM_READY) break;
      if (!PA_STREAM_IS_GOOD(sstate)) {
        std::cerr << "[pcmflux] ERROR: PulseAudio record stream failed: "
                  << pa_strerror(pa_context_errno(context)) << std::endl;
        if (device_to_use) {
          std::cerr << "  (Could not find the device named: '" << device_to_use
                    << "')" << std::endl;
        }
        fail_and_cleanup();
        return;
      }
      if (stop_requested) {
        std::cerr << "[pcmflux] audio capture start aborted: stop requested during "
                     "startup (stream connect)." << std::endl;
        fail_and_cleanup();
        return;
      }
      if (!pump_mainloop(mainloop, kPumpTimeoutUs)) {
        std::cerr << "[pcmflux] ERROR: mainloop iterate failed during stream connect."
                  << std::endl;
        fail_and_cleanup();
        return;
      }
    }

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

    started_ok = true;
    start_state.store(CaptureStartState::RUNNING, std::memory_order_release);

    // Hot loop: pump the mainloop in bounded steps. Each pump dispatches the read
    // callback (which does the peek/drop/encode/deliver on THIS thread) and then
    // returns within kPumpTimeoutUs even if no data arrived, so stop_requested is
    // observed promptly. A wedged source can no longer stall the stop path.
    while (!stop_requested) {
      if (!pump_mainloop(mainloop, kPumpTimeoutUs)) {
        std::cerr << "[pcmflux] ERROR: mainloop iterate failed; stopping capture."
                  << std::endl;
        started_ok = false;
        break;
      }
      // A stream that fails mid-run (e.g. source removed) must end the loop too;
      // otherwise it would spin reading no data.
      pa_stream_state_t sstate = pa_stream_get_state(stream);
      if (sstate != PA_STREAM_READY) {
        std::cerr << "[pcmflux] ERROR: record stream entered state "
                  << (int)sstate << "; stopping capture." << std::endl;
        started_ok = false;
        break;
      }

      const bool debug_logging = debug_logging_.load(std::memory_order_relaxed);
      // Only pay for the clock read when debug logging is on (drives the 2s status
      // log); the hot path skips steady_clock::now() entirely otherwise.
      if (debug_logging) {
        auto now = std::chrono::steady_clock::now();
        auto elapsed_ms =
            std::chrono::duration_cast<std::chrono::milliseconds>(now -
                                                                  last_log_time)
                .count();
        if (elapsed_ms < 2000) continue;
        double seconds = elapsed_ms / 1000.0;
        double kbps = (run_state.bytes_encoded * 8) / (seconds * 1000.0);
        double silent_percent =
            (run_state.chunks_read > 0
                 ? (100.0 * run_state.chunks_silent / run_state.chunks_read)
                 : 0.0);

        std::cout << "[pcmflux] Status | Read: " << run_state.chunks_read
                  << ", Silent: " << run_state.chunks_silent << " (" << std::fixed
                  << std::setprecision(1) << silent_percent << "%)"
                  << ", Encoded: " << run_state.chunks_encoded
                  << ", Rate: " << std::fixed << std::setprecision(2) << kbps
                  << " kbps" << std::endl;

        last_log_time = now;
        run_state.chunks_read = run_state.chunks_silent =
            run_state.chunks_encoded = run_state.bytes_encoded = 0;
      }
    }

    std::cout << "[pcmflux] Stop requested. Cleaning up capture loop..."
              << std::endl;
    started_ok = false;
    // Tear down in reverse creation order. Clear the read callback first so no
    // late dispatch touches run_state after we leave this frame, then disconnect
    // and unref the stream, destroy the encoder, disconnect/unref the context,
    // and free the mainloop. No leak; mirrors the old pa_simple_free cleanup.
    if (stream) {
      pa_stream_set_read_callback(stream, nullptr, nullptr);
      pa_stream_set_state_callback(stream, nullptr, nullptr);
      pa_stream_disconnect(stream);
      pa_stream_unref(stream);
    }
    if (encoder)
      opus_encoder_destroy(encoder);
    if (context) {
      pa_context_set_state_callback(context, nullptr, nullptr);
      pa_context_disconnect(context);
      pa_context_unref(context);
    }
    if (mainloop)
      pa_mainloop_free(mainloop);
    std::cout
        << "[pcmflux] Audio capture loop finished. Resources released."
        << std::endl;
  }
};

// ============================================================================
// CPython C-API extension (full API, not Limited/abi3). Built with -std=c++17,
// so no C++20 designated initializers. All types via PyType_FromSpec.
// ============================================================================

// Heap type for AudioFrame; created once in PyInit, never freed (immortal).
static PyObject* g_AudioFrameType = nullptr;

// ----------------------------------------------------------------------------
// AudioFrame: zero-copy, refcount-owned view over one encoded chunk. Owns the
// new[]'d buffer; a memoryview/slice keeps it alive via Py_buffer.obj refcount.
// ----------------------------------------------------------------------------
typedef struct {
  PyObject_HEAD
  unsigned char* data;  // new[]-allocated; freed in dealloc
  Py_ssize_t size;
  uint64_t pts;
} AudioFrameObject;

static int AudioFrame_getbuffer(PyObject* self, Py_buffer* view, int flags) {
  AudioFrameObject* f = (AudioFrameObject*)self;
  if (f->data == nullptr) {
    PyErr_SetString(PyExc_ValueError, "AudioFrame buffer already released");
    view->obj = nullptr;
    return -1;
  }
  // readonly=1; FillInfo INCREFs self into view->obj, pinning the buffer.
  return PyBuffer_FillInfo(view, self, f->data, f->size, 1, flags);
}

static void AudioFrame_releasebuffer(PyObject* self, Py_buffer* view) {
  (void)self;
  (void)view;  // FillInfo set view->obj; CPython DECREFs it. Nothing to do.
}

static Py_ssize_t AudioFrame_length(PyObject* self) {
  return ((AudioFrameObject*)self)->size;
}

static PyObject* AudioFrame_get_pts(PyObject* self, void* closure) {
  (void)closure;
  return PyLong_FromUnsignedLongLong(
      (unsigned long long)((AudioFrameObject*)self)->pts);
}

static void AudioFrame_dealloc(PyObject* self) {
  AudioFrameObject* f = (AudioFrameObject*)self;
  delete[] f->data;
  f->data = nullptr;
  // Heap-type teardown: call tp_free, then drop the type ref tp_alloc took.
  PyTypeObject* tp = Py_TYPE(self);
  freefunc free_fn = (freefunc)PyType_GetSlot(tp, Py_tp_free);
  free_fn(self);
  Py_DECREF(tp);
}

static PyGetSetDef AudioFrame_getset[] = {
    {"pts", AudioFrame_get_pts, nullptr, PyDoc_STR("Presentation timestamp (samples)."), nullptr},
    {nullptr, nullptr, nullptr, nullptr, nullptr},
};

static PyType_Slot AudioFrame_slots[] = {
    {Py_tp_dealloc, (void*)AudioFrame_dealloc},
    {Py_tp_getset, (void*)AudioFrame_getset},
    {Py_bf_getbuffer, (void*)AudioFrame_getbuffer},
    {Py_bf_releasebuffer, (void*)AudioFrame_releasebuffer},
    {Py_sq_length, (void*)AudioFrame_length},
    {Py_mp_length, (void*)AudioFrame_length},
    {Py_tp_doc, (void*)PyDoc_STR("Zero-copy buffer-protocol view over an encoded Opus chunk.")},
    {0, nullptr},
};

static PyType_Spec AudioFrame_spec = {
    "pcmflux._capture.AudioFrame",
    sizeof(AudioFrameObject),
    0,
    Py_TPFLAGS_DEFAULT,
    AudioFrame_slots,
};

// ----------------------------------------------------------------------------
// AudioCapture: owns an AudioCaptureModule and the Python callback. The C++
// capture thread invokes capture_trampoline per chunk.
// ----------------------------------------------------------------------------
typedef struct {
  PyObject_HEAD
  AudioCaptureModule* module;
  PyObject* callback;
  PyObject* device_name_bytes;  // keeps cset.device_name's bytes alive for the thread
} AudioCaptureObject;

// Called from the C++ capture thread. Builds an AudioFrame that takes ownership
// of result->data and dispatches to the Python callback. Never lets a Python
// exception escape into C++.
static void capture_trampoline(AudioChunkEncodeResult* result, void* user_data) {
  AudioCaptureObject* cap = (AudioCaptureObject*)user_data;
  PyGILState_STATE g = PyGILState_Ensure();
  // Snapshot the callback under the GIL and take a strong ref BEFORE calling it,
  // dropping it AFTER. The join-first teardown already prevents dealloc/tp_clear
  // from running concurrently with an in-flight call, but a re-entrant stop_capture()
  // invoked FROM the callback runs on this very thread and does Py_CLEAR(cap->callback)
  // before this frame returns. The local ref keeps the callback object alive across
  // the whole call (and any post-call use) regardless of such a re-entrant Py_CLEAR.
  PyObject* cb = cap->callback;
  Py_XINCREF(cb);
  if (cb && result && result->size > 0 && result->data) {
    PyTypeObject* ft = (PyTypeObject*)g_AudioFrameType;
    AudioFrameObject* f = (AudioFrameObject*)ft->tp_alloc(ft, 0);
    if (f) {
      f->data = result->data;
      f->size = result->size;
      f->pts = result->pts;
      result->data = nullptr;  // ownership transferred to Python; C++ won't free
      PyObject* r = PyObject_CallFunctionObjArgs(cb, (PyObject*)f, nullptr);
      if (!r) {
        PyErr_WriteUnraisable(cb);
      } else {
        Py_DECREF(r);
      }
      Py_DECREF(f);
    } else {
      PyErr_WriteUnraisable(cb);  // alloc failed; buffer freed by C++ dtor
    }
  }
  Py_XDECREF(cb);
  PyGILState_Release(g);
}

static PyObject* AudioCapture_new(PyTypeObject* type, PyObject* args, PyObject* kwds) {
  (void)args;
  (void)kwds;
  AudioCaptureObject* self = (AudioCaptureObject*)type->tp_alloc(type, 0);
  if (!self) return nullptr;
  self->module = nullptr;
  self->callback = nullptr;
  self->device_name_bytes = nullptr;
  self->module = new (std::nothrow) AudioCaptureModule();
  if (!self->module) {
    Py_DECREF(self);
    PyErr_NoMemory();
    return nullptr;
  }
  // No PyObject_GC_Track here: tp_alloc (PyType_GenericAlloc) already tracks GC
  // heap-type instances; tracking again aborts ("object already tracked").
  return (PyObject*)self;
}

// GC support: the object holds a Python callback (and device bytes) that can form a
// reference cycle (e.g. a bound-method callback whose self owns this AudioCapture).
static int AudioCapture_traverse(PyObject* self, visitproc visit, void* arg) {
  AudioCaptureObject* cap = (AudioCaptureObject*)self;
  Py_VISIT(Py_TYPE(self));  // heap type: must visit its type
  Py_VISIT(cap->callback);
  Py_VISIT(cap->device_name_bytes);
  return 0;
}

// Break the cycle. JOIN the capture thread FIRST (GIL released so any in-flight
// trampoline's PyGILState_Ensure can complete and the thread exits), THEN null
// the C++ members and Py_CLEAR the callback. Nulling chunk_callback/user_data as
// plain stores BEFORE the join is a data race + UAF: capture_loop reads them
// without the GIL or a mutex. After the join there's no concurrent reader, so the
// non-atomic stores are safe. Releasing the GIL across the join is required (the
// trampoline can't acquire the GIL otherwise -> deadlock); it's also the standard
// pattern for tp_clear running during GC.
static int AudioCapture_clear(PyObject* self) {
  AudioCaptureObject* cap = (AudioCaptureObject*)self;
  if (cap->module) {
    cap->module->stop_requested = true;
    Py_BEGIN_ALLOW_THREADS
    cap->module->stop_capture();  // joins; the trampoline can take the GIL/finish
    Py_END_ALLOW_THREADS
    // Capture thread has joined: no concurrent reader of these members remains.
    cap->module->chunk_callback = nullptr;
    cap->module->user_data = nullptr;
  }
  Py_CLEAR(cap->callback);
  Py_CLEAR(cap->device_name_bytes);
  return 0;
}

static void AudioCapture_dealloc(PyObject* self) {
  PyObject_GC_UnTrack(self);  // GC type: untrack before teardown
  AudioCaptureObject* cap = (AudioCaptureObject*)self;
  if (cap->module) {
    // JOIN the capture thread FIRST (GIL released so any in-flight trampoline can
    // acquire the GIL and the thread exits), THEN null the C++ members. Nulling
    // them before the join would race capture_loop's GIL-less reads (UAF/null
    // deref). After the join there's no concurrent reader, so the stores are safe.
    cap->module->stop_requested = true;
    Py_BEGIN_ALLOW_THREADS  // stop_capture joins the thread; release GIL so the
    cap->module->stop_capture();  // trampoline can acquire it to finish draining
    Py_END_ALLOW_THREADS
    cap->module->chunk_callback = nullptr;
    cap->module->user_data = nullptr;
    delete cap->module;
    cap->module = nullptr;
  }
  Py_CLEAR(cap->callback);
  Py_CLEAR(cap->device_name_bytes);
  PyTypeObject* tp = Py_TYPE(self);
  freefunc free_fn = (freefunc)PyType_GetSlot(tp, Py_tp_free);
  free_fn(self);
  Py_DECREF(tp);
}

// Read one bool-ish attribute (0/1) from a settings object; -1 + exception set.
static int read_bool_attr(PyObject* obj, const char* name, bool* out) {
  PyObject* v = PyObject_GetAttrString(obj, name);
  if (!v) return -1;
  int b = PyObject_IsTrue(v);
  Py_DECREF(v);
  if (b < 0) return -1;
  *out = (b != 0);
  return 0;
}

// Read one int attribute; -1 + exception set.
static int read_int_attr(PyObject* obj, const char* name, long* out) {
  PyObject* v = PyObject_GetAttrString(obj, name);
  if (!v) return -1;
  long n = PyLong_AsLong(v);
  Py_DECREF(v);
  if (n == -1 && PyErr_Occurred()) return -1;
  *out = n;
  return 0;
}

static PyObject* AudioCapture_start_capture(PyObject* self, PyObject* args) {
  AudioCaptureObject* cap = (AudioCaptureObject*)self;
  PyObject* settings;
  PyObject* callback;
  if (!PyArg_ParseTuple(args, "OO:start_capture", &settings, &callback)) {
    return nullptr;
  }
  if (!PyCallable_Check(callback)) {
    PyErr_SetString(PyExc_TypeError, "callback must be callable");
    return nullptr;
  }

  AudioCaptureSettings cset;  // C++ defaults; overridden below

  // device_name: str/bytes/None -> retained utf8 bytes; pointer held by thread.
  PyObject* dev = PyObject_GetAttrString(settings, "device_name");
  if (!dev) return nullptr;
  PyObject* dev_bytes = nullptr;
  if (dev == Py_None) {
    cset.device_name = nullptr;
  } else if (PyUnicode_Check(dev)) {
    dev_bytes = PyUnicode_AsUTF8String(dev);  // new ref
    if (!dev_bytes) { Py_DECREF(dev); return nullptr; }
  } else if (PyBytes_Check(dev)) {
    dev_bytes = dev;
    Py_INCREF(dev_bytes);
  } else {
    Py_DECREF(dev);
    PyErr_SetString(PyExc_TypeError, "device_name must be str, bytes, or None");
    return nullptr;
  }
  Py_DECREF(dev);
  if (dev_bytes) {
    cset.device_name = PyBytes_AS_STRING(dev_bytes);  // valid while dev_bytes lives
  }

  long lv;
  bool bv;
  if (read_int_attr(settings, "sample_rate", &lv) < 0) goto err;
  cset.sample_rate = (uint32_t)lv;
  if (read_int_attr(settings, "channels", &lv) < 0) goto err;
  cset.channels = (int)lv;
  if (read_int_attr(settings, "opus_bitrate", &lv) < 0) goto err;
  cset.opus_bitrate = (int)lv;
  if (read_int_attr(settings, "frame_duration_ms", &lv) < 0) goto err;
  cset.frame_duration_ms = (int)lv;
  if (read_int_attr(settings, "latency_ms", &lv) < 0) goto err;
  cset.latency_ms = (int)lv;
  if (read_bool_attr(settings, "use_vbr", &bv) < 0) goto err;
  cset.use_vbr = bv;
  if (read_bool_attr(settings, "use_silence_gate", &bv) < 0) goto err;
  cset.use_silence_gate = bv;
  if (read_bool_attr(settings, "debug_logging", &bv) < 0) goto err;
  cset.debug_logging = bv;
  if (read_bool_attr(settings, "omit_audio_header", &bv) < 0) goto err;
  cset.omit_audio_header = bv;
  // deferred_free is forced on: the trampoline always takes ownership. The
  // Python value is ignored (read elsewhere only for API compatibility).
  cset.deferred_free = true;

  // Stop/join any prior capture BEFORE mutating shared state. The old C++ thread
  // can still be reading device_name_bytes/chunk_callback/user_data, so swapping
  // them while it runs is a use-after-free + data race. Releasing the GIL lets a
  // final in-flight trampoline drain and the thread join. Parsing above is all
  // local, so a bad-settings error leaves a running capture untouched.
  Py_BEGIN_ALLOW_THREADS
  cap->module->stop_capture();
  Py_END_ALLOW_THREADS

  // Commit: retain device bytes, callback, wire up module, start. Safe now: no
  // live thread references the old buffer/callback.
  Py_XSETREF(cap->device_name_bytes, dev_bytes);  // steals our ref; clears prior
  dev_bytes = nullptr;
  Py_INCREF(callback);
  Py_XSETREF(cap->callback, callback);
  cap->module->chunk_callback = capture_trampoline;
  cap->module->user_data = cap;
  cap->module->modify_settings(cset);
  Py_BEGIN_ALLOW_THREADS
  cap->module->start_capture();  // only spawns the thread (quick)
  Py_END_ALLOW_THREADS

  // Wait briefly for the thread to publish its startup result so a failed
  // pa_simple_new/opus_encoder_create surfaces as an exception instead of a
  // silent no-op. Bounded so a healthy capture (which transitions to RUNNING
  // after its first read) doesn't block start_capture indefinitely. Scoped so
  // the local doesn't cross the goto into 'err' above.
  {
    CaptureStartState st = cap->module->start_state.load(std::memory_order_acquire);
    if (st == CaptureStartState::STARTING) {
      Py_BEGIN_ALLOW_THREADS
      for (int i = 0; i < 200; ++i) {  // up to ~2s
        st = cap->module->start_state.load(std::memory_order_acquire);
        if (st != CaptureStartState::STARTING) break;
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
      }
      Py_END_ALLOW_THREADS
    }
    if (st == CaptureStartState::FAILED) {
      Py_BEGIN_ALLOW_THREADS
      cap->module->stop_capture();  // join the failed thread
      Py_END_ALLOW_THREADS
      Py_CLEAR(cap->callback);
      PyErr_SetString(PyExc_RuntimeError,
                      "audio capture failed to start (see stderr for details)");
      return nullptr;
    }
  }
  Py_RETURN_NONE;

err:
  Py_XDECREF(dev_bytes);
  return nullptr;
}

static PyObject* AudioCapture_stop_capture(PyObject* self, PyObject* Py_UNUSED(ignored)) {
  AudioCaptureObject* cap = (AudioCaptureObject*)self;
  if (cap->module) {
    Py_BEGIN_ALLOW_THREADS  // joins the thread; release GIL so a final
    cap->module->stop_capture();  // trampoline callback can run
    Py_END_ALLOW_THREADS
  }
  Py_CLEAR(cap->callback);
  Py_RETURN_NONE;
}

static PyObject* AudioCapture_update_bitrate(PyObject* self, PyObject* arg) {
  AudioCaptureObject* cap = (AudioCaptureObject*)self;
  long n = PyLong_AsLong(arg);
  if (n == -1 && PyErr_Occurred()) return nullptr;
  if (cap->module) cap->module->set_bitrate((int)n);
  Py_RETURN_NONE;
}

static PyObject* AudioCapture_get_is_capturing(PyObject* self, void* closure) {
  (void)closure;
  AudioCaptureObject* cap = (AudioCaptureObject*)self;
  bool running = cap->module && cap->module->started_ok.load() &&
                 !cap->module->stop_requested.load();
  return PyBool_FromLong(running ? 1 : 0);
}

static PyMethodDef AudioCapture_methods[] = {
    {"start_capture", AudioCapture_start_capture, METH_VARARGS,
     PyDoc_STR("start_capture(settings, callback): begin capture; callback(frame) per chunk.")},
    {"stop_capture", AudioCapture_stop_capture, METH_NOARGS,
     PyDoc_STR("Stop capture and join the capture thread.")},
    {"update_bitrate", AudioCapture_update_bitrate, METH_O,
     PyDoc_STR("update_bitrate(bps): set the Opus bitrate during capture.")},
    {"update_audio_bitrate", AudioCapture_update_bitrate, METH_O,
     PyDoc_STR("Alias of update_bitrate(bps).")},
    {nullptr, nullptr, 0, nullptr},
};

static PyGetSetDef AudioCapture_getset[] = {
    {"is_capturing", AudioCapture_get_is_capturing, nullptr,
     PyDoc_STR("True while the capture thread is running."), nullptr},
    {nullptr, nullptr, nullptr, nullptr, nullptr},
};

static PyType_Slot AudioCapture_slots[] = {
    {Py_tp_new, (void*)AudioCapture_new},
    {Py_tp_dealloc, (void*)AudioCapture_dealloc},
    {Py_tp_traverse, (void*)AudioCapture_traverse},
    {Py_tp_clear, (void*)AudioCapture_clear},
    {Py_tp_methods, (void*)AudioCapture_methods},
    {Py_tp_getset, (void*)AudioCapture_getset},
    {Py_tp_doc, (void*)PyDoc_STR("PulseAudio capture + Opus encoder.")},
    {0, nullptr},
};

static PyType_Spec AudioCapture_spec = {
    "pcmflux._capture.AudioCapture",
    sizeof(AudioCaptureObject),
    0,
    Py_TPFLAGS_DEFAULT | Py_TPFLAGS_BASETYPE | Py_TPFLAGS_HAVE_GC,
    AudioCapture_slots,
};

// ----------------------------------------------------------------------------
// Module definition
// ----------------------------------------------------------------------------
static struct PyModuleDef capture_module = {
    PyModuleDef_HEAD_INIT,
    "pcmflux._capture",
    PyDoc_STR("Native PulseAudio->Opus capture (full C-API, zero-copy)."),
    -1,
    nullptr, nullptr, nullptr, nullptr, nullptr,
};

PyMODINIT_FUNC PyInit__capture(void) {
  PyObject* m = PyModule_Create(&capture_module);
  if (!m) return nullptr;

  g_AudioFrameType = PyType_FromSpec(&AudioFrame_spec);
  if (!g_AudioFrameType) { Py_DECREF(m); return nullptr; }
  Py_INCREF(g_AudioFrameType);  // module-global keeps it alive forever
  if (PyModule_AddObject(m, "AudioFrame", g_AudioFrameType) < 0) {
    Py_DECREF(g_AudioFrameType);  // undo the AddObject ref we tried to give
    Py_DECREF(g_AudioFrameType);  // undo the global ref
    g_AudioFrameType = nullptr;   // don't leave the file-scope ptr dangling
    Py_DECREF(m);
    return nullptr;
  }

  PyObject* capture_type = PyType_FromSpec(&AudioCapture_spec);
  if (!capture_type) { Py_DECREF(m); return nullptr; }
  if (PyModule_AddObject(m, "AudioCapture", capture_type) < 0) {
    Py_DECREF(capture_type);
    Py_DECREF(m);
    return nullptr;
  }

  return m;
}

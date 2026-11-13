// Screen casts: PipeWire video streams of a monitor, an area of the screen or
// a single window, for org.gnome.Mutter.ScreenCast (src/screencast.rs). The
// GNOME portal (xdg-desktop-portal-gnome) asks for these when a browser, OBS
// or a remote desktop tool wants to share the screen or a window, exactly as
// it asks mutter.
//
// Each stream renders with a screen shooter of its own, straight into the
// PipeWire buffer's memory: an area stream sees the scene as the outputs do,
// a window stream sees only its window (and the window's popups) through the
// scene filter in capture.cpp, so it keeps showing the window while it is
// covered, minimized or on another workspace, as in GNOME.
//
// Frames are rendered when something changed (a client posted a frame, a
// window moved, the pointer moved for streams that show it), at most at the
// rate the consumer asked for, and never while the consumer still holds
// every buffer.

#include "umoja_shim.h"

#include <mir/compositor/scene.h>
#include <mir/compositor/screen_shooter.h>
#include <mir/compositor/screen_shooter_factory.h>
#include <mir/executor.h>
#include <mir/input/cursor_observer.h>
#include <mir/input/cursor_observer_multiplexer.h>
#include <mir/renderer/sw/pixel_source.h>
#include <mir/scene/null_surface_observer.h>
#include <mir/scene/observer.h>
#include <mir/scene/surface.h>
#include <mir/server.h>

#include <pipewire/pipewire.h>
#include <spa/param/buffers.h>
#include <spa/buffer/meta.h>
#include <spa/param/video/format-utils.h>
#include <spa/pod/builder.h>
#include <spa/utils/result.h>

#include <fcntl.h>
#include <sys/mman.h>
#include <unistd.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cmath>
#include <condition_variable>
#include <cstdio>
#include <cstring>
#include <map>
#include <memory>
#include <mutex>
#include <optional>
#include <thread>
#include <vector>

namespace geom = mir::geometry;
namespace mrs = mir::renderer::software;
using namespace std::chrono_literals;

namespace {

mir::Server* g_server = nullptr;

// ---- What changed on screen ------------------------------------------------

// Bumped on every change to the scene (scene) or the pointer (cursor); each
// stream renders when a counter it cares about moved since its last frame.
std::atomic<uint64_t> g_scene_serial{1};
std::atomic<uint64_t> g_cursor_serial{1};
std::mutex g_wake_mutex;
std::condition_variable g_wake;

void wake() {
    // Taking the mutex orders the bump before a waiter's check.
    { std::lock_guard lock{g_wake_mutex}; }
    g_wake.notify_all();
}

void scene_changed() {
    g_scene_serial.fetch_add(1, std::memory_order_release);
    wake();
}

struct SurfaceDamage : mir::scene::NullSurfaceObserver {
    void frame_posted(mir::scene::Surface const*, geom::Rectangle const&) override { scene_changed(); }
    void moved_to(mir::scene::Surface const*, geom::Point const&) override { scene_changed(); }
    void window_resized_to(mir::scene::Surface const*, geom::Size const&) override { scene_changed(); }
    void content_resized_to(mir::scene::Surface const*, geom::Size const&) override { scene_changed(); }
    void hidden_set_to(mir::scene::Surface const*, bool) override { scene_changed(); }
    void alpha_set_to(mir::scene::Surface const*, float) override { scene_changed(); }
    void transformation_set_to(mir::scene::Surface const*, glm::mat4 const&) override { scene_changed(); }
};

struct SceneDamage : mir::scene::Observer {
    std::shared_ptr<SurfaceDamage> const surfaces = std::make_shared<SurfaceDamage>();

    void surface_added(std::shared_ptr<mir::scene::Surface> const& surface) override {
        surface->register_interest(surfaces);
        scene_changed();
    }
    void surface_removed(std::shared_ptr<mir::scene::Surface> const& surface) override {
        surface->unregister_interest(*surfaces);
        scene_changed();
    }
    void surfaces_reordered(mir::scene::SurfaceSet const&) override { scene_changed(); }
    void scene_changed() override { ::scene_changed(); }
    void surface_exists(std::shared_ptr<mir::scene::Surface> const& surface) override {
        surface->register_interest(surfaces);
    }
    void end_observation() override {}
};

struct CursorDamage : mir::input::CursorObserver {
    void bump() {
        g_cursor_serial.fetch_add(1, std::memory_order_release);
        wake();
    }
    void cursor_moved_to(float, float) override { bump(); }
    void pointer_usable() override { bump(); }
    void pointer_unusable() override { bump(); }
    void image_set_to(std::shared_ptr<mir::graphics::CursorImage>) override { bump(); }
};

// Installed with the first screen cast and kept: observing costs next to
// nothing, and adding and removing scene observers while clients draw is
// what wlr-screencopy does around the frozen-surface bug (see memory notes).
std::once_flag g_observers_once;
std::shared_ptr<SceneDamage> g_scene_damage;
std::shared_ptr<CursorDamage> g_cursor_damage;

void observe_damage() {
    std::call_once(g_observers_once, [] {
        g_scene_damage = std::make_shared<SceneDamage>();
        g_cursor_damage = std::make_shared<CursorDamage>();
        g_server->the_scene()->add_observer(g_scene_damage);
        g_server->the_cursor_observer_multiplexer()->register_interest(g_cursor_damage, mir::immediate_executor);
    });
}

// ---- Screen shooters --------------------------------------------------------

// Shooters are reused rather than destroyed: each owns a GL renderer per
// size, and tearing one down from an arbitrary thread is not something Mir
// promises to handle.
std::mutex g_shooters_mutex;
std::vector<std::unique_ptr<mir::compositor::ScreenShooter>> g_free_shooters;

std::unique_ptr<mir::compositor::ScreenShooter> take_shooter() {
    {
        std::lock_guard lock{g_shooters_mutex};
        if (!g_free_shooters.empty()) {
            auto s = std::move(g_free_shooters.back());
            g_free_shooters.pop_back();
            return s;
        }
    }
    return g_server->the_screen_shooter_factory()->create(mir::immediate_executor);
}

void give_back_shooter(std::unique_ptr<mir::compositor::ScreenShooter> s) {
    if (!s) return;
    std::lock_guard lock{g_shooters_mutex};
    g_free_shooters.push_back(std::move(s));
}

// A PipeWire buffer's memory, as somewhere for Mir to render to.
class BufferTarget : public mrs::WriteMappable {
public:
    BufferTarget(void* data, geom::Size size, uint32_t stride) : data{data}, size_{size}, stride_{stride} {}

    auto map_writeable() -> std::unique_ptr<mrs::Mapping<std::byte>> override {
        return std::make_unique<Map>(static_cast<std::byte*>(data), size_, stride_);
    }
    MirPixelFormat format() const override { return mir_pixel_format_argb_8888; }
    geom::Stride stride() const override { return geom::Stride{stride_}; }
    geom::Size size() const override { return size_; }

private:
    struct Map : mrs::Mapping<std::byte> {
        Map(std::byte* p, geom::Size s, uint32_t stride) : p{p}, s{s}, st{stride} {}
        auto data() const -> std::byte* override { return p; }
        auto len() const -> size_t override { return (size_t)st * s.height.as_uint32_t(); }
        MirPixelFormat format() const override { return mir_pixel_format_argb_8888; }
        geom::Stride stride() const override { return geom::Stride{st}; }
        geom::Size size() const override { return s; }
        std::byte* p;
        geom::Size s;
        uint32_t st;
    };
    void* data;
    geom::Size size_;
    uint32_t stride_;
};

// ---- PipeWire ---------------------------------------------------------------

std::mutex g_pw_mutex;
pw_thread_loop* g_loop = nullptr;
pw_context* g_context = nullptr;
pw_core* g_core = nullptr;
spa_hook g_core_listener;

// Wait until the loop thread has run everything queued on it so far. Never
// call it with the loop locked or from the loop thread.
int noop_invoke(spa_loop*, bool, uint32_t, void const*, size_t, void*) { return 0; }
void flush_loop() {
    pw_loop_invoke(pw_thread_loop_get_loop(g_loop), noop_invoke, 0, nullptr, 0, true, nullptr);
}

struct LoopLock {
    LoopLock() { pw_thread_loop_lock(g_loop); }
    ~LoopLock() { pw_thread_loop_unlock(g_loop); }
};

void on_core_error(void*, uint32_t id, int, int res, char const* message) {
    std::fprintf(stderr, "umoja-mir: PipeWire error (object %u): %s (%s)\n", id, message, spa_strerror(res));
}

pw_core_events const core_events = [] {
    pw_core_events e{};
    e.version = PW_VERSION_CORE_EVENTS;
    e.error = on_core_error;
    return e;
}();

// Connect to the user's PipeWire on first use. False when it isn't running.
bool pipewire_ready() {
    std::lock_guard lock{g_pw_mutex};
    if (g_core) return true;
    static std::once_flag init;
    std::call_once(init, [] { pw_init(nullptr, nullptr); });
    if (!g_loop) {
        g_loop = pw_thread_loop_new("umoja-screencast", nullptr);
        if (!g_loop) return false;
        g_context = pw_context_new(pw_thread_loop_get_loop(g_loop), nullptr, 0);
        if (!g_context || pw_thread_loop_start(g_loop) < 0) {
            std::fprintf(stderr, "umoja-mir: could not start the PipeWire loop\n");
            return false;
        }
    }
    LoopLock loop_lock;
    g_core = pw_context_connect(g_context, nullptr, 0);
    if (!g_core) {
        std::fprintf(stderr, "umoja-mir: could not connect to PipeWire: %s\n", std::strerror(errno));
        return false;
    }
    pw_core_add_listener(g_core, &g_core_listener, &core_events, nullptr);
    return true;
}

// ---- Streams ----------------------------------------------------------------

uint32_t constexpr bytes_per_pixel = 4;

class Cast {
public:
    Cast(uint64_t handle, UmojaCastParams const& p) : handle{handle}, params{p} {
        if (params.max_fps == 0) params.max_fps = 30;
        params.max_fps = std::clamp<uint32_t>(params.max_fps, 1, 60);
        if (!(params.scale > 0)) params.scale = 1.0;
        source = params.area;
        if (params.kind == UMOJA_CAST_WINDOW) {
            UmojaRect r;
            if (umoja_wm_window_rect(params.window, &r)) source = r;
        }
        auto const [w, h] = pixel_size(source);
        want_w = negotiated_w = w;
        want_h = negotiated_h = h;
    }

    ~Cast() { stop(); }

    bool start() {
        if (source.w <= 0 || source.h <= 0) return false;
        {
            LoopLock lock;
            auto* props = pw_properties_new(PW_KEY_MEDIA_CLASS, "Video/Source", PW_KEY_MEDIA_ROLE, "Screen",
                                            PW_KEY_NODE_DESCRIPTION, "Umoja screen cast", nullptr);
            pw_properties_setf(props, PW_KEY_NODE_NAME, "umoja-screencast-%llu", (unsigned long long)handle);
            stream = pw_stream_new(g_core, "umoja-screencast", props);
            if (!stream) return false;
            pw_stream_add_listener(stream, &listener, &stream_events, this);
            uint8_t buf[1024];
            spa_pod_builder b = SPA_POD_BUILDER_INIT(buf, sizeof buf);
            spa_pod const* fmt = enum_format(&b, want_w, want_h);
            int const res = pw_stream_connect(
                stream, PW_DIRECTION_OUTPUT, PW_ID_ANY,
                static_cast<pw_stream_flags>(PW_STREAM_FLAG_DRIVER | PW_STREAM_FLAG_ALLOC_BUFFERS), &fmt, 1);
            if (res < 0) {
                std::fprintf(stderr, "umoja-mir: screen cast stream failed to connect: %s\n", spa_strerror(res));
                return false;
            }
        }
        running = true;
        thread = std::thread{[this] { run(); }};
        return true;
    }

    void stop() {
        if (running.exchange(false)) {
            wake();
            if (thread.joinable()) thread.join();
        }
        if (stream) {
            // pw_stream_trigger_process() and the stream's own callbacks
            // queue invokes on the loop that point at the stream, and
            // pw_stream_destroy() only flushes those when it runs on the
            // loop thread. Destroying it from here with one still queued
            // let the loop thread run it on freed memory: the crash on
            // "Stop presenting" (SIGSEGV in libpipewire on the
            // umoja-screencast thread). So drain the queue around each step.
            flush_loop();
            {
                LoopLock lock;
                // Disconnect with the listener still in place so
                // remove_buffer frees our memfds; this stop is not news to
                // the Rust side.
                reported = false;
                pw_stream_disconnect(stream);
            }
            flush_loop();
            {
                LoopLock lock;
                spa_hook_remove(&listener);
                pw_stream_destroy(stream);
                stream = nullptr;
            }
            flush_loop();
        }
        if (shooter) {
            umoja_scene_filter_set(shooter->id(), nullptr);
            give_back_shooter(std::move(shooter));
        }
    }

    uint64_t const handle;

private:
    UmojaCastParams params;
    UmojaRect source{};  // what is shown, in layout coordinates
    pw_stream* stream = nullptr;
    spa_hook listener{};
    std::thread thread;
    std::atomic<bool> running{false};
    std::unique_ptr<mir::compositor::ScreenShooter> shooter;

    // PipeWire's side, written on its loop thread.
    std::atomic<bool> streaming{false};
    std::atomic<bool> reported{false};
    std::atomic<int> negotiated_w{0}, negotiated_h{0};
    std::atomic<uint64_t> format_serial{0};

    // The frame thread's own.
    int want_w = 0, want_h = 0;
    std::chrono::steady_clock::time_point size_changed_at{};
    uint64_t seen_scene = 0, seen_cursor = 0, seen_format = 0;
    uint64_t sequence = 0;
    bool window_lost = false;

    std::pair<int, int> pixel_size(UmojaRect r) const {
        return {std::max(1, (int)std::lround(r.w * params.scale)), std::max(1, (int)std::lround(r.h * params.scale))};
    }

    bool cursor_shown() const { return params.cursor_mode != UMOJA_CAST_CURSOR_HIDDEN; }

    spa_pod const* enum_format(spa_pod_builder* b, int w, int h) const {
        spa_rectangle size{(uint32_t)w, (uint32_t)h};
        spa_fraction none{0, 1};
        spa_fraction max_rate{params.max_fps, 1};
        spa_fraction min_rate{1, 1};
        return static_cast<spa_pod const*>(spa_pod_builder_add_object(
            b, SPA_TYPE_OBJECT_Format, SPA_PARAM_EnumFormat, SPA_FORMAT_mediaType, SPA_POD_Id(SPA_MEDIA_TYPE_video),
            SPA_FORMAT_mediaSubtype, SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw), SPA_FORMAT_VIDEO_format,
            SPA_POD_CHOICE_ENUM_Id(3, SPA_VIDEO_FORMAT_BGRx, SPA_VIDEO_FORMAT_BGRx, SPA_VIDEO_FORMAT_BGRA),
            SPA_FORMAT_VIDEO_size, SPA_POD_Rectangle(&size), SPA_FORMAT_VIDEO_framerate, SPA_POD_Fraction(&none),
            SPA_FORMAT_VIDEO_maxFramerate, SPA_POD_CHOICE_RANGE_Fraction(&max_rate, &min_rate, &max_rate)));
    }

    // ---- PipeWire callbacks (loop thread) ----

    static void on_state_changed(void* data, pw_stream_state, pw_stream_state state, char const* error) {
        auto* self = static_cast<Cast*>(data);
        switch (state) {
        case PW_STREAM_STATE_PAUSED:
            self->streaming = false;
            if (!self->reported.exchange(true)) {
                uint32_t const node = pw_stream_get_node_id(self->stream);
                uint64_t serial = node;
                if (auto const* props = pw_stream_get_properties(self->stream))
                    if (char const* s = pw_properties_get(props, PW_KEY_OBJECT_SERIAL))
                        serial = std::strtoull(s, nullptr, 10);
                umoja_cast_node_added(self->handle, node, serial);
            }
            break;
        case PW_STREAM_STATE_STREAMING:
            self->streaming = true;
            wake();
            break;
        case PW_STREAM_STATE_ERROR:
            std::fprintf(stderr, "umoja-mir: screen cast stream error: %s\n", error ? error : "unknown");
            self->streaming = false;
            umoja_cast_closed(self->handle);
            break;
        case PW_STREAM_STATE_UNCONNECTED:
            self->streaming = false;
            if (self->reported) umoja_cast_closed(self->handle);
            break;
        default:
            break;
        }
    }

    static void on_param_changed(void* data, uint32_t id, spa_pod const* param) {
        auto* self = static_cast<Cast*>(data);
        if (!param || id != SPA_PARAM_Format) return;
        spa_video_info_raw info{};
        if (spa_format_video_raw_parse(param, &info) < 0 || info.size.width == 0 || info.size.height == 0) return;
        uint32_t const stride = info.size.width * bytes_per_pixel;
        uint8_t buf[1024];
        spa_pod_builder b = SPA_POD_BUILDER_INIT(buf, sizeof buf);
        spa_pod const* params[2];
        params[0] = static_cast<spa_pod const*>(spa_pod_builder_add_object(
            &b, SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers, SPA_PARAM_BUFFERS_buffers,
            SPA_POD_CHOICE_RANGE_Int(4, 2, 8), SPA_PARAM_BUFFERS_blocks, SPA_POD_Int(1), SPA_PARAM_BUFFERS_size,
            SPA_POD_Int((int)(stride * info.size.height)), SPA_PARAM_BUFFERS_stride, SPA_POD_Int((int)stride),
            SPA_PARAM_BUFFERS_dataType, SPA_POD_CHOICE_FLAGS_Int((1 << SPA_DATA_MemFd) | (1 << SPA_DATA_MemPtr))));
        params[1] = static_cast<spa_pod const*>(spa_pod_builder_add_object(
            &b, SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta, SPA_PARAM_META_type, SPA_POD_Id(SPA_META_Header),
            SPA_PARAM_META_size, SPA_POD_Int((int)sizeof(spa_meta_header))));
        pw_stream_update_params(self->stream, params, 2);
        self->negotiated_w = (int)info.size.width;
        self->negotiated_h = (int)info.size.height;
        self->format_serial.fetch_add(1);
        wake();
    }

    static void on_add_buffer(void* data, pw_buffer* buffer) {
        auto* self = static_cast<Cast*>(data);
        spa_data& d = buffer->buffer->datas[0];
        size_t const size = (size_t)self->negotiated_w * bytes_per_pixel * self->negotiated_h;
        d.maxsize = (uint32_t)size;
        if (d.type & (1u << SPA_DATA_MemFd) || d.type == SPA_ID_INVALID || d.type == 0) {
            int const fd = memfd_create("umoja-screencast", MFD_CLOEXEC | MFD_ALLOW_SEALING);
            if (fd >= 0 && ftruncate(fd, (off_t)size) == 0) {
                void* p = mmap(nullptr, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
                if (p != MAP_FAILED) {
                    fcntl(fd, F_ADD_SEALS, F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL);
                    d.type = SPA_DATA_MemFd;
                    d.flags = SPA_DATA_FLAG_READWRITE;
                    d.fd = fd;
                    d.mapoffset = 0;
                    d.data = p;
                    return;
                }
            }
            if (fd >= 0) close(fd);
        }
        // MemPtr only.
        d.type = SPA_DATA_MemPtr;
        d.flags = SPA_DATA_FLAG_READWRITE;
        d.fd = -1;
        d.data = std::malloc(size);
    }

    static void on_remove_buffer(void*, pw_buffer* buffer) {
        spa_data& d = buffer->buffer->datas[0];
        if (d.type == SPA_DATA_MemFd) {
            if (d.data) munmap(d.data, d.maxsize);
            if (d.fd >= 0) close((int)d.fd);
        } else if (d.type == SPA_DATA_MemPtr) {
            std::free(d.data);
        }
        d.data = nullptr;
        d.fd = -1;
    }

    static constexpr pw_stream_events stream_events = [] {
        pw_stream_events e{};
        e.version = PW_VERSION_STREAM_EVENTS;
        e.state_changed = on_state_changed;
        e.param_changed = on_param_changed;
        e.add_buffer = on_add_buffer;
        e.remove_buffer = on_remove_buffer;
        return e;
    }();

    // ---- Frame thread ----

    void run() {
        auto const interval = std::chrono::nanoseconds{1'000'000'000 / params.max_fps};
        auto last_frame = std::chrono::steady_clock::now() - interval;
        while (running) {
            {
                std::unique_lock lock{g_wake_mutex};
                // Windows are also checked now and then: their size decides
                // the stream's, and a closed one ends the stream. Only a
                // stream that is running has frames due: one still waiting
                // for its consumer (or paused by it) always has changes it
                // hasn't sent, and treating that as due made this loop spin
                // on a whole core until the consumer connected.
                g_wake.wait_for(lock, params.kind == UMOJA_CAST_WINDOW ? 250ms : 1s,
                                [this] { return !running || (streaming && due()); });
            }
            if (!running) break;
            auto const now = std::chrono::steady_clock::now();
            if (now - last_frame < interval) {
                std::this_thread::sleep_for(interval - (now - last_frame));
                if (!running) break;
            }
            if (params.kind == UMOJA_CAST_WINDOW && !follow_window()) break;
            if (!streaming || !due()) continue;
            last_frame = std::chrono::steady_clock::now();
            frame();
        }
    }

    bool due() const {
        if (format_serial.load() != seen_format) return true;
        if (g_scene_serial.load(std::memory_order_acquire) != seen_scene) return true;
        return cursor_shown() && g_cursor_serial.load(std::memory_order_acquire) != seen_cursor;
    }

    // Track the window: false once it is gone (the stream ends, as in GNOME).
    bool follow_window() {
        UmojaRect r;
        if (!umoja_wm_window_rect(params.window, &r) || r.w <= 0 || r.h <= 0) {
            if (!window_lost) {
                window_lost = true;
                umoja_cast_closed(handle);
            }
            return false;
        }
        source = r;
        auto const [w, h] = pixel_size(r);
        auto const now = std::chrono::steady_clock::now();
        if (w != want_w || h != want_h) {
            want_w = w;
            want_h = h;
            size_changed_at = now;
        } else if ((w != negotiated_w || h != negotiated_h) && size_changed_at != decltype(size_changed_at){} &&
                   now - size_changed_at > 300ms) {
            // Settled on a new size: ask the consumer for it. Until it agrees
            // (and while the window is being resized) frames are scaled.
            size_changed_at = {};
            LoopLock lock;
            uint8_t buf[1024];
            spa_pod_builder b = SPA_POD_BUILDER_INIT(buf, sizeof buf);
            spa_pod const* fmt = enum_format(&b, w, h);
            pw_stream_update_params(stream, &fmt, 1);
        }
        return true;
    }

    void frame() {
        seen_format = format_serial.load();
        seen_scene = g_scene_serial.load(std::memory_order_acquire);
        seen_cursor = g_cursor_serial.load(std::memory_order_acquire);

        // The loop lock is held until the buffer is queued again: PipeWire
        // frees the buffers on its loop thread when the consumer goes away
        // or renegotiates ("Stop presenting" in a browser), and rendering
        // into, or queueing, a buffer freed under us crashed the compositor.
        LoopLock lock;
        if (!streaming) return;
        pw_buffer* const buffer = pw_stream_dequeue_buffer(stream);
        if (!buffer) {
            // The consumer holds them all; try again with the next change.
            seen_scene = 0;
            return;
        }
        spa_buffer* const sb = buffer->buffer;
        spa_data& d = sb->datas[0];
        int const w = negotiated_w, h = negotiated_h;
        uint32_t const stride = (uint32_t)w * bytes_per_pixel;
        bool ok = d.data && (size_t)stride * h <= d.maxsize;
        std::optional<mir::time::Timestamp> when;
        if (ok) {
            try {
                if (!shooter) shooter = take_shooter();
                auto const target = std::make_shared<BufferTarget>(d.data, geom::Size{w, h}, stride);
                geom::Rectangle const area{{source.x, source.y}, {source.w, source.h}};
                if (params.kind == UMOJA_CAST_WINDOW)
                    umoja_scene_filter_set(shooter->id(), reinterpret_cast<void const*>(params.window));
                shooter->capture(target, area, glm::mat2{1.0f}, cursor_shown(),
                                 [&when](std::optional<mir::time::Timestamp> t) { when = t; });
                ok = when.has_value();
                if (ok && params.kind == UMOJA_CAST_WINDOW && !umoja_scene_filter_consulted(shooter->id())) {
                    static std::once_flag warned;
                    std::call_once(warned, [] {
                        std::fprintf(stderr,
                                     "umoja-mir: window screen casts need Mir's scene filter hook (Umoja's Mir)\n");
                    });
                    ok = false;
                }
            } catch (std::exception const& e) {
                std::fprintf(stderr, "umoja-mir: screen cast frame failed: %s\n", e.what());
                ok = false;
            }
        }
        d.chunk->offset = 0;
        d.chunk->stride = (int32_t)stride;
        d.chunk->size = ok ? stride * (uint32_t)h : 0;
        d.chunk->flags = ok ? SPA_CHUNK_FLAG_NONE : SPA_CHUNK_FLAG_CORRUPTED;
        if (auto* header = static_cast<spa_meta_header*>(spa_buffer_find_meta_data(sb, SPA_META_Header,
                                                                                    sizeof(spa_meta_header)))) {
            header->flags = ok ? 0 : SPA_META_HEADER_FLAG_CORRUPTED;
            header->offset = 0;
            header->pts = when ? std::chrono::duration_cast<std::chrono::nanoseconds>(when->time_since_epoch()).count()
                               : -1;
            header->dts_offset = 0;
            header->seq = sequence++;
        }
        pw_stream_queue_buffer(stream, buffer);
        // We drive the graph: run a cycle so the consumer gets the frame.
        if (pw_stream_is_driving(stream)) pw_stream_trigger_process(stream);
    }
};

std::mutex g_casts_mutex;
std::map<uint64_t, std::unique_ptr<Cast>> g_casts;

}  // namespace

void umoja_screencast_setup(mir::Server& server) {
    server.add_init_callback([&server] { g_server = &server; });
    // Spare shooters go while Mir is still there, not in exit() after it
    // has unloaded the graphics platform (see capture.cpp).
    server.add_stop_callback([] {
        std::lock_guard lock{g_shooters_mutex};
        g_free_shooters.clear();
    });
}

extern "C" bool umoja_cast_start(uint64_t handle, UmojaCastParams const* params) {
    if (!g_server || !params || handle == 0) return false;
    try {
        if (!pipewire_ready()) return false;
        observe_damage();
        {
            std::lock_guard lock{g_casts_mutex};
            if (g_casts.count(handle)) return false;
        }
        auto cast = std::make_unique<Cast>(handle, *params);
        if (!cast->start()) return false;
        std::lock_guard lock{g_casts_mutex};
        g_casts.emplace(handle, std::move(cast));
        return true;
    } catch (std::exception const& e) {
        std::fprintf(stderr, "umoja-mir: screen cast failed to start: %s\n", e.what());
        return false;
    }
}

extern "C" void umoja_cast_stop(uint64_t handle) {
    std::unique_ptr<Cast> cast;
    {
        std::lock_guard lock{g_casts_mutex};
        auto const it = g_casts.find(handle);
        if (it == g_casts.end()) return;
        cast = std::move(it->second);
        g_casts.erase(it);
    }
    // Joins the frame thread: never call this from it (or the loop thread).
    cast.reset();
}

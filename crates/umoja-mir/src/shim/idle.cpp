// Screen blanking: drives Mir's idle handler from GNOME's settings.
//
// Mir already knows how to dim the screen, turn the outputs off (DPMS) and
// wake them on input, and it honours Wayland idle inhibitors
// (zwp_idle_inhibit_manager_v1). It only needs to be told the timeouts, and
// those live in GSettings, so the Rust side (`idle.rs`) works them out and
// calls umoja_idle_set_timeouts() whenever a setting or a D-Bus inhibitor
// changes. Mir's state changes (dim, off, wake) are reported back so Rust
// can lock the session after the screen blanks, as GNOME does.
//
// Mir's IdleHandler is only reachable through mir::Server, whose header is
// in libmirserver-internal-dev. Kept in its own file so the server headers
// don't mix with MirAL's toolkit API in umoja_shim.cpp.

#include "umoja_shim.h"

#include <mir/graphics/cursor.h>
#include <mir/server.h>
#include <mir/scene/session_lock.h>
#include <mir/shell/idle_handler.h>
#include <mir_toolkit/events/event.h>

#include <chrono>
#include <cstdio>
#include <memory>
#include <mutex>
#include <optional>

namespace {

struct Observer : mir::shell::IdleHandlerObserver {
    void dim() override { umoja_idle_state(UMOJA_IDLE_DIM); }
    void off() override { umoja_idle_state(UMOJA_IDLE_OFF); }
    void wake() override { umoja_idle_state(UMOJA_IDLE_WAKE); }
};

struct LockObserver : mir::scene::SessionLockObserver {
    void on_lock() override { umoja_idle_state(UMOJA_IDLE_LOCKED); }
    void on_unlock() override { umoja_idle_state(UMOJA_IDLE_UNLOCKED); }
};

std::mutex g_mutex;
std::shared_ptr<LockObserver> const g_lock_observer = std::make_shared<LockObserver>();
std::weak_ptr<mir::shell::IdleHandler> g_handler;
std::shared_ptr<Observer> const g_observer = std::make_shared<Observer>();
// Last requested timeouts, in seconds (0 = never). Kept so a request made
// before Mir has started is applied once it has.
int32_t g_off = 0;
int32_t g_locked_off = 0;

std::optional<mir::time::Duration> seconds(int32_t s) {
    if (s <= 0) return std::nullopt;
    return std::chrono::seconds{s};
}

void apply(std::shared_ptr<mir::shell::IdleHandler> const& handler) {
    // Mir ignores a timeout equal to the current one, so this never wakes
    // a blank screen unless the timeout really changed.
    handler->set_display_off_timeout(seconds(g_off));
    handler->set_display_off_timeout_when_locked(seconds(g_locked_off));
}

// Mir's pointer, wrapped so it can be hidden while the screensaver shows.
// The screensaver asks for no cursor itself, but a client only gets to set
// the pointer image after wl_pointer.enter, and Mir sends that on the first
// motion, which also dismisses the screensaver. So the compositor hides it.
// Mir keeps calling show() as the image under the pointer changes; while
// hidden those only update the image to bring back afterwards.
struct HideableCursor : mir::graphics::Cursor {
    explicit HideableCursor(std::shared_ptr<mir::graphics::Cursor> wrapped)
        : wrapped{std::move(wrapped)} {}

    void show(std::shared_ptr<mir::graphics::CursorImage> const& image) override {
        std::lock_guard lock{mutex};
        last = image;
        if (!forced_hidden) wrapped->show(image);
    }
    void hide() override {
        std::lock_guard lock{mutex};
        last.reset();
        wrapped->hide();
    }
    void move_to(mir::geometry::Point position) override { wrapped->move_to(position); }
    void scale(float new_scale) override { wrapped->scale(new_scale); }
    auto renderable() -> std::shared_ptr<mir::graphics::Renderable> override {
        return wrapped->renderable();
    }
    auto needs_compositing() const -> bool override { return wrapped->needs_compositing(); }

    void set_forced_hidden(bool hidden) {
        std::lock_guard lock{mutex};
        if (hidden == forced_hidden) return;
        forced_hidden = hidden;
        if (hidden) {
            wrapped->hide();
        } else if (last) {
            wrapped->show(last);
        }
    }

    std::shared_ptr<mir::graphics::Cursor> const wrapped;
    std::mutex mutex;
    std::shared_ptr<mir::graphics::CursorImage> last;
    bool forced_hidden = false;
};

std::mutex g_cursor_mutex;
std::weak_ptr<HideableCursor> g_cursor;
bool g_cursor_hidden = false;

} // namespace

extern "C" void umoja_cursor_set_hidden(bool hidden) {
    std::lock_guard lock{g_cursor_mutex};
    g_cursor_hidden = hidden;
    if (auto const cursor = g_cursor.lock()) {
        try {
            cursor->set_forced_hidden(hidden);
        } catch (std::exception const& e) {
            std::fprintf(stderr, "umoja-mir: %s the pointer failed: %s\n",
                         hidden ? "hiding" : "showing", e.what());
        }
    }
}

void umoja_idle_setup(mir::Server& server) {
    server.wrap_cursor([](std::shared_ptr<mir::graphics::Cursor> const& wrapped)
                           -> std::shared_ptr<mir::graphics::Cursor> {
        auto const cursor = std::make_shared<HideableCursor>(wrapped);
        std::lock_guard lock{g_cursor_mutex};
        g_cursor = cursor;
        if (g_cursor_hidden) cursor->set_forced_hidden(true);
        return cursor;
    });
    server.add_init_callback([&server] {
        auto const handler = server.the_idle_handler();
        handler->register_interest(g_observer);
        server.the_session_lock()->register_interest(g_lock_observer);
        std::lock_guard lock{g_mutex};
        g_handler = handler;
        apply(handler);
    });
    server.add_stop_callback([] {
        std::lock_guard lock{g_mutex};
        g_handler.reset();
    });
}

extern "C" void umoja_idle_set_timeouts(int32_t off_seconds, int32_t locked_off_seconds) {
    std::lock_guard lock{g_mutex};
    g_off = off_seconds;
    g_locked_off = locked_off_seconds;
    if (auto const handler = g_handler.lock()) {
        try {
            apply(handler);
        } catch (std::exception const& e) {
            std::fprintf(stderr, "umoja-mir: setting idle timeouts failed: %s\n", e.what());
        }
    }
}

// Every input event, before anything else sees it: tells idle.rs the user
// is back while the screensaver is up. Never consumes the event.
bool umoja_idle_input_filter(MirEvent const* event) {
    if (mir_event_get_type(event) == mir_event_type_input) umoja_input_activity();
    return false;
}

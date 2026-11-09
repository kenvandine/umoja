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

} // namespace

void umoja_idle_setup(mir::Server& server) {
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

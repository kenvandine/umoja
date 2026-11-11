// xdg-session-management-v1, as mutter implements it
// (src/wayland/meta-wayland-xdg-session*.c, src/core/meta-session-manager.c).
//
// A client asks for a session (new, or one it saved the id of last time),
// adds its toplevels to it under names of its choosing, and on its next run
// asks for each toplevel to be restored before mapping it. For every
// toplevel in a session we keep what mutter keeps: floating geometry,
// maximized / tiled / fullscreen, minimized and workspace, plus the monitor.
// Sessions are files under $XDG_DATA_HOME/umoja/sessions/.
//
// Threads: the protocol handlers run on Mir's Wayland thread. The window
// manager calls take_pending()/bind()/update()/window_closed() under its own
// lock. Both take `mutex`; nothing here takes the window manager's lock while
// holding it (the writer thread takes the WM lock first, through the
// snapshotter, and only then `mutex`).

#include "session_management.h"

#include <miral/wayland_extensions.h>

#include <wayland-server-core.h>

extern "C" {
#include "xdg-session-management-v1-server-protocol.h"
}

#include <atomic>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <deque>
#include <filesystem>
#include <fstream>
#include <map>
#include <mutex>
#include <random>
#include <set>
#include <sstream>
#include <thread>

namespace geom = mir::geometry;

// The xdg_toplevel argument of add_toplevel/restore_toplevel is typed with
// xdg-shell's interface. Mir's own xdg-shell code names its interface data
// differently, and libwayland only compares interface *names* when it checks
// an object argument, so this stand-in is all the generated code needs.
extern "C" {
extern const struct wl_interface xdg_toplevel_interface;
const struct wl_interface xdg_toplevel_interface = {"xdg_toplevel", 6, 0, nullptr, 0, nullptr};
}

namespace umoja_sm {
namespace {

constexpr int FORMAT_VERSION = 1;
// mutter keeps sessions forever unless removed; we drop ones nobody has used
// for a long time so the directory doesn't grow without bound.
constexpr auto SESSION_MAX_AGE = std::chrono::hours(24 * 90);
constexpr auto PENDING_MAX_AGE = std::chrono::seconds(5);

struct Session;

// One toplevel in a session: the xdg_toplevel_session_v1 object.
struct Binding {
    uint64_t token = 0;
    Session* session = nullptr;
    std::string name;
    wl_resource* toplevel = nullptr;          // the client's xdg_toplevel
    wl_resource* resource = nullptr;          // xdg_toplevel_session_v1
    wl_client* client = nullptr;
    miral::Window window;                     // once Mir has created it
    bool claimed = false;                     // handed to the window manager already
    std::optional<SavedToplevel> restore;     // restore_toplevel with known state
    std::chrono::steady_clock::time_point created;
    wl_listener toplevel_destroyed{};
};

struct Session {
    std::string id;
    wl_resource* resource = nullptr;          // xdg_session_v1, null when inert
    wl_client* client = nullptr;
    std::map<std::string, SavedToplevel> toplevels;
    std::set<Binding*> bindings;
    bool dirty = false;
};

std::mutex mutex;
std::map<std::string, std::unique_ptr<Session>> sessions; // loaded or created this run
std::map<uint64_t, Binding*> bindings_by_token;
uint64_t next_token = 1;
std::function<void()> snapshotter;
std::atomic<bool> save_scheduled{false};

// ---- storage ---------------------------------------------------------------

std::filesystem::path session_dir() {
    char const* data = std::getenv("XDG_DATA_HOME");
    std::filesystem::path base;
    if (data && *data) {
        base = data;
    } else {
        char const* home = std::getenv("HOME");
        base = std::filesystem::path{home ? home : "/tmp"} / ".local" / "share";
    }
    return base / "umoja" / "sessions";
}

// Our ids are 32 hex digits; anything else can't name a file of ours.
bool valid_id(std::string const& id) {
    if (id.size() != 32) return false;
    for (char c : id)
        if (!((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f'))) return false;
    return true;
}

std::string new_id() {
    std::random_device rd;
    std::mt19937_64 gen{(uint64_t(rd()) << 32) ^ rd()};
    static char const hex[] = "0123456789abcdef";
    for (;;) {
        std::string id;
        for (int i = 0; i < 32; ++i) id += hex[gen() & 15];
        std::error_code ec;
        if (!sessions.count(id) && !std::filesystem::exists(session_dir() / id, ec)) return id;
    }
}

// Names are any UTF-8 string; keep them on one tab-separated line.
std::string escape(std::string const& s) {
    std::string out;
    char buf[4];
    for (unsigned char c : s) {
        if (c <= 0x20 || c == '%' || c == 0x7f) {
            std::snprintf(buf, sizeof buf, "%%%02x", c);
            out += buf;
        } else {
            out += static_cast<char>(c);
        }
    }
    return out;
}

std::string unescape(std::string const& s) {
    std::string out;
    for (size_t i = 0; i < s.size(); ++i) {
        if (s[i] == '%' && i + 2 < s.size()) {
            out += static_cast<char>(std::strtol(s.substr(i + 1, 2).c_str(), nullptr, 16));
            i += 2;
        } else {
            out += s[i];
        }
    }
    return out;
}

std::string rect_str(geom::Rectangle const& r) {
    return std::to_string(r.top_left.x.as_int()) + " " + std::to_string(r.top_left.y.as_int()) + " " +
           std::to_string(r.size.width.as_int()) + " " + std::to_string(r.size.height.as_int());
}

bool parse_rect(std::string const& s, geom::Rectangle& r) {
    std::istringstream in{s};
    int x, y, w, h;
    if (!(in >> x >> y >> w >> h) || w <= 0 || h <= 0) return false;
    r = geom::Rectangle{geom::Point{x, y}, geom::Size{w, h}};
    return true;
}

std::vector<std::string> split_tabs(std::string const& line) {
    std::vector<std::string> out;
    size_t start = 0;
    for (;;) {
        auto const tab = line.find('\t', start);
        out.push_back(line.substr(start, tab - start));
        if (tab == std::string::npos) break;
        start = tab + 1;
    }
    return out;
}

// One line per toplevel:
// name \t state \t tile action \t floating rect \t minimized \t workspace \t output \t output rect
bool load(Session& s) {
    std::ifstream in{session_dir() / s.id};
    if (!in) return false;
    std::string line;
    if (!std::getline(in, line) || line != "umoja-session " + std::to_string(FORMAT_VERSION)) return false;
    while (std::getline(in, line)) {
        auto const f = split_tabs(line);
        if (f.size() < 8) continue;
        SavedToplevel t;
        int const state = std::atoi(f[1].c_str());
        if (state < 0 || state > 3) continue;
        t.state = static_cast<WindowState>(state);
        t.tile_action = static_cast<uint32_t>(std::strtoul(f[2].c_str(), nullptr, 10));
        if (!parse_rect(f[3], t.floating)) continue;
        t.minimized = f[4] == "1";
        t.workspace = std::atoi(f[5].c_str());
        t.output = unescape(f[6]);
        parse_rect(f[7], t.output_rect);
        s.toplevels[unescape(f[0])] = t;
    }
    return true;
}

std::string serialize(Session const& s) {
    std::string out = "umoja-session " + std::to_string(FORMAT_VERSION) + "\n";
    for (auto const& [name, t] : s.toplevels) {
        out += escape(name) + "\t" + std::to_string(static_cast<int>(t.state)) + "\t" +
               std::to_string(t.tile_action) + "\t" + rect_str(t.floating) + "\t" + (t.minimized ? "1" : "0") +
               "\t" + std::to_string(t.workspace) + "\t" + escape(t.output) + "\t" + rect_str(t.output_rect) + "\n";
    }
    return out;
}

void write_file(std::string const& id, std::string const& contents) {
    std::error_code ec;
    auto const dir = session_dir();
    std::filesystem::create_directories(dir, ec);
    auto const tmp = dir / (id + ".tmp");
    {
        std::ofstream out{tmp, std::ios::trunc};
        if (!out) return;
        out << contents;
        if (!out) return;
    }
    std::filesystem::rename(tmp, dir / id, ec);
}

void remove_file(std::string const& id) {
    std::error_code ec;
    std::filesystem::remove(session_dir() / id, ec);
}

void prune_old_sessions() {
    std::error_code ec;
    auto const now = std::filesystem::file_time_type::clock::now();
    for (auto const& entry : std::filesystem::directory_iterator{session_dir(), ec}) {
        auto const age = now - entry.last_write_time(ec);
        if (!ec && age > SESSION_MAX_AGE) std::filesystem::remove(entry.path(), ec);
    }
}

// Debounced: the window manager reports every move and resize.
void schedule_save() {
    if (save_scheduled.exchange(true)) return;
    std::thread([] {
        std::this_thread::sleep_for(std::chrono::milliseconds(500));
        save_scheduled = false;
        std::function<void()> snap;
        {
            std::lock_guard lock{mutex};
            snap = snapshotter;
        }
        try {
            if (snap) snap();
        } catch (...) {
        }
        std::vector<std::pair<std::string, std::string>> out;
        {
            std::lock_guard lock{mutex};
            for (auto& [id, s] : sessions)
                if (s->dirty) {
                    s->dirty = false;
                    out.emplace_back(id, serialize(*s));
                }
        }
        for (auto const& [id, contents] : out) write_file(id, contents);
    }).detach();
}

// ---- protocol --------------------------------------------------------------

void unbind(Binding* b) {
    if (!b) return;
    bindings_by_token.erase(b->token);
    if (b->session) b->session->bindings.erase(b);
    if (b->toplevel) wl_list_remove(&b->toplevel_destroyed.link);
    b->toplevel = nullptr;
    b->session = nullptr;
    b->window = {};
}

void toplevel_session_destroyed(wl_resource* resource) {
    auto* b = static_cast<Binding*>(wl_resource_get_user_data(resource));
    std::lock_guard lock{mutex};
    unbind(b);
    delete b;
}

void on_toplevel_destroyed(wl_listener* listener, void*) {
    Binding* b = wl_container_of(listener, b, toplevel_destroyed);
    std::lock_guard lock{mutex};
    // Keep the xdg_toplevel_session_v1 object (the client destroys it); it
    // is inert now.
    wl_list_remove(&b->toplevel_destroyed.link);
    b->toplevel = nullptr;
    bindings_by_token.erase(b->token);
    if (b->session) b->session->bindings.erase(b);
    b->session = nullptr;
    b->window = {};
}

bool name_bound(Session const& s, std::string const& name) {
    for (auto const* b : s.bindings)
        if (b->name == name) return true;
    return false;
}

void toplevel_session_destroy(wl_client*, wl_resource* resource) { wl_resource_destroy(resource); }

void toplevel_session_rename(wl_client*, wl_resource* resource, char const* name) {
    auto* b = static_cast<Binding*>(wl_resource_get_user_data(resource));
    std::lock_guard lock{mutex};
    if (!b->session || !name) return;
    std::string const to{name};
    if (to == b->name) return;
    if (name_bound(*b->session, to) || b->session->toplevels.count(to)) {
        wl_resource_post_error(b->session->resource ? b->session->resource : resource,
                               XDG_SESSION_V1_ERROR_NAME_IN_USE, "toplevel name '%s' is already in use", name);
        return;
    }
    auto it = b->session->toplevels.find(b->name);
    if (it != b->session->toplevels.end()) {
        b->session->toplevels[to] = it->second;
        b->session->toplevels.erase(it);
        b->session->dirty = true;
    }
    b->name = to;
    schedule_save();
}

struct xdg_toplevel_session_v1_interface const toplevel_session_impl = {
    toplevel_session_destroy,
    toplevel_session_rename,
};

void add(wl_client* client, wl_resource* session_resource, uint32_t id, wl_resource* toplevel, char const* name,
         bool restore) {
    auto* s = static_cast<Session*>(wl_resource_get_user_data(session_resource));
    auto* res = wl_resource_create(client, &xdg_toplevel_session_v1_interface,
                                   wl_resource_get_version(session_resource), id);
    if (!res) {
        wl_client_post_no_memory(client);
        return;
    }
    auto* b = new Binding;
    wl_resource_set_implementation(res, &toplevel_session_impl, b, toplevel_session_destroyed);
    b->resource = res;
    b->client = client;
    b->created = std::chrono::steady_clock::now();
    if (!name) name = "";

    // The window may exist already (add_toplevel on a mapped toplevel).
    // Only Umoja's Mir resolves an xdg_toplevel to its window; elsewhere
    // this is null and the binding waits for the client's next window.
    miral::Window const existing = miral::window_for(toplevel);

    std::lock_guard lock{mutex};
    if (!s) return; // inert session (destroyed or replaced): the toplevel object is inert too
    if (name_bound(*s, name)) {
        wl_resource_post_error(session_resource, XDG_SESSION_V1_ERROR_NAME_IN_USE,
                               "toplevel name '%s' is already in use", name);
        return;
    }
    if (restore && existing) {
        wl_resource_post_error(session_resource, XDG_SESSION_V1_ERROR_ALREADY_MAPPED,
                               "toplevel was already mapped when restored");
        return;
    }
    b->session = s;
    b->name = name;
    b->toplevel = toplevel;
    b->token = next_token++;
    b->toplevel_destroyed.notify = on_toplevel_destroyed;
    wl_resource_add_destroy_listener(toplevel, &b->toplevel_destroyed);
    s->bindings.insert(b);
    bindings_by_token[b->token] = b;

    if (existing) {
        b->window = existing;
        b->claimed = true;
    } else if (restore) {
        auto const it = s->toplevels.find(b->name);
        if (it != s->toplevels.end()) {
            b->restore = it->second;
            // "emitted prior to the first xdg_toplevel.configure": the
            // configure only comes once the client commits.
            xdg_toplevel_session_v1_send_restored(res);
        }
    }
    schedule_save();
}

void session_add_toplevel(wl_client* client, wl_resource* resource, uint32_t id, wl_resource* toplevel,
                          char const* name) {
    add(client, resource, id, toplevel, name, false);
}

void session_restore_toplevel(wl_client* client, wl_resource* resource, uint32_t id, wl_resource* toplevel,
                              char const* name) {
    add(client, resource, id, toplevel, name, true);
}

void session_remove_toplevel(wl_client*, wl_resource* resource, char const* name) {
    auto* s = static_cast<Session*>(wl_resource_get_user_data(resource));
    std::lock_guard lock{mutex};
    if (!s || !name) return;
    for (auto* b : std::set<Binding*>{s->bindings})
        if (b->name == name) unbind(b);
    if (s->toplevels.erase(name)) s->dirty = true;
    schedule_save();
}

// The xdg_session_v1 object going away makes its toplevels inert.
void make_inert(Session& s) {
    for (auto* b : std::set<Binding*>{s.bindings}) unbind(b);
    if (s.resource) wl_resource_set_user_data(s.resource, nullptr);
    s.resource = nullptr;
    s.client = nullptr;
}

void session_resource_destroyed(wl_resource* resource) {
    auto* s = static_cast<Session*>(wl_resource_get_user_data(resource));
    std::lock_guard lock{mutex};
    if (s && s->resource == resource) make_inert(*s);
}

void session_destroy(wl_client*, wl_resource* resource) { wl_resource_destroy(resource); }

void session_remove(wl_client*, wl_resource* resource) {
    auto* s = static_cast<Session*>(wl_resource_get_user_data(resource));
    {
        std::lock_guard lock{mutex};
        if (s) {
            std::string const id = s->id;
            make_inert(*s);
            remove_file(id);
            sessions.erase(id);
        }
    }
    wl_resource_destroy(resource);
}

struct xdg_session_v1_interface const session_impl = {
    session_destroy,
    session_remove,
    session_add_toplevel,
    session_restore_toplevel,
    session_remove_toplevel,
};

void manager_destroy(wl_client*, wl_resource* resource) { wl_resource_destroy(resource); }

void manager_get_session(wl_client* client, wl_resource* manager, uint32_t id, uint32_t /*reason*/,
                         char const* session_id) {
    auto* res = wl_resource_create(client, &xdg_session_v1_interface, wl_resource_get_version(manager), id);
    if (!res) {
        wl_client_post_no_memory(client);
        return;
    }
    wl_resource_set_implementation(res, &session_impl, nullptr, session_resource_destroyed);

    std::lock_guard lock{mutex};
    Session* s = nullptr;
    bool restored = false;
    if (session_id && valid_id(session_id)) {
        auto it = sessions.find(session_id);
        if (it != sessions.end()) {
            s = it->second.get();
            restored = true;
        } else {
            auto loaded = std::make_unique<Session>();
            loaded->id = session_id;
            if (load(*loaded)) {
                s = loaded.get();
                sessions[session_id] = std::move(loaded);
                restored = true;
            }
        }
    }
    if (s && s->resource) {
        if (s->client == client) {
            wl_resource_post_error(manager, XDG_SESSION_MANAGER_V1_ERROR_IN_USE, "session %s is already in use",
                                   s->id.c_str());
            return;
        }
        // Another client takes the session over.
        xdg_session_v1_send_replaced(s->resource);
        make_inert(*s);
    }
    if (!s) {
        auto created = std::make_unique<Session>();
        created->id = new_id();
        s = created.get();
        sessions[s->id] = std::move(created);
    }
    s->resource = res;
    s->client = client;
    wl_resource_set_user_data(res, s);
    if (restored) {
        xdg_session_v1_send_restored(res);
        // Touch the file: it's in use, so not one to prune.
        std::error_code ec;
        std::filesystem::last_write_time(session_dir() / s->id, std::filesystem::file_time_type::clock::now(), ec);
    } else {
        xdg_session_v1_send_created(res, s->id.c_str());
        s->dirty = true;
        schedule_save();
    }
}

struct xdg_session_manager_v1_interface const manager_impl = {
    manager_destroy,
    manager_get_session,
};

void bind_manager(wl_client* client, void*, uint32_t version, uint32_t id) {
    auto* res = wl_resource_create(client, &xdg_session_manager_v1_interface, static_cast<int>(version), id);
    if (!res) {
        wl_client_post_no_memory(client);
        return;
    }
    wl_resource_set_implementation(res, &manager_impl, nullptr, nullptr);
}

} // namespace

auto extension() -> miral::WaylandExtensions::Builder {
    return miral::WaylandExtensions::Builder{
        xdg_session_manager_v1_interface.name,
        [](miral::WaylandExtensions::Context const* context) -> std::shared_ptr<void> {
            prune_old_sessions();
            auto* global = wl_global_create(context->display(), &xdg_session_manager_v1_interface, 1, nullptr,
                                            bind_manager);
            return std::shared_ptr<void>{global, [](void* g) {
                                             if (g) wl_global_destroy(static_cast<wl_global*>(g));
                                         }};
        }};
}

auto take_pending(miral::Application const& app) -> std::optional<Pending> {
    if (!app) return std::nullopt;
    std::lock_guard lock{mutex};
    auto const now = std::chrono::steady_clock::now();
    Binding* best = nullptr;
    for (auto const& [token, b] : bindings_by_token) {
        if (b->claimed || !b->toplevel || now - b->created > PENDING_MAX_AGE) continue;
        if (miral::application_for(b->toplevel) != app) continue;
        if (!best || b->token < best->token) best = b;
    }
    if (!best) return std::nullopt;
    Pending p;
    p.token = best->token;
    p.restore = best->restore;
    best->restore.reset();
    best->claimed = true;
    return p;
}

void bind(uint64_t token, miral::Window const& window) {
    std::lock_guard lock{mutex};
    auto it = bindings_by_token.find(token);
    if (it != bindings_by_token.end()) it->second->window = window;
}

auto bound_windows() -> std::vector<miral::Window> {
    std::lock_guard lock{mutex};
    std::vector<miral::Window> out;
    for (auto const& [token, b] : bindings_by_token)
        if (b->window && b->session) out.push_back(b->window);
    return out;
}

void update(miral::Window const& window, SavedToplevel const& state) {
    std::lock_guard lock{mutex};
    for (auto const& [token, b] : bindings_by_token) {
        if (!(b->window == window) || !b->session) continue;
        auto& saved = b->session->toplevels[b->name];
        auto next = state;
        // Keep the last known floating geometry if this one has none.
        if (next.floating.size.width.as_int() <= 0) next.floating = saved.floating;
        if (saved.state != next.state || saved.tile_action != next.tile_action || saved.floating != next.floating ||
            saved.minimized != next.minimized || saved.workspace != next.workspace || saved.output != next.output ||
            saved.output_rect != next.output_rect) {
            saved = next;
            b->session->dirty = true;
        }
    }
}

void window_closed(miral::Window const& window) {
    std::lock_guard lock{mutex};
    for (auto const& [token, b] : bindings_by_token)
        if (b->window == window) b->window = {};
}

void set_snapshotter(std::function<void()> snapshot) {
    std::lock_guard lock{mutex};
    snapshotter = std::move(snapshot);
}

void changed() {
    {
        std::lock_guard lock{mutex};
        if (bindings_by_token.empty()) return;
    }
    schedule_save();
}

} // namespace umoja_sm

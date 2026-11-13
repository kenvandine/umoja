//! umoja-portal-helper: the Hotspot Login window.
//!
//! A port of gnome-shell's portal helper (js/portalHelper/main.js). When
//! NetworkManager finds a captive portal (hotel, airport and café Wi-Fi),
//! umoja-panel shows "Sign in to Network"; clicking it calls `Authenticate`
//! on `org.umoja.PortalHelper`, which D-Bus activates this program. It opens
//! the login page in a small WebKit browser with its own throwaway session
//! (no cookies or history from the user's browser, no proxy) and tells the
//! panel when to re-check connectivity. The panel closes the window once
//! NetworkManager reports full connectivity.
//!
//! Run it by hand (`umoja-portal-helper`) to open a test window on
//! nmcheck.gnome.org.

use gtk::{gio, glib};
use gtk4 as gtk;
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;
use umoja_common::dbus::{
    PORTAL_CANCELLED, PORTAL_COMPLETED, PORTAL_HELPER_BUS_NAME, PORTAL_HELPER_INTERFACE, PORTAL_HELPER_OBJECT_PATH,
    PORTAL_HELPER_XML, PORTAL_RECHECK,
};
use umoja_common::i18n;
use webkit6::prelude::*;

const CONNECTIVITY_CHECK_HOST: &str = "nmcheck.gnome.org";
const CONNECTIVITY_CHECK_URI: &str = "http://nmcheck.gnome.org";
/// Portals with ten or more internal redirects exist: re-check at most this often.
const RECHECK_RATELIMIT: Duration = Duration::from_secs(5);

fn uri_flags() -> glib::UriFlags {
    glib::UriFlags::HAS_PASSWORD
        | glib::UriFlags::ENCODED_PATH
        | glib::UriFlags::ENCODED_QUERY
        | glib::UriFlags::ENCODED_FRAGMENT
        | glib::UriFlags::SCHEME_NORMALIZE
        | glib::UriFlags::PARSE_RELAXED
}

fn host_of(uri: &str) -> Option<String> {
    glib::Uri::parse(uri, uri_flags()).ok()?.host().map(|h| h.to_string())
}

#[derive(Clone, Copy)]
enum Security {
    NotYetDetermined,
    Secure,
    Insecure,
}

/// The padlock in the header bar, with a popover saying how safe the page is.
struct SecurityButton {
    button: gtk::MenuButton,
    icon: gtk::Image,
    title: gtk::Label,
    description: gtk::Label,
}

impl SecurityButton {
    fn new() -> Self {
        let popover = gtk::Popover::new();
        let button = gtk::MenuButton::builder().popover(&popover).visible(false).build();
        let vbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .spacing(6)
            .build();
        popover.set_child(Some(&vbox));
        let hbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .halign(gtk::Align::Center)
            .spacing(6)
            .build();
        vbox.append(&hbox);
        let icon = gtk::Image::new();
        hbox.append(&icon);
        let title = gtk::Label::new(None);
        title.add_css_class("title");
        hbox.append(&title);
        let description = gtk::Label::builder().wrap(true).max_width_chars(32).build();
        vbox.append(&description);
        Self {
            button,
            icon,
            title,
            description,
        }
    }

    fn set_title(&self, host: &str) {
        self.title.set_text(host);
    }

    fn set_level(&self, level: Security) {
        let (icon, text) = match level {
            Security::NotYetDetermined => {
                self.button.set_visible(false);
                return;
            }
            Security::Secure => (
                "channel-secure-symbolic",
                i18n::shell("Your connection seems to be secure"),
            ),
            Security::Insecure => (
                "channel-insecure-symbolic",
                i18n::shell(
                    "Your connection to this hotspot login is not secure. Passwords or other information you enter on this page can be viewed by people nearby.",
                ),
            ),
        };
        self.icon.set_icon_name(Some(icon));
        self.button.set_icon_name(icon);
        self.description.set_label(&text);
        self.button.set_visible(true);
    }
}

type StatusFn = Box<dyn Fn(u32)>;

struct PortalWindow {
    window: gtk::ApplicationWindow,
    view: webkit6::WebView,
    session: webkit6::NetworkSession,
    security: SecurityButton,
    original_url: String,
    /// We were given no URL and went to nmcheck.gnome.org ourselves, so
    /// arriving back there means the portal let us through.
    original_was_gnome: bool,
    /// Host of the page being navigated to (follows chained redirects).
    host: RefCell<Option<String>>,
    ever_seen_redirect: Cell<bool>,
    last_recheck: Cell<Option<std::time::Instant>>,
    recheck_timeout: RefCell<Option<glib::SourceId>>,
    status: StatusFn,
}

impl PortalWindow {
    fn new(app: &adw::Application, url: &str, token: &str, status: StatusFn) -> Rc<Self> {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title(i18n::shell("Hotspot Login"))
            .default_width(600)
            .default_height(450)
            .build();
        for (name, step) in [
            ("zoom-in", Zoom::In),
            ("zoom-out", Zoom::Out),
            ("zoom-normal", Zoom::Normal),
        ] {
            let action = gio::SimpleAction::new(name, None);
            let w = window.downgrade();
            action.connect_activate(move |_, _| {
                if let Some(view) = w.upgrade().and_then(|w| w.child()).and_downcast::<webkit6::WebView>() {
                    view.set_zoom_level(zoom_level(view.zoom_level(), step));
                }
            });
            window.add_action(&action);
        }

        let headerbar = gtk::HeaderBar::new();
        let security = SecurityButton::new();
        headerbar.pack_start(&security.button);
        window.set_titlebar(Some(&headerbar));

        let (original_url, original_was_gnome) = if url.is_empty() {
            (CONNECTIVITY_CHECK_URI.to_string(), true)
        } else {
            (url.to_string(), false)
        };

        // A throwaway session: nothing from (or for) the user's own
        // browser, and no proxy, which would hide the portal.
        let session = webkit6::NetworkSession::new_ephemeral();
        session.set_proxy_settings(webkit6::NetworkProxyMode::NoProxy, None);
        let context = webkit6::WebContext::new();
        context.set_cache_model(webkit6::CacheModel::DocumentViewer);
        let view = webkit6::WebView::builder()
            .network_session(&session)
            .web_context(&context)
            .build();

        let this = Rc::new(Self {
            window,
            view,
            session,
            security,
            host: RefCell::new(host_of(&original_url)),
            original_url,
            original_was_gnome,
            ever_seen_redirect: Cell::new(false),
            last_recheck: Cell::new(None),
            recheck_timeout: RefCell::new(None),
            status,
        });

        let weak = Rc::downgrade(&this);
        this.view.connect_decide_policy(move |_, decision, kind| {
            weak.upgrade().is_some_and(|w| w.decide_policy(decision, kind))
        });
        let weak = Rc::downgrade(&this);
        this.view.connect_load_changed(move |view, event| {
            let Some(w) = weak.upgrade() else { return };
            match event {
                webkit6::LoadEvent::Started => w.security.set_level(Security::NotYetDetermined),
                webkit6::LoadEvent::Committed => {
                    let secure = view.tls_info().is_some_and(|(_, errors)| errors.is_empty());
                    w.security
                        .set_level(if secure { Security::Secure } else { Security::Insecure });
                }
                _ => {}
            }
        });
        let weak = Rc::downgrade(&this);
        this.view.connect_insecure_content_detected(move |_, _| {
            if let Some(w) = weak.upgrade() {
                w.security.set_level(Security::Insecure);
            }
        });
        // Portals often use self-signed certificates: load anyway, marked
        // insecure, as gnome-shell does.
        let weak = Rc::downgrade(&this);
        this.view
            .connect_load_failed_with_tls_errors(move |view, failing, cert, _| {
                let Some(w) = weak.upgrade() else { return false };
                w.security.set_level(Security::Insecure);
                if let Some(host) = host_of(failing) {
                    w.session.allow_tls_certificate_for_host(cert, &host);
                }
                view.load_uri(failing);
                true
            });
        let weak = Rc::downgrade(&this);
        this.view.connect_uri_notify(move |_| {
            if let Some(w) = weak.upgrade() {
                w.sync_uri();
            }
        });
        this.view.load_uri(&this.original_url);
        this.sync_uri();

        let weak = Rc::downgrade(&this);
        this.window.connect_close_request(move |_| {
            if let Some(w) = weak.upgrade() {
                if let Some(id) = w.recheck_timeout.take() {
                    id.remove();
                    (w.status)(PORTAL_RECHECK);
                }
                (w.status)(PORTAL_CANCELLED);
            }
            glib::Propagation::Proceed
        });

        this.window.set_child(Some(&this.view));
        this.window.maximize();
        if !token.is_empty() {
            // An xdg-activation token from the panel, so the compositor
            // gives the new window focus.
            this.window.set_startup_id(token);
        }
        this.window.present();
        this
    }

    fn sync_uri(&self) {
        let host = self.view.uri().and_then(|u| {
            glib::Uri::split_network(&u, uri_flags())
                .ok()
                .and_then(|(_, host, _)| host)
                .map(|h| h.to_string())
        });
        self.security.set_title(host.as_deref().unwrap_or(""));
    }

    fn refresh(&self) {
        self.ever_seen_redirect.set(false);
        self.view.load_uri(&self.original_url);
    }

    fn decide_policy(self: &Rc<Self>, decision: &webkit6::PolicyDecision, kind: webkit6::PolicyDecisionType) -> bool {
        use webkit6::PolicyDecisionType as T;
        if !matches!(kind, T::NavigationAction | T::NewWindowAction) {
            return false;
        }
        let Some(mut action) = decision
            .downcast_ref::<webkit6::NavigationPolicyDecision>()
            .and_then(|d| d.navigation_action())
        else {
            return false;
        };
        let Some(request) = action.request() else {
            return false;
        };

        if kind == T::NewWindowAction {
            // Some portals open a window as their last login step and
            // don't let the user through if it's ignored: follow it here
            // instead, but only when the user clicked (no popups).
            if action.is_user_gesture() {
                self.view.load_request(&request);
            }
            decision.ignore();
            return true;
        }

        let Some(uri) = request.uri() else {
            return false;
        };
        let host = host_of(&uri);
        if self.original_was_gnome && host != *self.host.borrow() {
            if host.as_deref() == Some(CONNECTIVITY_CHECK_HOST) && self.ever_seen_redirect.get() {
                // Back at nmcheck.gnome.org after the portal: we're through.
                decision.ignore();
                (self.status)(PORTAL_COMPLETED);
                return true;
            } else if host.as_deref() != Some(CONNECTIVITY_CHECK_HOST) {
                self.ever_seen_redirect.set(true);
            }
        }

        // Maybe we're done, maybe not: ask the panel to have NetworkManager
        // check again, rate limited.
        if self.recheck_timeout.borrow().is_none() {
            let now = std::time::Instant::now();
            match self.last_recheck.get().map(|t| now.duration_since(t)) {
                Some(since) if since <= RECHECK_RATELIMIT => {
                    let weak = Rc::downgrade(self);
                    let id = glib::timeout_add_local_once(RECHECK_RATELIMIT - since, move || {
                        if let Some(w) = weak.upgrade() {
                            w.recheck_timeout.take();
                            w.last_recheck.set(Some(std::time::Instant::now()));
                            (w.status)(PORTAL_RECHECK);
                        }
                    });
                    self.recheck_timeout.replace(Some(id));
                }
                _ => {
                    self.last_recheck.set(Some(now));
                    (self.status)(PORTAL_RECHECK);
                }
            }
        }

        // Follow chained redirects, so we still think we're logging in
        // until the panel closes us.
        self.host.replace(host);
        decision.use_();
        true
    }
}

#[derive(Clone, Copy)]
enum Zoom {
    Normal,
    In,
    Out,
}

/// The next zoom level, in Epiphany's steps.
fn zoom_level(current: f64, step: Zoom) -> f64 {
    const STEPS: [f64; 14] = [
        0.30, 0.50, 0.67, 0.80, 0.90, 1.00, 1.10, 1.20, 1.33, 1.50, 1.70, 2.00, 2.40, 3.00,
    ];
    if let Zoom::Normal = step {
        return 1.0;
    }
    let i = STEPS
        .iter()
        .position(|&v| (v - current).abs() < 1e-6)
        .or_else(|| STEPS.iter().rposition(|&v| v < current));
    let Some(i) = i else { return 1.0 };
    match step {
        Zoom::Out if i > 0 => STEPS[i - 1],
        Zoom::In if i < STEPS.len() - 1 => STEPS[i + 1],
        _ => STEPS[i],
    }
}

struct Request {
    connection: String,
    url: String,
    token: String,
    window: Option<Rc<PortalWindow>>,
}

/// One login window at a time; later requests wait their turn.
struct Helper {
    app: adw::Application,
    queue: RefCell<Vec<Request>>,
    conn: RefCell<Option<gio::DBusConnection>>,
}

impl Helper {
    fn authenticate(self: &Rc<Self>, connection: &str, url: &str, token: &str) {
        {
            let mut queue = self.queue.borrow_mut();
            // NetworkManager asking again for the same connection: show the
            // window we have rather than a second one.
            if let Some(r) = queue.iter().find(|r| r.connection == connection) {
                if let Some(w) = &r.window {
                    if !token.is_empty() {
                        w.window.set_startup_id(token);
                    }
                    w.window.present();
                }
                return;
            }
            queue.push(Request {
                connection: connection.into(),
                url: url.into(),
                token: token.into(),
                window: None,
            });
        }
        self.process();
    }

    fn close(self: &Rc<Self>, connection: &str) {
        let removed = {
            let mut queue = self.queue.borrow_mut();
            queue
                .iter()
                .position(|r| r.connection == connection)
                .map(|i| queue.remove(i))
        };
        if let Some(w) = removed.and_then(|r| r.window) {
            w.window.destroy();
        }
        self.process();
    }

    fn refresh(&self, connection: &str) {
        if let Some(w) = self
            .queue
            .borrow()
            .iter()
            .find(|r| r.connection == connection)
            .and_then(|r| r.window.clone())
        {
            w.refresh();
        }
    }

    fn process(self: &Rc<Self>) {
        let (connection, url, token) = {
            let queue = self.queue.borrow();
            let Some(top) = queue.first() else { return };
            if top.window.is_some() {
                return;
            }
            (top.connection.clone(), top.url.clone(), top.token.clone())
        };
        let weak = Rc::downgrade(self);
        let path = connection.clone();
        let status: StatusFn = Box::new(move |result| {
            if let Some(h) = weak.upgrade() {
                h.emit_status(&path, result);
                // Closed by the user: drop it from the queue (the panel
                // keeps it in its own, so it won't ask again for now), and
                // move on to the next one.
                if result == PORTAL_CANCELLED {
                    let h = h.clone();
                    let path = path.clone();
                    glib::idle_add_local_once(move || {
                        let mut queue = h.queue.borrow_mut();
                        if let Some(i) = queue.iter().position(|r| r.connection == path) {
                            queue.remove(i);
                        }
                        drop(queue);
                        h.process();
                    });
                }
            }
        });
        let window = PortalWindow::new(&self.app, &url, &token, status);
        if let Some(top) = self.queue.borrow_mut().first_mut() {
            top.window = Some(window);
        }
    }

    fn emit_status(&self, connection: &str, result: u32) {
        let Some(conn) = self.conn.borrow().clone() else { return };
        let Ok(path) = glib::variant::ObjectPath::try_from(connection.to_string()) else {
            return;
        };
        if let Err(e) = conn.emit_signal(
            None,
            PORTAL_HELPER_OBJECT_PATH,
            PORTAL_HELPER_INTERFACE,
            "StatusChanged",
            Some(&(path, result).to_variant()),
        ) {
            tracing::warn!(error = %e, "could not emit StatusChanged");
        }
    }

    fn export(self: &Rc<Self>, conn: &gio::DBusConnection) -> Result<(), glib::Error> {
        let info = gio::DBusNodeInfo::for_xml(PORTAL_HELPER_XML)?;
        let iface = info
            .lookup_interface(PORTAL_HELPER_INTERFACE)
            .ok_or_else(|| glib::Error::new(gio::IOErrorEnum::Failed, "no interface"))?;
        let weak = Rc::downgrade(self);
        conn.register_object(PORTAL_HELPER_OBJECT_PATH, &iface)
            .method_call(move |_, _, _, _, method, params, invocation| {
                let Some(h) = weak.upgrade() else { return };
                let arg = |i: usize| params.try_child_value(i).and_then(|v| v.str().map(String::from));
                match (method, arg(0)) {
                    ("Authenticate", Some(path)) => {
                        h.authenticate(&path, &arg(1).unwrap_or_default(), &arg(2).unwrap_or_default())
                    }
                    ("Close", Some(path)) => h.close(&path),
                    ("Refresh", Some(path)) => h.refresh(&path),
                    _ => {
                        invocation.return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", method);
                        return;
                    }
                }
                invocation.return_value(None);
            })
            .build()?;
        self.conn.replace(Some(conn.clone()));
        Ok(())
    }
}

fn main() -> glib::ExitCode {
    umoja_common::init_logging("umoja-portal-helper");
    i18n::init();
    // D-Bus activation passes --gapplication-service.
    let app = adw::Application::builder()
        .application_id(PORTAL_HELPER_BUS_NAME)
        .build();
    app.set_inactivity_timeout(30_000);
    let helper = Rc::new(Helper {
        app: app.clone(),
        queue: RefCell::default(),
        conn: RefCell::default(),
    });

    app.connect_startup({
        let helper = helper.clone();
        move |app| {
            if let Some(conn) = app.dbus_connection()
                && let Err(e) = helper.export(&conn)
            {
                tracing::warn!(error = %e, "could not export {PORTAL_HELPER_INTERFACE}");
            }
            let quit = gio::SimpleAction::new("quit", None);
            let a = app.downgrade();
            quit.connect_activate(move |_, _| {
                if let Some(w) = a.upgrade().and_then(|a| a.active_window()) {
                    w.close();
                }
            });
            app.add_action(&quit);
            app.set_accels_for_action(
                "win.zoom-in",
                &["<Primary>plus", "<Primary>KP_Add", "<Primary>equal", "ZoomIn"],
            );
            app.set_accels_for_action("win.zoom-out", &["<Primary>minus", "<Primary>KP_Subtract", "ZoomOut"]);
            app.set_accels_for_action("win.zoom-normal", &["<Primary>0", "<Primary>KP_0"]);
            app.set_accels_for_action("app.quit", &["<Primary>q", "<Primary>w"]);
        }
    });
    // Started by hand (for testing): a dummy login on nmcheck.gnome.org.
    app.connect_activate({
        let helper = helper.clone();
        move |_| helper.authenticate("/org/umoja/dummy", "", "")
    });
    app.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_steps() {
        assert_eq!(zoom_level(1.0, Zoom::In), 1.10);
        assert_eq!(zoom_level(1.0, Zoom::Out), 0.90);
        assert_eq!(zoom_level(3.0, Zoom::In), 3.0);
        assert_eq!(zoom_level(0.30, Zoom::Out), 0.30);
        // Between steps: from the one below.
        assert_eq!(zoom_level(1.05, Zoom::In), 1.10);
        assert_eq!(zoom_level(0.1, Zoom::In), 1.0);
        assert_eq!(zoom_level(2.2, Zoom::Normal), 1.0);
    }

    #[test]
    fn hosts() {
        assert_eq!(
            host_of("http://nmcheck.gnome.org/check").as_deref(),
            Some("nmcheck.gnome.org")
        );
        assert_eq!(
            host_of("http://connectivity-check.ubuntu.com./").as_deref(),
            Some("connectivity-check.ubuntu.com.")
        );
    }
}

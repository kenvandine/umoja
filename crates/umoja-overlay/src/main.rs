//! umoja-overlay: app grid, window-switching entry point and AI search.
//!
//! Exposes `org.umoja.Overlay` on the session bus so the compositor (Super
//! key), panel (Activities) and dock (Show Apps) can toggle it.

mod assistant;
mod canvas;
mod chrome;
mod clipboard;
mod clipwatch;
mod cloud;
mod context;
mod files;
mod lemonade;
mod model;
mod model_picker;
mod myna;
mod screencast;
mod screenshot;
mod shortcuts;
mod spread;
mod texpane;
mod view;
mod window_menu;

use gio::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;
use std::rc::Rc;
use umoja_common::{dbus, i18n};

const APP_ID: &str = "org.umoja.OverlayApp";

const IFACE_XML: &str = r#"
<node>
  <interface name="org.umoja.Overlay">
    <method name="Toggle"/>
    <method name="ToggleOverview"/>
    <method name="Show"/>
    <method name="Hide"/>
    <method name="ToggleDictation"/>
    <method name="AskAboutScreen"/>
    <method name="LaunchAgent"/>
    <method name="Ask">
      <arg name="question" type="s" direction="in"/>
    </method>
    <!-- flags: 1 = the local model, 2 = type the question out first -->
    <method name="AskWith">
      <arg name="question" type="s" direction="in"/>
      <arg name="flags" type="u" direction="in"/>
    </method>
    <!-- An answer finished: its text and steps as "state:label"
         (state is done, failed, declined or running). -->
    <signal name="AnswerFinished">
      <arg name="answer" type="s"/>
      <arg name="steps" type="as"/>
    </signal>
    <!-- The assistant is asking "Allow the assistant to do this?". -->
    <signal name="ConfirmShown">
      <arg name="label" type="s"/>
    </signal>
    <method name="ShowClipboard"/>
    <method name="ShowScreenshotUI"/>
    <method name="ShowScreenRecordingUI"/>
    <method name="ShowCopyTextUI"/>
    <method name="HideScreenshotUI"/>
    <method name="ToggleShortcuts"/>
    <method name="StopScreencast"/>
    <method name="IsScreencasting">
      <arg name="active" type="b" direction="out"/>
    </method>
    <signal name="ScreencastChanged">
      <arg name="active" type="b"/>
    </signal>
    <method name="ScreenshotScreen"/>
    <method name="ScreenshotArea">
      <arg name="x" type="i" direction="in"/>
      <arg name="y" type="i" direction="in"/>
      <arg name="width" type="i" direction="in"/>
      <arg name="height" type="i" direction="in"/>
    </method>
    <method name="TilePreview">
      <arg name="show" type="b" direction="in"/>
      <arg name="x" type="i" direction="in"/>
      <arg name="y" type="i" direction="in"/>
      <arg name="width" type="i" direction="in"/>
      <arg name="height" type="i" direction="in"/>
      <arg name="window_x" type="i" direction="in"/>
      <arg name="window_y" type="i" direction="in"/>
      <arg name="window_width" type="i" direction="in"/>
      <arg name="window_height" type="i" direction="in"/>
    </method>
    <method name="LayoutPicker">
      <arg name="visibility" type="u" direction="in"/>
      <arg name="tile" type="u" direction="in"/>
      <arg name="x" type="i" direction="in"/>
      <arg name="work_area_y" type="i" direction="in"/>
    </method>
    <method name="ShowSwitcher">
      <arg name="mode" type="u" direction="in"/>
      <arg name="items" type="a(ssas)" direction="in"/>
      <arg name="selected" type="u" direction="in"/>
      <arg name="window" type="i" direction="in"/>
    </method>
    <method name="SwitcherSelect">
      <arg name="selected" type="u" direction="in"/>
      <arg name="window" type="i" direction="in"/>
    </method>
    <method name="HideSwitcher"/>
    <method name="WorkspaceSwitched">
      <arg name="active" type="u" direction="in"/>
      <arg name="count" type="u" direction="in"/>
    </method>
    <method name="ShiftOverview">
      <arg name="up" type="b" direction="in"/>
    </method>
    <method name="ShowWindowMenu">
      <arg name="id" type="t" direction="in"/>
      <arg name="x" type="i" direction="in"/>
      <arg name="y" type="i" direction="in"/>
      <arg name="flags" type="u" direction="in"/>
      <arg name="workspace" type="u" direction="in"/>
      <arg name="n_workspaces" type="u" direction="in"/>
      <arg name="neighbours" type="u" direction="in"/>
    </method>
    <property name="Visible" type="b" access="read"/>
  </interface>
</node>"#;

fn main() -> glib::ExitCode {
    umoja_common::init_logging("umoja-overlay");
    i18n::init();
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_startup(|_| load_css());
    app.connect_activate(|app| {
        let overlay = view::Overlay::new(app);
        let shots = screenshot::ScreenshotUi::new(app);
        overlay.set_screenshot_ui(&shots);
        // Fetch the assistant's model in the background at login.
        let weak = std::rc::Rc::downgrade(&overlay);
        std::mem::forget(model::ModelWatcher::start(move |status| {
            if let Some(o) = weak.upgrade() {
                o.set_model_status(status);
            }
        }));
        let spread = spread::Spread::new(app, &overlay);
        let chrome = chrome::Chrome::new(app);
        let menu = window_menu::WindowMenu::new(app);
        let guide = shortcuts::ShortcutsGuide::new(app);
        export(overlay, shots, spread, chrome, menu, guide);
        // Keep the process alive while the overlay window is hidden.
        std::mem::forget(app.hold());
    });
    app.run()
}

fn export(
    overlay: Rc<view::Overlay>,
    shots: Rc<screenshot::ScreenshotUi>,
    spread: Rc<spread::Spread>,
    chrome: Rc<chrome::Chrome>,
    menu: Rc<window_menu::WindowMenu>,
    guide: Rc<shortcuts::ShortcutsGuide>,
) {
    let on_bus = move |conn: gio::DBusConnection, _: &str| {
        let info = gio::DBusNodeInfo::for_xml(IFACE_XML).expect("valid XML");
        let iface = info
            .lookup_interface(dbus::OVERLAY_INTERFACE)
            .expect("interface");
        let methods = overlay.clone();
        let props = overlay.clone();
        let shots = shots.clone();
        let spread = spread.clone();
        let chrome = chrome.clone();
        let menu = menu.clone();
        let guide = guide.clone();
        // umoja-panel's recording indicator follows this.
        let events_conn = conn.clone();
        overlay.set_event_sink(move |signal, params| {
            if let Err(e) = events_conn.emit_signal(
                None,
                dbus::OVERLAY_OBJECT_PATH,
                dbus::OVERLAY_INTERFACE,
                signal,
                Some(&params),
            ) {
                tracing::warn!(error = %e, signal, "could not emit");
            }
        });
        let signal_conn = conn.clone();
        shots.recorder().connect_changed(move |active| {
            if let Err(e) = signal_conn.emit_signal(
                None,
                dbus::OVERLAY_OBJECT_PATH,
                dbus::OVERLAY_INTERFACE,
                "ScreencastChanged",
                Some(&(active,).to_variant()),
            ) {
                tracing::warn!(error = %e, "could not emit ScreencastChanged");
            }
        });
        let res = conn
            .register_object(dbus::OVERLAY_OBJECT_PATH, &iface)
            .method_call(move |_, _, _, _, method, params, invocation| {
                match method {
                    "ShowScreenshotUI" => {
                        methods.hide();
                        shots.open();
                    }
                    "ShowScreenRecordingUI" => {
                        methods.hide();
                        shots.toggle_screen_recording();
                    }
                    "ShowCopyTextUI" => {
                        methods.hide();
                        shots.open_copy_text();
                    }
                    // Close the screenshot UI without taking a shot, as
                    // Escape does (tools/demo drives it hands-free).
                    "HideScreenshotUI" => shots.dismiss(),
                    "ToggleShortcuts" => {
                        if !guide.is_visible() {
                            methods.hide();
                            spread.hide();
                            chrome.hide_switcher();
                            menu.hide();
                        }
                        guide.toggle();
                    }
                    "StopScreencast" => shots.recorder().stop(),
                    "IsScreencasting" => {
                        invocation
                            .return_value(Some(&(shots.recorder().is_recording(),).to_variant()));
                        return;
                    }
                    "ScreenshotScreen" => shots.capture_screen(),
                    "ScreenshotArea" => match params.get::<(i32, i32, i32, i32)>() {
                        Some((x, y, w, h)) => shots.capture_area(x, y, w, h),
                        None => {
                            invocation.return_dbus_error(
                                "org.freedesktop.DBus.Error.InvalidArgs",
                                "expected (iiii)",
                            );
                            return;
                        }
                    },
                    "Toggle" => {
                        spread.hide();
                        methods.toggle()
                    }
                    "ToggleOverview" => {
                        // Drag chrome never stays above the overview.
                        chrome.tile_preview(false, (0, 0, 0, 0), (0, 0, 0, 0));
                        chrome.layout_picker(0, 0, 0, 0);
                        chrome.hide_switcher();
                        menu.hide();
                        // Super from the app grid closes it, as in GNOME.
                        if methods.is_visible() {
                            methods.hide();
                        } else {
                            spread.toggle();
                        }
                    }
                    "TilePreview" => {
                        if let Some((show, x, y, w, h, wx, wy, ww, wh)) =
                            params.get::<(bool, i32, i32, i32, i32, i32, i32, i32, i32)>()
                        {
                            chrome.tile_preview(show, (x, y, w, h), (wx, wy, ww, wh));
                        }
                    }
                    "LayoutPicker" => {
                        if let Some((vis, tile, x, y)) = params.get::<(u32, u32, i32, i32)>() {
                            chrome.layout_picker(vis, tile, x, y);
                        }
                    }
                    "ShowSwitcher" => {
                        if let Some((mode, items, sel, sub)) =
                            params.get::<(u32, chrome::SwitcherItems, u32, i32)>()
                        {
                            chrome.show_switcher(mode, items, sel, sub);
                        }
                    }
                    "SwitcherSelect" => {
                        if let Some((sel, sub)) = params.get::<(u32, i32)>() {
                            chrome.switcher_select(sel, sub);
                        }
                    }
                    "HideSwitcher" => chrome.hide_switcher(),
                    "ShowWindowMenu" => {
                        if let Some((id, x, y, flags, workspace, n_workspaces, neighbours)) =
                            params.get::<(u64, i32, i32, u32, u32, u32, u32)>()
                        {
                            menu.show(window_menu::Target {
                                id,
                                x,
                                y,
                                flags,
                                workspace,
                                n_workspaces,
                                neighbours,
                            });
                        }
                    }
                    "WorkspaceSwitched" => {
                        // The overview shows the switch itself (it follows
                        // WorkspacesChanged), so no popup on top of it.
                        if let Some((active, count)) = params.get::<(u32, u32)>() {
                            if !spread.is_visible() {
                                chrome.workspace_switched(active, count);
                            }
                        }
                    }
                    // gnome-shell's shift-overview-up/down (Super+Alt+Up/Down):
                    // desktop, overview and app grid are steps on one ladder.
                    "ShiftOverview" => {
                        let up = params.get::<(bool,)>().map(|(u,)| u).unwrap_or(true);
                        if up {
                            if spread.is_visible() {
                                spread.hide();
                                methods.show();
                            } else if !methods.is_visible() {
                                chrome.hide_switcher();
                                spread.show();
                            }
                        } else if methods.is_visible() {
                            methods.hide();
                            spread.show();
                        } else if spread.is_visible() {
                            spread.hide();
                        }
                    }
                    "Show" => {
                        spread.hide();
                        methods.show()
                    }
                    "Hide" => methods.hide(),
                    "ToggleDictation" => methods.toggle_dictation(),
                    "AskAboutScreen" => {
                        spread.hide();
                        methods.ask_about_screen()
                    }
                    "LaunchAgent" => {
                        spread.hide();
                        methods.launch_agent("")
                    }
                    "Ask" => {
                        let Some((question,)) = params.get::<(String,)>() else {
                            invocation.return_dbus_error(
                                "org.freedesktop.DBus.Error.InvalidArgs",
                                "expected (s)",
                            );
                            return;
                        };
                        spread.hide();
                        methods.ask_now(&question)
                    }
                    "AskWith" => {
                        let Some((question, flags)) = params.get::<(String, u32)>() else {
                            invocation.return_dbus_error(
                                "org.freedesktop.DBus.Error.InvalidArgs",
                                "expected (su)",
                            );
                            return;
                        };
                        spread.hide();
                        methods.ask_with(&question, flags & 1 != 0, flags & 2 != 0)
                    }
                    "ShowClipboard" => {
                        spread.hide();
                        methods.show_clipboard()
                    }
                    _ => {
                        invocation
                            .return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", method);
                        return;
                    }
                }
                invocation.return_value(None);
            })
            .property(move |_, _, _, _, name| match name {
                "Visible" => props.is_visible().to_variant(),
                _ => false.to_variant(),
            })
            .build();
        if let Err(e) = res {
            tracing::error!(error = %e, "could not export org.umoja.Overlay");
        }
    };
    let _ = gio::bus_own_name(
        gio::BusType::Session,
        dbus::OVERLAY_BUS_NAME,
        gio::BusNameOwnerFlags::REPLACE,
        on_bus,
        |_, _| tracing::info!("overlay ready"),
        |_, _| tracing::warn!("lost org.umoja.Overlay"),
    );
}

fn load_css() {
    // Always dark, like the panel and dock: the overlay's controls sit on
    // the shell's dark scrim whatever Settings > Appearance says.
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("overlay.css"));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

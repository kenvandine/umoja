//! umoja-overlay: app grid, overview, Ask Umoja and the shell's other
//! full-screen chrome.
//!
//! Exposes `org.umoja.Overlay` on the session bus so the compositor (Super,
//! the Copilot key), panel (Activities) and dock (Show Apps) can toggle it.

mod appgrid;
mod ask;
mod assistant;
mod canvas;
mod captions;
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
mod shotname;
mod speech;
mod spread;
mod texpane;
mod typer;
mod view;
mod voice_picker;
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
    <!-- The app grid (Show Apps). -->
    <method name="Toggle"/>
    <method name="ToggleOverview"/>
    <!-- Ask Umoja (the Copilot key, or a tap of Alt). -->
    <method name="ToggleAsk"/>
    <!-- Open Ask Umoja with text typed but not asked; typed = a few
         letters at a time, as a person would (for tools/demo). -->
    <method name="AskType">
      <arg name="text" type="s" direction="in"/>
      <arg name="typed" type="b" direction="in"/>
    </method>
    <method name="Show"/>
    <method name="Hide"/>
    <method name="ToggleDictation"/>
    <method name="ToggleCaptions"/>
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
    <!-- Press Allow (true) or Cancel on the confirmation that is showing,
         for tools/demo. Only while $XDG_RUNTIME_DIR/umoja-demo exists;
         answered = false when there was nothing to answer. -->
    <method name="AnswerConfirm">
      <arg name="allow" type="b" direction="in"/>
      <arg name="answered" type="b" direction="out"/>
    </method>
    <!-- Feature-test hooks (tools/test/umoja-feature-test), only while
         $XDG_RUNTIME_DIR/umoja-demo exists. AskRows lists Ask Umoja's rows
         as (kind, title, subtitle); AskActivate runs one (headings not
         counted), like Enter; TestAction presses Replace the Selection
         ("replace") or the newest Undo ("undo"), attaches the selection
         ("attach-selection"), or reads "attachments", "answer" and
         "captions-text", or presses captions' Summarize
         ("captions-summarize"). -->
    <method name="AskRows">
      <arg name="rows" type="a(sss)" direction="out"/>
    </method>
    <method name="AskActivate">
      <arg name="index" type="u" direction="in"/>
      <arg name="done" type="b" direction="out"/>
    </method>
    <method name="TestAction">
      <arg name="action" type="s" direction="in"/>
      <arg name="ok" type="b" direction="out"/>
      <arg name="text" type="s" direction="out"/>
    </method>
    <method name="ShowClipboard"/>
    <!-- Read text aloud in Umoja's voice (Piper); StopSpeaking ends it. -->
    <method name="Speak">
      <arg type="s" name="text" direction="in"/>
    </method>
    <method name="StopSpeaking"/>
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
      <arg name="window_ids" type="aat" direction="in"/>
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
        let grid = appgrid::AppGrid::new(app);
        let shots = screenshot::ScreenshotUi::new(app);
        overlay.set_screenshot_ui(&shots);
        // Fetch the assistant's model in the background at login.
        let weak = std::rc::Rc::downgrade(&overlay);
        std::mem::forget(model::ModelWatcher::start(move |status| {
            if let Some(o) = weak.upgrade() {
                o.set_model_status(status);
            }
        }));
        let spread = spread::Spread::new(app, &grid);
        let chrome = chrome::Chrome::new(app);
        let menu = window_menu::WindowMenu::new(app);
        let guide = shortcuts::ShortcutsGuide::new(app);
        export(overlay, grid, shots, spread, chrome, menu, guide);
        // Keep the process alive while the overlay window is hidden.
        std::mem::forget(app.hold());
    });
    app.run()
}

fn export(
    overlay: Rc<view::Overlay>,
    grid: Rc<appgrid::AppGrid>,
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
        let props = (overlay.clone(), grid.clone());
        let grid = grid.clone();
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
                        grid.hide();
                        shots.open();
                    }
                    "ShowScreenRecordingUI" => {
                        methods.hide();
                        grid.hide();
                        shots.toggle_screen_recording();
                    }
                    "ShowCopyTextUI" => {
                        methods.hide();
                        grid.hide();
                        shots.open_copy_text();
                    }
                    // Close the screenshot UI without taking a shot, as
                    // Escape does (tools/demo drives it hands-free).
                    "HideScreenshotUI" => shots.dismiss(),
                    "ToggleShortcuts" => {
                        if !guide.is_visible() {
                            methods.hide();
                            grid.hide();
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
                        methods.hide();
                        grid.toggle()
                    }
                    "ToggleAsk" => {
                        if !methods.is_visible() {
                            spread.hide();
                            grid.hide();
                            chrome.hide_switcher();
                            menu.hide();
                        }
                        methods.toggle()
                    }
                    "AskRows" | "AskActivate" | "TestAction" => {
                        if !gtk::glib::user_runtime_dir().join("umoja-demo").exists() {
                            invocation.return_dbus_error(
                                "org.freedesktop.DBus.Error.AccessDenied",
                                "only while $XDG_RUNTIME_DIR/umoja-demo exists",
                            );
                            return;
                        }
                        let reply = match method {
                            "AskRows" => Some((methods.test_rows(),).to_variant()),
                            "AskActivate" => params
                                .get::<(u32,)>()
                                .map(|(i,)| (methods.test_activate(i),).to_variant()),
                            _ => params
                                .get::<(String,)>()
                                .map(|(a,)| methods.test_action(&a).to_variant()),
                        };
                        match reply {
                            Some(v) => invocation.return_value(Some(&v)),
                            None => invocation.return_dbus_error(
                                "org.freedesktop.DBus.Error.InvalidArgs",
                                "bad arguments",
                            ),
                        }
                        return;
                    }
                    "AnswerConfirm" => {
                        let Some((allow,)) = params.get::<(bool,)>() else {
                            invocation.return_dbus_error(
                                "org.freedesktop.DBus.Error.InvalidArgs",
                                "expected (b)",
                            );
                            return;
                        };
                        // Confirmations guard what the assistant does, so
                        // only the demo, which sets this flag, answers them.
                        if !gtk::glib::user_runtime_dir().join("umoja-demo").exists() {
                            invocation.return_dbus_error(
                                "org.freedesktop.DBus.Error.AccessDenied",
                                "only while $XDG_RUNTIME_DIR/umoja-demo exists",
                            );
                            return;
                        }
                        let answered = methods.answer_confirm(allow);
                        invocation.return_value(Some(&(answered,).to_variant()));
                        return;
                    }
                    "AskType" => {
                        let Some((text, typed)) = params.get::<(String, bool)>() else {
                            invocation.return_dbus_error(
                                "org.freedesktop.DBus.Error.InvalidArgs",
                                "expected (sb)",
                            );
                            return;
                        };
                        spread.hide();
                        grid.hide();
                        methods.type_text(&text, typed)
                    }
                    "ToggleOverview" => {
                        // Drag chrome never stays above the overview.
                        chrome.tile_preview(false, (0, 0, 0, 0), (0, 0, 0, 0));
                        chrome.layout_picker(0, 0, 0, 0);
                        chrome.hide_switcher();
                        menu.hide();
                        // Super from the app grid closes it, as in GNOME.
                        methods.hide();
                        if grid.is_visible() {
                            grid.hide();
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
                        if let Some((mode, items, sel, sub, ids)) = params
                            .get::<(u32, chrome::SwitcherItems, u32, i32, Vec<Vec<u64>>)>(
                        ) {
                            chrome.show_switcher(mode, items, ids, sel, sub);
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
                        methods.hide();
                        if up {
                            if spread.is_visible() {
                                spread.hide();
                                grid.show();
                            } else if !grid.is_visible() {
                                chrome.hide_switcher();
                                spread.show();
                            }
                        } else if grid.is_visible() {
                            grid.hide();
                            spread.show();
                        } else if spread.is_visible() {
                            spread.hide();
                        }
                    }
                    "Show" => {
                        spread.hide();
                        methods.hide();
                        grid.show()
                    }
                    "Hide" => {
                        methods.hide();
                        grid.hide()
                    }
                    "ToggleDictation" => methods.toggle_dictation(),
                    "ToggleCaptions" => methods.toggle_captions(),
                    "AskAboutScreen" => {
                        spread.hide();
                        grid.hide();
                        methods.ask_about_screen()
                    }
                    "LaunchAgent" => {
                        spread.hide();
                        grid.hide();
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
                        grid.hide();
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
                        grid.hide();
                        methods.ask_with(&question, flags & 1 != 0, flags & 2 != 0)
                    }
                    "Speak" => {
                        let Some((text,)) = params.get::<(String,)>() else {
                            invocation.return_dbus_error(
                                "org.freedesktop.DBus.Error.InvalidArgs",
                                "expected (s)",
                            );
                            return;
                        };
                        methods.speak(&text)
                    }
                    "StopSpeaking" => methods.stop_speaking(),
                    "ShowClipboard" => {
                        spread.hide();
                        grid.hide();
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
                "Visible" => (props.0.is_visible() || props.1.is_visible()).to_variant(),
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
    umoja_common::icons::add_shell_icons();
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

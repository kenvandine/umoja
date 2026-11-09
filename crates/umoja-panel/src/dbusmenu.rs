//! `com.canonical.dbusmenu` client: turns an AppIndicator's exported menu
//! into a native GTK popover menu, with checkmarks, radio items, submenus,
//! disabled items and separators, as Ubuntu's AppIndicator extension does.

use gio::prelude::*;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use std::rc::Rc;

pub const IFACE: &str = "com.canonical.dbusmenu";
const GROUP: &str = "dbusmenu";

#[derive(Debug, Clone, PartialEq)]
pub enum Toggle {
    None,
    Check(bool),
    Radio(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub id: i32,
    pub label: String,
    pub separator: bool,
    pub enabled: bool,
    pub visible: bool,
    pub toggle: Toggle,
    pub children: Vec<Node>,
}

/// Parses one `(ia{sv}av)` layout node.
pub fn parse(v: &glib::Variant) -> Option<Node> {
    let id = v.try_child_value(0)?.get::<i32>()?;
    let props = glib::VariantDict::new(Some(&v.try_child_value(1)?));
    let s = |k: &str| props.lookup_value(k, None).and_then(|v| v.get::<String>());
    let b = |k: &str, d: bool| {
        props
            .lookup_value(k, None)
            .and_then(|v| v.get::<bool>())
            .unwrap_or(d)
    };
    let state = props
        .lookup_value("toggle-state", None)
        .and_then(|v| v.get::<i32>())
        .unwrap_or(0)
        == 1;
    let toggle = match s("toggle-type").as_deref() {
        Some("checkmark") => Toggle::Check(state),
        Some("radio") => Toggle::Radio(state),
        _ => Toggle::None,
    };
    let children = v
        .try_child_value(2)?
        .iter()
        .filter_map(|c| parse(&c.as_variant().unwrap_or(c)))
        .collect();
    Some(Node {
        id,
        label: s("label").unwrap_or_default(),
        separator: s("type").as_deref() == Some("separator"),
        enabled: b("enabled", true),
        visible: b("visible", true),
        toggle,
        children,
    })
}

fn action_name(id: i32) -> String {
    format!("i{}", id.unsigned_abs())
}

/// Builds the GMenu for `node`'s children; separators split sections.
fn build(node: &Node, actions: &gio::SimpleActionGroup, on_click: &Rc<dyn Fn(i32)>) -> gio::Menu {
    let menu = gio::Menu::new();
    let mut section = gio::Menu::new();
    for child in node.children.iter().filter(|c| c.visible) {
        if child.separator {
            if section.n_items() > 0 {
                menu.append_section(None, &section);
                section = gio::Menu::new();
            }
            continue;
        }
        let name = action_name(child.id);
        let detailed = format!("{GROUP}.{name}");
        let item = if !child.children.is_empty() {
            gio::MenuItem::new_submenu(Some(&child.label), &build(child, actions, on_click))
        } else {
            let (action, target) = match child.toggle {
                Toggle::Check(on) => (
                    gio::SimpleAction::new_stateful(&name, None, &on.to_variant()),
                    None,
                ),
                Toggle::Radio(on) => (
                    gio::SimpleAction::new_stateful(
                        &name,
                        Some(glib::VariantTy::STRING),
                        &(if on { "on" } else { "off" }).to_variant(),
                    ),
                    Some("on".to_variant()),
                ),
                Toggle::None => (gio::SimpleAction::new(&name, None), None),
            };
            action.set_enabled(child.enabled);
            let id = child.id;
            let on_click = on_click.clone();
            action.connect_activate(move |_, _| on_click(id));
            actions.add_action(&action);
            let item = gio::MenuItem::new(Some(&child.label), None);
            item.set_action_and_target_value(Some(&detailed), target.as_ref());
            item
        };
        section.append_item(&item);
    }
    if section.n_items() > 0 {
        menu.append_section(None, &section);
    }
    menu
}

/// One item's menu, shown in a popover anchored to its tray button.
pub struct Menu {
    proxy: gio::DBusProxy,
    popover: gtk::PopoverMenu,
}

impl Menu {
    pub fn new(
        conn: &gio::DBusConnection,
        bus: &str,
        path: &str,
        anchor: &gtk::Widget,
    ) -> Option<Rc<Self>> {
        let proxy = gio::DBusProxy::new_sync(
            conn,
            gio::DBusProxyFlags::DO_NOT_AUTO_START | gio::DBusProxyFlags::DO_NOT_LOAD_PROPERTIES,
            None,
            Some(bus),
            path,
            IFACE,
            gio::Cancellable::NONE,
        )
        .ok()?;
        let popover = gtk::PopoverMenu::from_model(None::<&gio::MenuModel>);
        popover.add_css_class("tray-menu");
        popover.set_has_arrow(false);
        umoja_common::blur::blur_behind(&popover);
        popover.set_parent(anchor);
        crate::menu_gap::below_bar(&popover);
        let menu = Rc::new(Self { proxy, popover });

        // Apps rebuild their menus while open (e.g. a "Connecting..." label);
        // follow along.
        let weak = Rc::downgrade(&menu);
        menu.proxy.connect_local("g-signal", false, move |args| {
            let signal = args[2].get::<String>().ok()?;
            let m = weak.upgrade()?;
            if matches!(signal.as_str(), "LayoutUpdated" | "ItemsPropertiesUpdated")
                && m.popover.is_visible()
            {
                m.fetch(false);
            }
            None
        });
        Some(menu)
    }

    pub fn popup(self: &Rc<Self>) {
        // Lets apps fill lazily-built menus before we read them.
        self.proxy.call(
            "AboutToShow",
            Some(&(0i32,).to_variant()),
            gio::DBusCallFlags::NONE,
            500,
            gio::Cancellable::NONE,
            |_| {},
        );
        self.fetch(true);
    }

    fn fetch(self: &Rc<Self>, show: bool) {
        let weak = Rc::downgrade(self);
        let no_props: Vec<String> = Vec::new();
        self.proxy.call(
            "GetLayout",
            Some(&(0i32, -1i32, no_props).to_variant()),
            gio::DBusCallFlags::NONE,
            2000,
            gio::Cancellable::NONE,
            move |res| {
                let Some(m) = weak.upgrade() else { return };
                let root = match res {
                    Ok(v) => v.try_child_value(1).and_then(|l| parse(&l)),
                    Err(e) => {
                        tracing::warn!(error = %e, "could not read tray menu");
                        None
                    }
                };
                let Some(root) = root else { return };
                let actions = gio::SimpleActionGroup::new();
                let proxy = m.proxy.clone();
                let on_click: Rc<dyn Fn(i32)> = Rc::new(move |id| {
                    let ts = (glib::monotonic_time() / 1000) as u32;
                    proxy.call(
                        "Event",
                        Some(
                            &(
                                id,
                                "clicked",
                                0i32.to_variant(),
                                ts,
                            )
                                .to_variant(),
                        ),
                        gio::DBusCallFlags::NONE,
                        2000,
                        gio::Cancellable::NONE,
                        |_| {},
                    );
                });
                let model = build(&root, &actions, &on_click);
                m.popover.insert_action_group(GROUP, Some(&actions));
                m.popover.set_menu_model(Some(&model));
                if show && model.n_items() > 0 {
                    m.popover.popup();
                }
            },
        );
    }
}

impl Drop for Menu {
    fn drop(&mut self) {
        self.popover.unparent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn node(
        id: i32,
        props: &[(&str, glib::Variant)],
        children: Vec<glib::Variant>,
    ) -> glib::Variant {
        let p: HashMap<String, glib::Variant> = props
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        // `av` boxes each child once, as a real GetLayout reply does.
        let kids = children;
        (id, p, kids).to_variant()
    }

    #[test]
    fn parses_layout() {
        let layout = node(
            0,
            &[("children-display", "submenu".to_variant())],
            vec![
                node(1, &[("label", "_Open".to_variant())], vec![]),
                node(2, &[("type", "separator".to_variant())], vec![]),
                node(
                    3,
                    &[
                        ("label", "Mute".to_variant()),
                        ("toggle-type", "checkmark".to_variant()),
                        ("toggle-state", 1i32.to_variant()),
                        ("enabled", false.to_variant()),
                    ],
                    vec![],
                ),
                node(4, &[("visible", false.to_variant())], vec![]),
            ],
        );
        let root = parse(&layout).unwrap();
        assert_eq!(root.children.len(), 4);
        assert_eq!(root.children[0].label, "_Open");
        assert!(root.children[1].separator);
        assert_eq!(root.children[2].toggle, Toggle::Check(true));
        assert!(!root.children[2].enabled);
        assert!(!root.children[3].visible);
    }
}

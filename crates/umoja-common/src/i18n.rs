//! Internationalisation.
//!
//! Umoja does not translate standard desktop phrases itself. "Cancel",
//! "Settings", "Activities", "Power Off" and friends come from the catalogs
//! Ubuntu already ships for gnome-shell, GTK and gnome-control-center. Only
//! the AI-specific strings live in our own `umoja-shell` domain.

use gettextrs::{
    LocaleCategory, bind_textdomain_codeset, bindtextdomain, dgettext, setlocale, textdomain,
};

pub const DOMAIN_UMOJA: &str = "umoja-shell";
pub const DOMAIN_SHELL: &str = "gnome-shell";
pub const DOMAIN_GTK: &str = "gtk40";
pub const DOMAIN_CONTROL_CENTER: &str = "gnome-control-center-2.0";

const LOCALEDIR: &str = "/usr/share/locale";

/// Bind all text domains and make `umoja-shell` the default one.
pub fn init() {
    setlocale(LocaleCategory::LcAll, "");
    for domain in [
        DOMAIN_UMOJA,
        DOMAIN_SHELL,
        DOMAIN_GTK,
        DOMAIN_CONTROL_CENTER,
    ] {
        let _ = bindtextdomain(domain, LOCALEDIR);
        let _ = bind_textdomain_codeset(domain, "UTF-8");
    }
    let _ = textdomain(DOMAIN_UMOJA);
}

/// A string from gnome-shell's catalog ("Activities", "Power Off", "Log Out"...).
pub fn shell(msgid: &str) -> String {
    dgettext(DOMAIN_SHELL, msgid)
}

/// A string from GTK's catalog ("Cancel", "OK"...).
pub fn gtk(msgid: &str) -> String {
    dgettext(DOMAIN_GTK, msgid)
}

/// A string from gnome-control-center's catalog ("Settings", "Network"...).
pub fn control_center(msgid: &str) -> String {
    dgettext(DOMAIN_CONTROL_CENTER, msgid)
}

/// An Umoja-specific string (AI assistant, Myna pill...).
pub fn umoja(msgid: &str) -> String {
    dgettext(DOMAIN_UMOJA, msgid)
}

//! What to ask for: gnome-shell's NetworkSecretDialog._getContent(), built
//! from the connection NetworkManager hands the agent.

use crate::bus::Props;
use gtk4::glib;
use std::collections::HashMap;
use umoja_common::i18n;

// NMSettingSecretFlags
pub const AGENT_OWNED: u32 = 0x1;
pub const NOT_SAVED: u32 = 0x2;

/// A connection as `a{sa{sv}}`: setting name -> its properties.
#[derive(Debug, Clone, Default)]
pub struct Conn(pub HashMap<String, Props>);

impl Conn {
    pub fn from_variant(v: &glib::Variant) -> Self {
        let mut map = HashMap::new();
        for e in v.iter() {
            if let Some(name) = e.child_value(0).str() {
                map.insert(name.to_string(), Props::from_dict(&e.child_value(1)));
            }
        }
        Self(map)
    }

    pub fn section(&self, name: &str) -> Option<&Props> {
        self.0.get(name)
    }

    pub fn str(&self, section: &str, key: &str) -> Option<String> {
        self.section(section)?.str(key)
    }

    pub fn u32(&self, section: &str, key: &str) -> Option<u32> {
        self.section(section)?.u32(key)
    }

    pub fn strs(&self, section: &str, key: &str) -> Vec<String> {
        self.section(section).map(|p| p.objs(key)).unwrap_or_default()
    }

    pub fn uuid(&self) -> String {
        self.str("connection", "uuid").unwrap_or_default()
    }

    pub fn id(&self) -> String {
        self.str("connection", "id").unwrap_or_default()
    }

    pub fn kind(&self) -> String {
        self.str("connection", "type").unwrap_or_default()
    }

    pub fn ssid(&self) -> String {
        let bytes = self
            .section("802-11-wireless")
            .map(|p| p.bytes("ssid"))
            .unwrap_or_default();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// An `a{ss}` property (the VPN's `data` and `secrets`).
    pub fn dict(&self, section: &str, key: &str) -> Vec<(String, String)> {
        let Some(v) = self.section(section).and_then(|p| p.get(key)) else {
            return vec![];
        };
        v.iter()
            .filter_map(|e| {
                Some((
                    e.child_value(0).str()?.to_string(),
                    e.child_value(1).str()?.to_string(),
                ))
            })
            .collect()
    }

    /// The flags NetworkManager keeps beside a secret.
    pub fn secret_flags(&self, section: &str, key: &str) -> u32 {
        if section == "vpn" {
            return self
                .dict("vpn", "data")
                .into_iter()
                .find(|(k, _)| *k == format!("{key}-flags"))
                .and_then(|(_, v)| v.parse().ok())
                .unwrap_or(0);
        }
        let flags_key = if key.starts_with("wep-key") && key.len() == 8 {
            "wep-key-flags".to_string()
        } else {
            format!("{key}-flags")
        };
        self.u32(section, &flags_key).unwrap_or(0)
    }

    /// Some secret is "ask every time" (NOT_SAVED): gnome-shell's
    /// is_connection_always_ask(), which skips the keyring then.
    pub fn always_ask(&self) -> bool {
        let kind = self.kind();
        let mut sections = vec![kind.clone()];
        match kind.as_str() {
            "802-11-wireless" => sections.extend(["802-11-wireless-security".into(), "802-1x".into()]),
            "802-3-ethernet" => sections.extend(["pppoe".into(), "802-1x".into()]),
            _ => {}
        }
        sections.iter().any(|s| {
            if s == "vpn" {
                return self
                    .dict("vpn", "data")
                    .iter()
                    .any(|(k, v)| k.ends_with("-flags") && v.parse::<u32>().is_ok_and(|f| f & NOT_SAVED != 0));
            }
            secret_keys(s)
                .iter()
                .any(|k| self.secret_flags(s, k) & NOT_SAVED != 0)
        })
    }
}

/// The secret properties of each setting NetworkManager has (the ones
/// libnm marks NM_SETTING_PARAM_SECRET and that hold a string).
pub fn secret_keys(setting: &str) -> &'static [&'static str] {
    match setting {
        "802-11-wireless-security" => &["psk", "wep-key0", "wep-key1", "wep-key2", "wep-key3", "leap-password"],
        "802-1x" => &[
            "password",
            "private-key-password",
            "phase2-private-key-password",
            "pin",
        ],
        "pppoe" | "cdma" | "adsl" => &["password"],
        "gsm" => &["password", "pin"],
        "wireguard" => &["private-key"],
        "macsec" => &["mka-cak"],
        _ => &[],
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validate {
    NonEmpty,
    WpaPsk,
    /// Static WEP, by NMWepKeyType (0 unknown, 1 key, 2 passphrase).
    Wep(u32),
}

impl Validate {
    pub fn check(self, value: &str) -> bool {
        let hex = |s: &str| s.chars().all(|c| c.is_ascii_hexdigit());
        let len = value.chars().count();
        match self {
            Validate::NonEmpty => !value.is_empty(),
            Validate::WpaPsk => {
                if len == 64 {
                    hex(value)
                } else {
                    (8..=63).contains(&len)
                }
            }
            Validate::Wep(1) => match len {
                10 | 26 => hex(value),
                5 | 13 => value.chars().all(|c| c.is_ascii_alphabetic()),
                _ => false,
            },
            Validate::Wep(2) => len <= 64,
            Validate::Wep(_) => true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Secret {
    pub label: String,
    /// The setting key the answer goes to; `None` is shown, not asked.
    pub key: Option<String>,
    pub value: String,
    pub password: bool,
    pub validate: Validate,
}

impl Secret {
    fn ask(label: &str, key: &str, value: Option<String>, password: bool) -> Self {
        Self {
            label: i18n::shell(label),
            key: Some(key.into()),
            value: value.unwrap_or_default(),
            password,
            validate: Validate::NonEmpty,
        }
    }

    fn show(label: &str, value: Option<String>) -> Self {
        Self {
            key: None,
            ..Self::ask(label, "", value, false)
        }
    }

    fn validated(mut self, v: Validate) -> Self {
        self.validate = v;
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct Content {
    pub title: String,
    pub message: Option<String>,
    pub secrets: Vec<Secret>,
    /// WPS push-button is possible: say so, as gnome-shell does.
    pub wps: bool,
}

fn format1(template: &str, arg: &str) -> String {
    i18n::shell(template).replacen("%s", arg, 1)
}

/// The dialog for a non-VPN request. `known` holds secrets found in the
/// keyring, used to fill the entries in.
pub fn build(conn: &Conn, setting: &str, hints: &[String], known: &HashMap<String, String>) -> Option<Content> {
    let value = |section: &str, key: &str| known.get(key).cloned().or_else(|| conn.str(section, key));
    let mut c = Content::default();
    match conn.kind().as_str() {
        "802-11-wireless" => {
            c.title = i18n::shell("Authentication required");
            c.message = Some(format1(
                "Passwords or encryption keys are required to access the wireless network “%s”",
                &conn.ssid(),
            ));
            wireless(conn, setting, hints, &value, &mut c.secrets);
        }
        "802-3-ethernet" => {
            c.title = i18n::shell("Wired 802.1X authentication");
            c.secrets.push(Secret::show("Network name", Some(conn.id())));
            ieee8021x(conn, setting, hints, &value, &mut c.secrets);
        }
        "pppoe" => {
            c.title = i18n::shell("DSL authentication");
            c.secrets.push(Secret::ask("Username", "username", value("pppoe", "username"), false));
            c.secrets.push(Secret::ask("Service", "service", value("pppoe", "service"), false));
            c.secrets.push(Secret::ask("Password", "password", value("pppoe", "password"), true));
        }
        "gsm" if hints.iter().any(|h| h == "pin") => {
            c.title = i18n::shell("PIN code required");
            c.message = Some(i18n::shell("PIN code is needed for the mobile broadband device"));
            c.secrets.push(Secret::ask("PIN", "pin", value("gsm", "pin"), true));
        }
        kind @ ("gsm" | "cdma" | "bluetooth") => {
            c.title = i18n::shell("Authentication required");
            c.message = Some(format1("A password is required to connect to “%s”", &conn.id()));
            let section = if kind == "bluetooth" {
                if conn.section("cdma").is_some() { "cdma" } else { "gsm" }
            } else {
                kind
            };
            c.secrets.push(Secret::ask("Password", "password", value(section, "password"), true));
        }
        other => {
            tracing::warn!(kind = other, "network agent: no dialog for this connection type");
            return None;
        }
    }
    (!c.secrets.is_empty()).then_some(c)
}

fn wireless(
    conn: &Conn,
    setting: &str,
    hints: &[String],
    value: &dyn Fn(&str, &str) -> Option<String>,
    out: &mut Vec<Secret>,
) {
    if setting == "802-1x" {
        ieee8021x(conn, setting, hints, value, out);
        return;
    }
    const SEC: &str = "802-11-wireless-security";
    match conn.str(SEC, "key-mgmt").as_deref() {
        Some("wpa-none" | "wpa-psk" | "sae") => {
            out.push(Secret::ask("Password", "psk", value(SEC, "psk"), true).validated(Validate::WpaPsk));
        }
        Some("none") => {
            let idx = conn.u32(SEC, "wep-tx-keyidx").unwrap_or(0).min(3);
            let key = format!("wep-key{idx}");
            let kind = conn.u32(SEC, "wep-key-type").unwrap_or(0);
            out.push(Secret::ask("Key", &key, value(SEC, &key), true).validated(Validate::Wep(kind)));
        }
        Some("ieee8021x") => {
            if conn.str(SEC, "auth-alg").as_deref() == Some("leap") {
                out.push(Secret::ask("Password", "leap-password", value(SEC, "leap-password"), true));
            } else {
                ieee8021x(conn, setting, hints, value, out);
            }
        }
        Some("wpa-eap" | "wpa-eap-suite-b-192") => ieee8021x(conn, setting, hints, value, out),
        other => tracing::warn!(key_mgmt = ?other, "network agent: unknown wireless key management"),
    }
}

fn ieee8021x(
    conn: &Conn,
    setting: &str,
    hints: &[String],
    value: &dyn Fn(&str, &str) -> Option<String>,
    out: &mut Vec<Secret>,
) {
    const X: &str = "802-1x";
    // With hints NetworkManager says exactly what it needs.
    if setting == X && !hints.is_empty() {
        let has = |h: &str| hints.iter().any(|x| x == h);
        if has("identity") {
            out.push(Secret::ask("Username", "identity", value(X, "identity"), false));
        }
        if has("password") {
            out.push(Secret::ask("Password", "password", value(X, "password"), true));
        }
        if has("private-key-password") {
            out.push(Secret::ask(
                "Private key password",
                "private-key-password",
                value(X, "private-key-password"),
                true,
            ));
        }
        return;
    }
    let eap = conn.strs(X, "eap");
    match eap.first().map(String::as_str) {
        Some("md5" | "leap" | "ttls" | "peap" | "fast" | "pwd") => {
            out.push(Secret::show("Username", value(X, "identity")));
            out.push(Secret::ask("Password", "password", value(X, "password"), true));
        }
        Some("tls") => {
            out.push(Secret::show("Identity", value(X, "identity")));
            out.push(Secret::ask(
                "Private key password",
                "private-key-password",
                value(X, "private-key-password"),
                true,
            ));
        }
        other => tracing::warn!(eap = ?other, "network agent: unknown EAP method"),
    }
}

/// The notification shown when NetworkManager asks on its own (an
/// automatic reconnect), as gnome-shell's _showNotification().
pub fn notification(conn: &Conn, hints: &[String]) -> Option<(String, String)> {
    let needs_password = |id: &str| format1("A password is required to connect to “%s”", id);
    Some(match conn.kind().as_str() {
        "802-11-wireless" => (
            i18n::shell("Authentication required"),
            format1(
                "Passwords or encryption keys are required to access the wireless network “%s”",
                &conn.ssid(),
            ),
        ),
        "802-3-ethernet" => (i18n::shell("Wired 802.1X authentication"), needs_password(&conn.id())),
        "pppoe" => (i18n::shell("DSL authentication"), needs_password(&conn.id())),
        "gsm" if hints.iter().any(|h| h == "pin") => (
            i18n::shell("PIN code required"),
            i18n::shell("PIN code is needed for the mobile broadband device"),
        ),
        "gsm" | "cdma" | "bluetooth" => (i18n::shell("Authentication required"), needs_password(&conn.id())),
        "vpn" => (i18n::shell("VPN password"), needs_password(&conn.id())),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wpa_psk_rules() {
        assert!(!Validate::WpaPsk.check("short"));
        assert!(Validate::WpaPsk.check("eight888"));
        assert!(Validate::WpaPsk.check(&"a".repeat(63)));
        assert!(!Validate::WpaPsk.check(&"g".repeat(64)));
        assert!(Validate::WpaPsk.check(&"f".repeat(64)));
    }

    #[test]
    fn wep_rules() {
        assert!(Validate::Wep(1).check("0123456789"));
        assert!(!Validate::Wep(1).check("012345678g"));
        assert!(Validate::Wep(1).check("abcde"));
        assert!(!Validate::Wep(1).check("abcd"));
        assert!(Validate::Wep(2).check("any passphrase"));
        assert!(Validate::Wep(0).check(""));
    }
}

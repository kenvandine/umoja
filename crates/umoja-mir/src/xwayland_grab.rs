//! Which X11 apps may take every key with a keyboard grab (Xwayland's
//! xwayland-keyboard-grab, shim/wm_protocols.cpp). As mutter: the patterns
//! of `org.gnome.mutter.wayland xwayland-grab-access-rules` (a leading `!`
//! denies), matched against the window's app id (WM_CLASS), plus Umoja's
//! own list of virtual machine and remote desktop viewers. Super+Escape
//! takes the keys back either way.

use umoja_common::gsettings as gs;

const MUTTER_WAYLAND: &str = "org.gnome.mutter.wayland";

/// mutter's built-in rules, and the X11 viewers Umoja users run (QEMU's
/// SDL window under quickemu, VMware, TigerVNC, FreeRDP, Xephyr).
const DEFAULT_RULES: &[&str] = &[
    "VirtualBox Machine",
    "Remmina",
    "org.remmina.Remmina",
    "qemu*",
    "vmware*",
    "Vncviewer",
    "xfreerdp*",
    "Xephyr",
];

pub fn allowed(app_id: &str) -> bool {
    let rules = gs::get_strv(MUTTER_WAYLAND, "xwayland-grab-access-rules");
    decide(app_id, &rules)
}

/// Denied by a `!` rule first, then allowed by a rule or Umoja's list.
fn decide(app_id: &str, rules: &[String]) -> bool {
    if app_id.is_empty() {
        return false;
    }
    if rules
        .iter()
        .filter_map(|r| r.strip_prefix('!'))
        .any(|p| glob(p, app_id))
    {
        return false;
    }
    rules
        .iter()
        .filter(|r| !r.starts_with('!'))
        .map(String::as_str)
        .chain(DEFAULT_RULES.iter().copied())
        .any(|p| glob(p, app_id))
}

/// `*` and `?` wildcards, ignoring case.
fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(r: &[&str]) -> Vec<String> {
        r.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn wildcards_ignore_case() {
        assert!(glob("qemu*", "qemu-system-x86_64"));
        assert!(glob("qemu*", "Qemu-system-x86_64"));
        assert!(glob("VirtualBox Machine", "virtualbox machine"));
        assert!(glob("a?c", "abc"));
        assert!(!glob("qemu*", "firefox"));
        assert!(!glob("abc", "abcd"));
    }

    #[test]
    fn viewers_are_allowed_others_not() {
        assert!(decide("qemu-system-x86_64", &[]));
        assert!(decide("Remmina", &[]));
        assert!(!decide("firefox", &[]));
        assert!(!decide("", &rules(&["*"])));
    }

    #[test]
    fn rules_add_and_deny() {
        assert!(decide("Steam", &rules(&["steam"])));
        assert!(!decide("qemu-system-x86_64", &rules(&["!qemu*"])));
    }
}

//! 12- or 24-hour time, the way the user's locale writes it.
//!
//! GNOME's `org.gnome.desktop.interface clock-format` defaults to "24h"
//! everywhere, so a US user sees 24-hour time until they find the switch.
//! Umoja treats that key as an override: when the user has set it
//! (Settings, Date & Time, Time Format) it wins, otherwise the locale's own time
//! format (`LC_TIME`'s `t_fmt`) decides, so en_US gets 12-hour time and
//! en_GB, de_DE, fr_FR... get 24-hour time.

use crate::gsettings as gs;
use gio::prelude::*;
use std::ffi::CStr;

const KEY: &str = "clock-format";

/// True when clocks should show 12-hour time with AM/PM.
pub fn twelve_hour() -> bool {
    if let Some(s) = gs::settings(gs::DESKTOP_INTERFACE)
        && gs::has_key(&s, KEY)
        && s.user_value(KEY).is_some()
    {
        return s.string(KEY) == "12h";
    }
    locale_twelve_hour()
}

/// True when the `LC_TIME` locale from the environment writes times with a
/// 12-hour clock. Reads the environment directly (not the process's
/// current locale) so it works before or without `setlocale`.
pub fn locale_twelve_hour() -> bool {
    // SAFETY: newlocale with an empty name reads LC_ALL/LC_TIME/LANG; the
    // returned string is owned by `loc` and copied before freelocale.
    unsafe {
        let loc = libc::newlocale(libc::LC_TIME_MASK, c"".as_ptr(), std::ptr::null_mut());
        if loc.is_null() {
            return false;
        }
        let p = libc::nl_langinfo_l(libc::T_FMT, loc);
        let twelve = !p.is_null() && is_twelve_hour_pattern(CStr::from_ptr(p).to_bytes());
        libc::freelocale(loc);
        twelve
    }
}

/// True when an strftime pattern uses a 12-hour conversion
/// (`%I`, `%l`, `%r`, `%p`, `%P`, with optional flags or `E`/`O`).
fn is_twelve_hour_pattern(fmt: &[u8]) -> bool {
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i] != b'%' {
            i += 1;
            continue;
        }
        i += 1;
        while i < fmt.len() && b"_-0^#EO".contains(&fmt[i]) {
            i += 1;
        }
        if i < fmt.len() && b"IlrpP".contains(&fmt[i]) {
            return true;
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::is_twelve_hour_pattern as t;

    #[test]
    fn patterns() {
        assert!(t(b"%r")); // en_US
        assert!(t(b"%I:%M:%S %p"));
        assert!(t(b"%-l:%M %P"));
        assert!(!t(b"%T")); // en_GB, de_DE
        assert!(!t(b"%H:%M:%S"));
        assert!(!t(b"%Hh%M")); // fr_CA-ish
        assert!(!t(b"100%%")); // literal percent
        assert!(!t(b""));
    }
}

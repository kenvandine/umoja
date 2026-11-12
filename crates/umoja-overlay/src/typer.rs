//! Type text into the focused window, as `wtype` does.
//!
//! Uses `zwp_virtual_keyboard_v1` (which umoja-mir offers for Myna's
//! dictation too) on a Wayland connection of its own: a keymap is made
//! that maps one keycode to each character needed (as Unicode keysyms, so
//! emoji and accents need no layout), then each character is pressed and
//! released. Nothing touches the clipboard. Used to put an emoji, or
//! rewritten text over a selection, into the app the user was in.

use std::io::Write;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::time::Duration;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

/// Keycodes per keymap; longer text is typed in several rounds.
const CHUNK: usize = 200;
/// Characters typed in one call at most (a runaway model answer).
pub const MAX_CHARS: usize = 20_000;

/// The keysym name for `c` in an XKB keymap.
pub fn keysym_name(c: char) -> String {
    match c {
        '\n' => "Return".into(),
        '\t' => "Tab".into(),
        c => format!("U{:04X}", c as u32),
    }
}

/// An XKB keymap with keycode 9 + i for `chars[i]`.
pub fn keymap(chars: &[char]) -> String {
    let mut codes = String::new();
    let mut syms = String::new();
    for (i, c) in chars.iter().enumerate() {
        codes.push_str(&format!("  <K{i}> = {};\n", i + 9));
        syms.push_str(&format!("  key <K{i}> {{[ {} ]}};\n", keysym_name(*c)));
    }
    format!(
        "xkb_keymap {{\nxkb_keycodes \"umoja\" {{\n  minimum = 8;\n  maximum = {};\n{codes}}};\n\
         xkb_types \"umoja\" {{ include \"complete\" }};\n\
         xkb_compat \"umoja\" {{ include \"complete\" }};\n\
         xkb_symbols \"umoja\" {{\n{syms}}};\n}};\n",
        chars.len() + 9
    )
}

/// The text as rounds of (distinct characters, the text's characters as
/// indexes into them), each round at most [`CHUNK`] distinct characters.
/// `\r` is dropped (a `\r\n` is one Return).
pub fn plan(text: &str) -> Vec<(Vec<char>, Vec<usize>)> {
    let mut rounds = Vec::new();
    let mut chars: Vec<char> = Vec::new();
    let mut seq: Vec<usize> = Vec::new();
    for c in text.chars().filter(|c| *c != '\r').take(MAX_CHARS) {
        let i = match chars.iter().position(|x| *x == c) {
            Some(i) => i,
            None => {
                if chars.len() == CHUNK {
                    rounds.push((std::mem::take(&mut chars), std::mem::take(&mut seq)));
                }
                chars.push(c);
                chars.len() - 1
            }
        };
        seq.push(i);
    }
    if !seq.is_empty() {
        rounds.push((chars, seq));
    }
    rounds
}

struct State;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlSeat, ()> for State {
    fn event(_: &mut Self, _: &WlSeat, _: <WlSeat as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for State {
    fn event(_: &mut Self, _: &ZwpVirtualKeyboardManagerV1, _: <ZwpVirtualKeyboardManagerV1 as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ZwpVirtualKeyboardV1, ()> for State {
    fn event(_: &mut Self, _: &ZwpVirtualKeyboardV1, _: <ZwpVirtualKeyboardV1 as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

fn keymap_fd(map: &str) -> Result<(OwnedFd, u32), String> {
    let name = c"umoja-typer";
    // SAFETY: memfd_create with a valid C string; the fd is owned below.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err("memfd_create failed".into());
    }
    // SAFETY: fd is a fresh, valid descriptor we own.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut f = std::fs::File::from(fd.try_clone().map_err(|e| e.to_string())?);
    f.write_all(map.as_bytes()).map_err(|e| e.to_string())?;
    f.write_all(&[0]).map_err(|e| e.to_string())?;
    Ok((fd, map.len() as u32 + 1))
}

/// Type `text` into whatever has the keyboard focus. Blocking: run it off
/// the GTK thread.
pub fn type_text(text: &str) -> Result<(), String> {
    let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).map_err(|e| e.to_string())?;
    let qh = queue.handle();
    let manager = globals
        .bind::<ZwpVirtualKeyboardManagerV1, _, _>(&qh, 1..=1, ())
        .map_err(|_| "the compositor has no virtual keyboard".to_string())?;
    let seat = globals.bind::<WlSeat, _, _>(&qh, 1..=7, ()).map_err(|e| e.to_string())?;
    let kb = manager.create_virtual_keyboard(&seat, &qh, ());
    let mut state = State;
    let mut time = 1u32;
    for (chars, seq) in plan(text) {
        let (fd, size) = keymap_fd(&keymap(&chars))?;
        // 1 = WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1.
        kb.keymap(1, fd.as_fd(), size);
        kb.modifiers(0, 0, 0, 0);
        queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
        for i in seq {
            let key = (i + 1) as u32; // evdev code = XKB keycode - 8
            kb.key(time, key, 1);
            kb.key(time + 1, key, 0);
            time += 2;
            conn.flush().map_err(|e| e.to_string())?;
            std::thread::sleep(Duration::from_millis(2));
        }
        queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    }
    kb.destroy();
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    drop(manager);
    Ok(())
}

/// [`type_text`] after a short pause (so the overlay has gone and the
/// app has the keyboard again), off the GTK thread.
pub async fn type_into_focused(text: String) -> Result<(), String> {
    glib::timeout_future(Duration::from_millis(250)).await;
    gio::spawn_blocking(move || type_text(&text))
        .await
        .unwrap_or_else(|_| Err("typing crashed".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keysyms() {
        assert_eq!(keysym_name('a'), "U0061");
        assert_eq!(keysym_name('🎉'), "U1F389");
        assert_eq!(keysym_name('\n'), "Return");
    }

    #[test]
    fn plan_reuses_keys_and_chunks() {
        let p = plan("abba\r\n");
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].0, vec!['a', 'b', '\n']);
        assert_eq!(p[0].1, vec![0, 1, 1, 0, 2]);
        let many: String = (0..450).map(|i| char::from_u32(0x4e00 + i).unwrap()).collect();
        let p = plan(&many);
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].0.len(), CHUNK);
        assert_eq!(p.iter().map(|r| r.1.len()).sum::<usize>(), 450);
    }

    #[test]
    fn keymap_lists_every_key() {
        let m = keymap(&['x', '🎉']);
        assert!(m.contains("<K1> = 10;"));
        assert!(m.contains("key <K1> {[ U1F389 ]};"));
        assert!(m.contains("maximum = 11;"));
    }
}

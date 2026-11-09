//! Running commands where the user can see them: commands that need a
//! password (sudo, pkexec) or a package manager open in a terminal.

use crate::i18n;
use std::process::Command;

/// True when `cmd` must run in a terminal: it asks for a password
/// (sudo, pkexec) or uses a package manager, which prompts and prints
/// progress.
pub fn needs_terminal(cmd: &str) -> bool {
    const TOOLS: &[&str] = &[
        "sudo", "pkexec", "apt", "apt-get", "aptitude", "dpkg", "snap", "flatpak",
    ];
    cmd.split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')'))
        .filter(|w| !w.is_empty())
        .any(|w| TOOLS.contains(&w.rsplit('/').next().unwrap_or(w)))
}

/// Run `cmd` in the user's terminal, kept open afterwards so the result
/// can be read.
pub fn spawn_in_terminal(cmd: &str) -> std::io::Result<()> {
    let script = format!("{cmd}\nstatus=$?\necho\nprintf '%s' \"$1\"\nread -r _\nexit $status");
    let prompt = i18n::umoja("Press Enter to close this window.");
    let shell = ["sh", "-c", script.as_str(), "umoja-run", prompt.as_str()];
    // xdg-terminal-exec takes the command directly; the others after a flag.
    let candidates: [(&str, &[&str]); 4] = [
        ("xdg-terminal-exec", &[]),
        ("ptyxis", &["--"]),
        ("gnome-terminal", &["--"]),
        ("x-terminal-emulator", &["-e"]),
    ];
    let mut last = std::io::Error::from(std::io::ErrorKind::NotFound);
    for (bin, flag) in candidates {
        if glib::find_program_in_path(bin).is_none() {
            continue;
        }
        match Command::new(bin)
            .args(flag)
            .args(shell)
            .current_dir(glib::home_dir())
            .spawn()
        {
            Ok(child) => {
                // Its own app scope, not the launching shell part's cgroup.
                crate::app_scope::move_to_scope(bin, child.id());
                return Ok(());
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Run `argv` in a new terminal window in `dir`, as an interactive
/// program (a coding agent): no shell in between and no "press Enter" at
/// the end.
pub fn spawn_program_in_terminal(argv: &[String], dir: &std::path::Path) -> std::io::Result<()> {
    let candidates: [(&str, &[&str]); 4] = [
        ("xdg-terminal-exec", &[]),
        ("ptyxis", &["--"]),
        ("gnome-terminal", &["--"]),
        ("x-terminal-emulator", &["-e"]),
    ];
    let mut last = std::io::Error::from(std::io::ErrorKind::NotFound);
    for (bin, flag) in candidates {
        if glib::find_program_in_path(bin).is_none() {
            continue;
        }
        match Command::new(bin)
            .args(flag)
            .args(argv)
            .current_dir(dir)
            .spawn()
        {
            Ok(child) => {
                // Its own app scope, not the launching shell part's cgroup.
                crate::app_scope::move_to_scope(bin, child.id());
                return Ok(());
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::needs_terminal;

    #[test]
    fn install_commands_use_a_terminal() {
        assert!(needs_terminal("sudo apt install vlc"));
        assert!(needs_terminal("pkexec apt-get install -y gimp"));
        assert!(needs_terminal("snap install spotify"));
        assert!(needs_terminal("flatpak install flathub org.gimp.GIMP"));
        assert!(needs_terminal("cd /tmp && /usr/bin/sudo make install"));
        assert!(!needs_terminal("xdg-open ~/Downloads"));
        assert!(!needs_terminal(
            "gsettings set org.gnome.desktop.interface color-scheme prefer-dark"
        ));
        assert!(!needs_terminal("echo snapshot"));
    }
}

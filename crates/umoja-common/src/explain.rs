//! Plain-language lines for shell commands, so a confirmation reads
//! "Read your current display setup" rather than a gdbus call. The model
//! is asked for its own `description`; this is the fallback when it gives
//! none (or gives the command back).

use serde_json::Value;

/// The line shown for a command-running tool call: the model's
/// `description` when it gave a usable one, else a guess from `command`.
pub fn summary(args: &Value) -> String {
    let get = |k: &str| args.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let cmd = get("command").unwrap_or("");
    match get("description") {
        Some(d) if usable(d, cmd) => sentence(d),
        _ => command(cmd),
    }
}

/// A description that is plain words, not the command again.
fn usable(desc: &str, cmd: &str) -> bool {
    let d = desc.trim();
    if d.len() < 4 || d.len() > 160 {
        return false;
    }
    let c = cmd.trim();
    if !c.is_empty() && (d.contains(c) || c.starts_with(d.trim_matches(|ch| ch == '"' || ch == '“' || ch == '”'))) {
        return false;
    }
    // Shell syntax means it's still a command line.
    !d.contains(" | ") && !d.contains("--") && !d.contains("&&") && !d.contains('$')
}

/// Capitalised, without a trailing full stop (step lines have none).
fn sentence(s: &str) -> String {
    let s = s.trim().trim_end_matches('.').trim();
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// A best guess at what `cmd` does, from the programs in it.
pub fn command(cmd: &str) -> String {
    let progs = programs(cmd);
    let has = |p: &str| progs.iter().any(|x| x == p);
    let lower = cmd.to_lowercase();
    let first = progs.first().map(String::as_str).unwrap_or("");
    let line = match first {
        "gdbus" | "dbus-send" | "busctl" | "qdbus" => {
            if lower.contains("displayconfig") || lower.contains("monitor") {
                "Read your display setup"
            } else if lower.contains(".get") || lower.contains(" get") || lower.contains("introspect") {
                "Ask a desktop service for information"
            } else {
                "Send a request to a desktop service"
            }
        }
        "gsettings" | "dconf" => {
            if lower.contains(" set ") || lower.contains(" write ") || lower.contains(" reset") {
                "Change a desktop setting"
            } else {
                "Read a desktop setting"
            }
        }
        "ls" | "find" | "tree" | "fd" | "fdfind" | "locate" => "Look through your files",
        "cat" | "head" | "tail" | "less" | "more" | "bat" | "batcat" | "wc" | "file" | "stat" => "Read a file",
        "grep" | "rg" | "egrep" | "fgrep" => "Search inside files",
        "ps" | "pgrep" | "top" | "htop" | "btop" | "pidof" => "Check which programs are running",
        "kill" | "pkill" | "killall" => "Stop a running program",
        "df" | "du" | "ncdu" => "Check disk space",
        "free" | "vmstat" => "Check memory use",
        "uname" | "lsb_release" | "hostnamectl" | "hostname" | "uptime" | "neofetch" | "fastfetch" => {
            "Check system information"
        }
        "lspci" | "lsusb" | "lscpu" | "lshw" | "inxi" | "dmidecode" | "sensors" | "nvidia-smi" => {
            "Look at your hardware"
        }
        "lsblk" | "blkid" | "mount" | "findmnt" | "udisksctl" => "Look at your drives",
        "ip" | "ifconfig" | "nmcli" | "iwconfig" | "iw" | "ss" | "netstat" | "resolvectl" => {
            "Check your network"
        }
        "ping" | "traceroute" | "dig" | "nslookup" | "host" => "Test your internet connection",
        "curl" | "wget" => "Download from the internet",
        "journalctl" | "dmesg" => "Read the system logs",
        "systemctl" => {
            if ["start", "stop", "restart", "enable", "disable", "mask", "kill"]
                .iter()
                .any(|v| lower.split_whitespace().any(|w| w == *v))
            {
                "Change a system service"
            } else {
                "Check system services"
            }
        }
        "pactl" | "wpctl" | "amixer" | "pw-cli" | "pw-dump" => "Check your sound devices",
        "xrandr" | "wlr-randr" | "xdpyinfo" => "Read your display setup",
        "rm" | "rmdir" | "shred" | "trash" | "trash-put" => "Delete files",
        "mv" | "cp" | "rsync" => "Move or copy files",
        "mkdir" | "touch" => "Create a file or folder",
        "chmod" | "chown" => "Change file permissions",
        "tar" | "zip" | "unzip" | "7z" | "gzip" | "gunzip" | "xz" => "Pack or unpack files",
        "apt" | "apt-get" | "aptitude" | "dpkg" | "snap" | "flatpak" => {
            let words: Vec<&str> = lower.split_whitespace().collect();
            if words.iter().any(|w| ["install", "refresh", "upgrade"].contains(w)) {
                "Install or update software"
            } else if words.iter().any(|w| ["remove", "purge", "uninstall", "autoremove"].contains(w)) {
                "Remove software"
            } else {
                "Look up installed software"
            }
        }
        "xdg-open" => "Open a file or link",
        "python3" | "python" | "bash" | "sh" | "perl" | "node" => "Run a script",
        _ => "",
    };
    let line = if line.is_empty() {
        if has("sudo") || has("pkexec") {
            "Run a command with administrator rights"
        } else {
            "Run a command on your computer"
        }
    } else {
        line
    };
    if (has("sudo") || has("pkexec")) && !line.contains("administrator") {
        format!("{line} (needs your password)")
    } else {
        line.to_string()
    }
}

/// The program names in a command line: the first word of each pipeline
/// or list part, past sudo/env and VAR=value prefixes. `sudo` itself is
/// kept so callers can tell.
fn programs(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in cmd.split(['|', ';', '&', '(', ')', '\n']) {
        let mut words = part.split_whitespace().peekable();
        while let Some(w) = words.next() {
            let name = w.rsplit('/').next().unwrap_or(w);
            if w.contains('=') && !w.starts_with('-') {
                continue;
            }
            match name {
                "sudo" | "pkexec" => {
                    out.push(name.to_string());
                    // Skip sudo's own options.
                    while words.peek().is_some_and(|n| n.starts_with('-')) {
                        words.next();
                    }
                }
                "env" | "nice" | "nohup" | "time" | "timeout" | "stdbuf" | "command" | "exec" => {
                    while words.peek().is_some_and(|n| n.starts_with('-') || n.contains('=') || n.parse::<f64>().is_ok()) {
                        words.next();
                    }
                }
                _ => {
                    out.push(name.to_string());
                    break;
                }
            }
        }
    }
    // The real program first, sudo after, so `first` is what runs.
    let (mut sudo, mut rest): (Vec<_>, Vec<_>) = out.into_iter().partition(|p| p == "sudo" || p == "pkexec");
    rest.append(&mut sudo);
    rest
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn uses_the_models_description() {
        let a = json!({"command": "gdbus call --session --dest org.gnome.Mutter.DisplayConfig", "description": "read your current display setup."});
        assert_eq!(summary(&a), "Read your current display setup");
    }

    #[test]
    fn ignores_a_description_that_is_the_command() {
        let a = json!({"command": "df -h | head", "description": "df -h | head"});
        assert_eq!(summary(&a), "Check disk space");
        let a = json!({"command": "ls ~/Downloads", "description": "Run ls ~/Downloads"});
        assert_eq!(summary(&a), "Look through your files");
    }

    #[test]
    fn guesses_from_the_command() {
        assert_eq!(
            command("gdbus call --session --dest org.gnome.Mutter.DisplayConfig --object-path /org/gnome/Mutter/DisplayConfig --method org.gnome.Mutter.DisplayConfig.GetCurrentState | head -c 3000"),
            "Read your display setup"
        );
        assert_eq!(command("LANG=C free -h"), "Check memory use");
        assert_eq!(command("sudo apt install vlc"), "Install or update software (needs your password)");
        assert_eq!(command("snap list"), "Look up installed software");
        assert_eq!(command("systemctl --user status umoja-panel"), "Check system services");
        assert_eq!(command("gsettings set org.gnome.desktop.interface color-scheme prefer-dark"), "Change a desktop setting");
        assert_eq!(command("frobnicate --all"), "Run a command on your computer");
        assert_eq!(command("sudo frobnicate"), "Run a command with administrator rights");
    }
}

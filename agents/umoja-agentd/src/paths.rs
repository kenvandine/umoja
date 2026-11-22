//! Where everything lives, per user. Each account gets its own OpenShell
//! gateway (own port, own state) and its own LXD project, so several people
//! on one computer never share sandboxes or approvals.

use std::path::PathBuf;

/// The sandbox Umoja keeps the active agent in: its id ("hermes").
pub fn sandbox() -> String {
    crate::catalog::active().id.clone()
}

/// Inside the sandbox: the agent's home (OpenShell's working directory, the
/// one place the default policy lets the agent write), and the folder that
/// shows up on the host as ~/Agents/<Name>.
#[allow(dead_code)]
pub const SANDBOX_HOME: &str = "/sandbox";
pub const SANDBOX_WORKSPACE: &str = "/sandbox/workspace";
pub const SANDBOX_SHARED: &str = "/sandbox/shared";

pub fn uid() -> u32 {
    // SAFETY: getuid never fails.
    unsafe { libc::getuid() }
}

pub fn user_name() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| format!("uid{}", uid()))
}

/// Gateway port, then the model relay on the next one. 17700 + 2 per
/// account (wrapping after 100), clear of the OpenShell snap's 17670 and
/// openshell-desktop's 17671.
pub fn ports() -> (u16, u16) {
    if let Some(p) = std::env::var("UMOJA_AGENTS_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
    {
        return (p, p + 1);
    }
    let slot = (uid().saturating_sub(1000) % 100) as u16;
    let base = 17700 + slot * 2;
    (base, base + 1)
}

/// The LXD managed bridge the sandboxes' supervisors join.
pub fn lxd_network() -> String {
    std::env::var("UMOJA_AGENTS_NETWORK").unwrap_or_else(|_| "lxdbr0".into())
}

/// LXD project holding this account's sandboxes: "umoja-ken".
pub fn lxd_project() -> String {
    // tools/test/umoja-agents-test runs its own instance beside the user's.
    if let Ok(p) = std::env::var("UMOJA_AGENTS_PROJECT") {
        if !p.is_empty() {
            return p;
        }
    }
    let mut name = String::from("umoja-");
    for c in user_name().chars() {
        if c.is_ascii_alphanumeric() {
            name.push(c.to_ascii_lowercase());
        } else if !name.ends_with('-') {
            name.push('-');
        }
    }
    name.trim_end_matches('-').to_owned()
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/tmp".into())
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(fallback))
}

/// ~/.local/state/umoja/agents: gateway TLS + database, our state file.
pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("umoja/agents")
}

pub fn tls_dir() -> PathBuf {
    state_dir().join("tls")
}

pub fn state_file() -> PathBuf {
    state_dir().join("state.json")
}

pub fn staging_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache").join("umoja/agents/staging")
}

pub fn runtime_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("UMOJA_AGENTS_RUNTIME_DIR") {
        return d.into();
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("umoja-agents")
}

pub fn driver_socket() -> PathBuf {
    runtime_dir().join("lxd-driver.sock")
}

/// The agent's memory and conversations on the host (its catalog
/// state_dir): ~/.local/state/umoja/agents/hermes-home.
pub fn agent_home() -> PathBuf {
    state_dir().join(format!("{}-home", crate::catalog::active().id))
}

/// The agent's own folder on the host: ~/Agents/Hermes.
pub fn workspace() -> PathBuf {
    std::env::var_os("UMOJA_AGENTS_WORKSPACE")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("Agents").join(&crate::catalog::active().name))
}

/// The gateway and driver binaries, from the umoja-agents package.
pub fn libexec(name: &str) -> PathBuf {
    std::env::var_os("UMOJA_AGENTS_LIBEXEC")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/usr/libexec/umoja-agents".into())
        .join(name)
}

/// The active agent's sandbox image (UMOJA_AGENT_IMAGE overrides it, for
/// tests of an image not published yet).
pub fn image() -> String {
    std::env::var("UMOJA_AGENT_IMAGE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| crate::catalog::active().image.clone())
}

/// The embedded Lemonade's port, as umoja-lemonade.service sets it.
pub fn lemonade_port() -> u16 {
    // Tests point Hermes at a scripted model instead.
    if let Some(p) = std::env::var("UMOJA_AGENTS_MODEL_PORT").ok().and_then(|v| v.parse().ok()) {
        return p;
    }
    let env = xdg("XDG_CONFIG_HOME", ".config").join("umoja/lemonade.env");
    std::fs::read_to_string(env)
        .ok()
        .and_then(|s| {
            s.lines()
                .filter_map(|l| l.trim().strip_prefix("UMOJA_LEMONADE_PORT="))
                .last()
                .and_then(|v| v.trim().trim_matches('"').parse().ok())
        })
        .unwrap_or(13345)
}

/// Total RAM in MiB, for the sandbox's memory cap.
pub fn total_ram_mb() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|kb| kb.parse::<u64>().ok())
        })
        .map(|kb| kb / 1024)
        .unwrap_or(8192)
}

/// 2 GiB on machines up to 16 GiB, 4 GiB above: Hermes is Python plus
/// whatever it runs; the model itself stays outside on the host.
pub fn memory_cap_mb() -> u32 {
    if let Some(v) = std::env::var("UMOJA_AGENTS_MEMORY_MB").ok().and_then(|v| v.parse().ok()) {
        return v;
    }
    if total_ram_mb() > 16 * 1024 { 4096 } else { 2048 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_names_are_lxd_safe() {
        // SAFETY: tests run single-threaded for env in this module only.
        unsafe { std::env::set_var("USER", "Ken.Van_Dine") };
        assert_eq!(lxd_project(), "umoja-ken-van-dine");
    }
}

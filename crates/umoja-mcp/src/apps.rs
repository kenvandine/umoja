//! Apps: find what's installed and how (snap, deb, Flatpak, AppImage or a
//! hand-made launcher), open apps, and remove them the right way.
//!
//! "Remove Discord" shouldn't send the user to a terminal to find out how
//! Discord got there. [`lookup`] works it out from the app's launcher (a
//! snap's lives in /var/lib/snapd/desktop/applications, a Flatpak's under
//! flatpak/exports, a deb's is owned by a package per `dpkg -S`) and from
//! `snap list`, `flatpak list` and dpkg for things without a launcher.
//! `remove_app` then picks the matching removal, shows what else would go
//! (apt's simulation) and asks the user before running it.
//!
//! Privileged removals run with `sudo -n` when sudo needs no password,
//! then through polkit (`snap remove` and `pkexec`), and fall back to a
//! terminal running `sudo …` when there is no polkit agent to ask.

use crate::tool::{LocalFuture, Outcome, Tool, arg_str, blocking, high, tilde};
use gio::prelude::*;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Snap { name: String },
    Deb { package: String },
    Flatpak { id: String, user: bool },
    AppImage { path: PathBuf },
    /// A launcher someone made by hand (~/.local/share/applications,
    /// /usr/local/share/applications) that no package owns.
    Manual { desktop: PathBuf },
}

impl Source {
    pub fn kind(&self) -> &'static str {
        match self {
            Source::Snap { .. } => "snap",
            Source::Deb { .. } => "deb",
            Source::Flatpak { .. } => "flatpak",
            Source::AppImage { .. } => "appimage",
            Source::Manual { .. } => "manual",
        }
    }

    fn key(&self) -> String {
        match self {
            Source::Snap { name } => format!("snap:{name}"),
            Source::Deb { package } => format!("deb:{package}"),
            Source::Flatpak { id, user } => format!("flatpak:{id}:{user}"),
            Source::AppImage { path } => format!("appimage:{}", path.display()),
            Source::Manual { desktop } => format!("manual:{}", desktop.display()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct App {
    pub name: String,
    pub desktop_id: Option<String>,
    pub desktop_path: Option<PathBuf>,
    pub source: Source,
    /// Version, channel, publisher… for the model.
    pub detail: String,
}

impl App {
    pub fn describe(&self) -> String {
        let how = match &self.source {
            Source::Snap { name } => format!("a snap (\"{name}\")"),
            Source::Deb { package } => format!("a deb package (\"{package}\", from apt)"),
            Source::Flatpak { id, user } => {
                format!("a Flatpak (\"{id}\", {} installation)", if *user { "user" } else { "system" })
            }
            Source::AppImage { path } => format!("an AppImage ({})", tilde(&path.to_string_lossy())),
            Source::Manual { desktop } => format!(
                "a hand-made launcher no package owns ({})",
                tilde(&desktop.to_string_lossy())
            ),
        };
        let mut s = format!("{}: installed as {how}", self.name);
        if !self.detail.is_empty() {
            s.push_str(&format!(", {}", self.detail));
        }
        if let Some(id) = &self.desktop_id {
            s.push_str(&format!("; launcher {id}"));
        }
        s
    }
}

// ---- matching ------------------------------------------------------------------

/// How well an app matches `q` (lower is better).
pub fn app_score(q: &str, name: &str, id: &str, extra: &str) -> Option<u8> {
    let q = q.trim().to_lowercase();
    let name = name.to_lowercase();
    let id = id.to_lowercase();
    if q.is_empty() {
        return None;
    }
    let bare_id = id.trim_end_matches(".desktop");
    if name == q || bare_id == q || bare_id.rsplit(['.', '_']).next() == Some(q.as_str()) {
        Some(0)
    } else if name.starts_with(&q) {
        Some(1)
    } else if name.contains(&q) || id.contains(&q) {
        Some(2)
    } else if q.split_whitespace().all(|w| extra.contains(w)) {
        Some(3)
    } else {
        None
    }
}

/// Words that don't help find an app ("the discord app").
fn clean_query(q: &str) -> String {
    let drop = ["the", "app", "application", "program", "snap", "flatpak", "deb", "package"];
    let words: Vec<&str> = q.split_whitespace().filter(|w| !drop.contains(&w.to_lowercase().as_str())).collect();
    if words.is_empty() { q.trim().to_string() } else { words.join(" ") }
}

/// The best-matching visible launcher for `query` (main thread).
pub fn find_launcher(query: &str) -> Option<gio::AppInfo> {
    let q = clean_query(query).to_lowercase();
    let mut best: Option<(u8, gio::AppInfo)> = None;
    for app in gio::AppInfo::all().into_iter().filter(|a| a.should_show()) {
        let id = app.id().map(|s| s.to_string()).unwrap_or_default();
        if let Some(s) = app_score(&q, &app.display_name(), &id, &extra_words(&app))
            && best.as_ref().is_none_or(|(b, _)| s < *b)
        {
            best = Some((s, app));
        }
    }
    best.map(|(_, a)| a)
}

fn extra_words(app: &gio::AppInfo) -> String {
    let mut extra = app.description().map(|d| d.to_lowercase()).unwrap_or_default();
    if let Some(d) = app.downcast_ref::<gio::DesktopAppInfo>() {
        for k in d.keywords() {
            extra.push(' ');
            extra.push_str(&k.to_lowercase());
        }
        if let Some(g) = d.generic_name() {
            extra.push(' ');
            extra.push_str(&g.to_lowercase());
        }
    }
    extra
}

// ---- package managers ----------------------------------------------------------

pub(crate) fn output(argv: &[&str]) -> Option<String> {
    let o = Command::new(argv[0]).args(&argv[1..]).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

pub(crate) fn have(bin: &str) -> bool {
    glib::find_program_in_path(bin).is_some()
}

#[derive(Debug, Clone, PartialEq)]
pub struct SnapRow {
    pub name: String,
    pub version: String,
    pub tracking: String,
    pub publisher: String,
    pub notes: String,
}

/// Parse `snap list`.
pub fn parse_snap_list(text: &str) -> Vec<SnapRow> {
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() >= 5).then(|| SnapRow {
                name: f[0].into(),
                version: f[1].into(),
                tracking: f[3].into(),
                publisher: f[4].trim_end_matches('✓').trim_end_matches('*').into(),
                notes: f.get(5).copied().unwrap_or("-").into(),
            })
        })
        .collect()
}

fn snaps() -> Vec<SnapRow> {
    if !have("snap") {
        return Vec::new();
    }
    output(&["snap", "list"]).map(|t| parse_snap_list(&t)).unwrap_or_default()
}

/// Snaps other snaps need: bases, runtimes and snapd itself.
pub fn is_support_snap(name: &str, notes: &str) -> bool {
    notes.contains("base")
        || notes.contains("core")
        || name == "snapd"
        || name == "bare"
        || name.starts_with("core")
        || name.starts_with("gnome-")
        || name.starts_with("kf5-")
        || name.starts_with("kf6-")
        || name.starts_with("mesa-")
        || name.starts_with("gaming-graphics")
        || name == "gtk-common-themes"
}

#[derive(Debug, Clone, PartialEq)]
pub struct FlatpakRow {
    pub id: String,
    pub name: String,
    pub version: String,
    pub user: bool,
}

/// Parse `flatpak list --app --columns=application,name,version,installation`.
pub fn parse_flatpak_list(text: &str) -> Vec<FlatpakRow> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            (f.len() >= 4).then(|| FlatpakRow {
                id: f[0].trim().into(),
                name: f[1].trim().into(),
                version: f[2].trim().into(),
                user: f[3].trim() == "user",
            })
        })
        .collect()
}

fn flatpaks() -> Vec<FlatpakRow> {
    if !have("flatpak") {
        return Vec::new();
    }
    output(&["flatpak", "list", "--app", "--columns=application,name,version,installation"])
        .map(|t| parse_flatpak_list(&t))
        .unwrap_or_default()
}

/// Parse `dpkg -S` output into path → first owning package.
pub fn parse_dpkg_s(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let (pkgs, path) = l.rsplit_once(": ")?;
            if pkgs.starts_with("diversion ") {
                return None;
            }
            let pkg = pkgs.split(", ").next()?.split(':').next()?.trim();
            Some((path.trim().to_string(), pkg.to_string()))
        })
        .collect()
}

fn dpkg_owners(paths: &[PathBuf]) -> Vec<(String, String)> {
    if paths.is_empty() || !have("dpkg") {
        return Vec::new();
    }
    let o = Command::new("dpkg").arg("-S").args(paths).output();
    // Exit status is 1 when some paths aren't owned; the rest still print.
    o.map(|o| parse_dpkg_s(&String::from_utf8_lossy(&o.stdout))).unwrap_or_default()
}

fn deb_version(pkg: &str) -> Option<String> {
    let s = output(&["dpkg-query", "-W", "-f=${db:Status-Abbrev}\t${Version}", pkg])?;
    let (status, version) = s.split_once('\t')?;
    status.starts_with("ii").then(|| version.trim().to_string())
}

// ---- lookup --------------------------------------------------------------------

struct Launcher {
    name: String,
    id: String,
    path: PathBuf,
    exec: String,
    snap: Option<String>,
    extra: String,
}

fn launchers() -> Vec<Launcher> {
    gio::AppInfo::all()
        .into_iter()
        .filter(|a| a.should_show())
        .filter_map(|a| {
            let extra = extra_words(&a);
            let d = a.downcast::<gio::DesktopAppInfo>().ok()?;
            Some(Launcher {
                name: d.display_name().to_string(),
                id: d.id().map(|s| s.to_string()).unwrap_or_default(),
                path: d.filename()?,
                exec: d.commandline().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
                snap: d.string("X-SnapInstanceName").map(|s| s.to_string()),
                extra,
            })
        })
        .collect()
}

fn appimage_path(exec: &str) -> Option<PathBuf> {
    let first = shell_words(exec).into_iter().next()?;
    first.to_lowercase().ends_with(".appimage").then(|| PathBuf::from(first))
}

/// Split an Exec line on spaces, honouring double quotes.
fn shell_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => quoted = !quoted,
            ' ' if !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// How the app behind a launcher was installed, without asking dpkg.
fn quick_source(l: &Launcher) -> Option<Source> {
    let p = l.path.to_string_lossy();
    if p.starts_with("/var/lib/snapd/desktop/applications/") {
        let name = l.snap.clone().unwrap_or_else(|| {
            l.id.trim_end_matches(".desktop").split('_').next().unwrap_or("").to_string()
        });
        return Some(Source::Snap { name });
    }
    if p.contains("/flatpak/exports/share/applications/") {
        let user = l.path.starts_with(glib::home_dir());
        return Some(Source::Flatpak { id: l.id.trim_end_matches(".desktop").to_string(), user });
    }
    if let Some(path) = appimage_path(&l.exec) {
        return Some(Source::AppImage { path });
    }
    None
}

/// Every installed thing matching `query`, best first. Blocking.
pub fn lookup(query: &str) -> Vec<App> {
    let q = clean_query(query).to_lowercase();
    let mut scored: Vec<(u8, Launcher)> = launchers()
        .into_iter()
        .filter_map(|l| app_score(&q, &l.name, &l.id, &l.extra).map(|s| (s, l)))
        .collect();
    scored.sort_by_key(|(s, _)| *s);
    // Weak keyword matches only count when nothing matched by name.
    if scored.first().is_some_and(|(s, _)| *s < 3) {
        scored.retain(|(s, _)| *s < 3);
    }
    scored.truncate(6);

    let unresolved: Vec<PathBuf> = scored
        .iter()
        .filter(|(_, l)| quick_source(l).is_none())
        .map(|(_, l)| l.path.clone())
        .collect();
    let owners = dpkg_owners(&unresolved);
    let snap_rows = snaps();
    let flatpak_rows = flatpaks();

    let mut apps: Vec<App> = Vec::new();
    for (_, l) in scored {
        let source = quick_source(&l).or_else(|| {
            let path = l.path.to_string_lossy();
            owners
                .iter()
                .find(|(p, _)| *p == path)
                .map(|(_, pkg)| Source::Deb { package: pkg.clone() })
        });
        let source = source.unwrap_or(Source::Manual { desktop: l.path.clone() });
        apps.push(App {
            name: l.name.clone(),
            desktop_id: Some(l.id.clone()),
            desktop_path: Some(l.path.clone()),
            source,
            detail: String::new(),
        });
    }
    // Things without a launcher (command-line snaps and packages).
    for s in &snap_rows {
        if s.name.to_lowercase() == q && !apps.iter().any(|a| a.source == Source::Snap { name: s.name.clone() }) {
            apps.push(App {
                name: s.name.clone(),
                desktop_id: None,
                desktop_path: None,
                source: Source::Snap { name: s.name.clone() },
                detail: String::new(),
            });
        }
    }
    for f in &flatpak_rows {
        let hit = f.name.to_lowercase() == q || f.id.to_lowercase().rsplit('.').next() == Some(q.as_str());
        if hit && !apps.iter().any(|a| matches!(&a.source, Source::Flatpak { id, .. } if *id == f.id)) {
            apps.push(App {
                name: f.name.clone(),
                desktop_id: None,
                desktop_path: None,
                source: Source::Flatpak { id: f.id.clone(), user: f.user },
                detail: String::new(),
            });
        }
    }
    let pkg = q.replace(' ', "-");
    if !apps.iter().any(|a| matches!(&a.source, Source::Deb { package } if *package == pkg))
        && deb_version(&pkg).is_some()
    {
        apps.push(App {
            name: pkg.clone(),
            desktop_id: None,
            desktop_path: None,
            source: Source::Deb { package: pkg },
            detail: String::new(),
        });
    }
    // Ubuntu's transitional debs (firefox 1:1snap1…) only install the snap.
    let snap_names: Vec<String> = apps
        .iter()
        .filter_map(|a| match &a.source {
            Source::Snap { name } => Some(name.clone()),
            _ => None,
        })
        .collect();
    apps.retain(|a| match &a.source {
        Source::Deb { package } => {
            !(a.desktop_id.is_none()
                && snap_names.contains(package)
                && deb_version(package).is_some_and(|v| v.contains("snap")))
        }
        _ => true,
    });
    // Details, and drop duplicates (two launchers from one package).
    let mut seen = Vec::new();
    apps.retain(|a| {
        let k = a.source.key();
        let fresh = !seen.contains(&k);
        seen.push(k);
        fresh
    });
    for a in &mut apps {
        a.detail = match &a.source {
            Source::Snap { name } => snap_rows
                .iter()
                .find(|s| s.name == *name)
                .map(|s| format!("version {}, channel {}, publisher {}", s.version, s.tracking, s.publisher))
                .unwrap_or_default(),
            Source::Deb { package } => deb_version(package).map(|v| format!("version {v}")).unwrap_or_default(),
            Source::Flatpak { id, .. } => flatpak_rows
                .iter()
                .find(|f| f.id == *id)
                .map(|f| format!("version {}", f.version))
                .unwrap_or_default(),
            _ => String::new(),
        };
    }
    apps
}

/// Every app with a launcher and how it's installed. Blocking.
fn inventory() -> Vec<App> {
    let ls = launchers();
    let unresolved: Vec<PathBuf> = ls.iter().filter(|l| quick_source(l).is_none()).map(|l| l.path.clone()).collect();
    let owners = dpkg_owners(&unresolved);
    let mut apps: Vec<App> = ls
        .into_iter()
        .map(|l| {
            let path = l.path.to_string_lossy().into_owned();
            let source = quick_source(&l)
                .or_else(|| owners.iter().find(|(p, _)| *p == path).map(|(_, p)| Source::Deb { package: p.clone() }))
                .unwrap_or(Source::Manual { desktop: l.path.clone() });
            App { name: l.name, desktop_id: Some(l.id), desktop_path: Some(l.path), source, detail: String::new() }
        })
        .collect();
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps
}

// ---- removal -------------------------------------------------------------------

/// Packages that must never go as a side effect of removing an app.
const PROTECTED_DEBS: &[&str] = &[
    "umoja-desktop", "umoja-greeter", "apt", "dpkg", "sudo", "systemd", "snapd", "network-manager",
    "ubuntu-minimal", "ubuntu-standard", "libc6", "bash", "coreutils", "login", "polkitd",
    "gnome-control-center", "gsettings-desktop-schemas", "dbus", "lightdm", "gdm3", "mir-platform-graphics-gbm-kms",
];

/// Packages `apt-get -s remove` would remove (from its "Remv" lines).
pub fn parse_apt_simulation(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.strip_prefix("Remv "))
        .filter_map(|l| l.split_whitespace().next())
        .map(|p| p.split(':').next().unwrap_or(p).to_string())
        .collect()
}

pub struct Plan {
    pub app: App,
    /// What the confirmation adds under the summary.
    pub detail: String,
}

/// Pick what to remove for these arguments, or say why not. Blocking.
pub fn plan_removal(args: &Value) -> Result<Plan, String> {
    let Some(name) = arg_str(args, "name") else { return Err("name is required".into()) };
    let want = arg_str(args, "source").map(|s| s.to_lowercase());
    let mut apps = lookup(name);
    if let Some(w) = &want {
        let w = match w.as_str() {
            "apt" | "debian" | "dpkg" | "package" => "deb",
            other => other,
        };
        apps.retain(|a| a.source.kind() == w);
    }
    let Some(first) = apps.first().cloned() else {
        return Err(format!("no installed app matches \"{name}\"; check the name with find_app"));
    };
    // Same app installed more than one way: ask which.
    let same: Vec<&App> = apps
        .iter()
        .filter(|a| a.name.eq_ignore_ascii_case(&first.name) && a.source.kind() != first.source.kind())
        .collect();
    if !same.is_empty() && want.is_none() {
        let kinds: Vec<&str> = std::iter::once(&first).chain(same).map(|a| a.source.kind()).collect();
        return Err(format!(
            "{} is installed more than one way ({}). Ask the user which one to remove, then call remove_app again with source set.",
            first.name,
            kinds.join(" and ")
        ));
    }
    let app = first;
    let detail = match &app.source {
        Source::Snap { name } => {
            let row = snaps().into_iter().find(|s| s.name == *name);
            if let Some(r) = &row
                && is_support_snap(&r.name, &r.notes)
            {
                return Err(format!("{name} is a base or runtime other snaps need; it can't be removed on its own"));
            }
            format!(
                "Runs: snap remove {name}\nsnapd keeps a snapshot of its data for a while, so it can be restored."
            )
        }
        Source::Deb { package } => {
            if PROTECTED_DEBS.contains(&package.as_str()) {
                return Err(format!("{package} is part of the system and can't be removed by the assistant"));
            }
            let sim = Command::new("apt-get")
                .args(["-s", "remove", package])
                .env("LC_ALL", "C")
                .output()
                .map_err(|e| format!("apt-get isn't available: {e}"))?;
            let text = String::from_utf8_lossy(&sim.stdout);
            let removed = parse_apt_simulation(&text);
            if let Some(p) = removed.iter().find(|p| PROTECTED_DEBS.contains(&p.as_str()) || p.starts_with("linux-image")) {
                return Err(format!(
                    "removing {package} would also remove {p}, which the system needs; not doing that"
                ));
            }
            let others: Vec<&String> = removed.iter().filter(|p| *p != package).collect();
            let mut d = format!("Runs: apt-get remove {package}");
            if !others.is_empty() {
                d.push_str(&format!(
                    "\nThis also removes {} other package(s): {}",
                    others.len(),
                    others.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                ));
            }
            d
        }
        Source::Flatpak { id, user } => format!(
            "Runs: flatpak uninstall --{} {id}",
            if *user { "user" } else { "system" }
        ),
        Source::AppImage { path } => format!(
            "Moves {} and its launcher to the Trash",
            tilde(&path.to_string_lossy())
        ),
        Source::Manual { desktop } => format!(
            "No package owns this app. Moves its launcher ({}) to the Trash; any program files it points to stay where they are.",
            tilde(&desktop.to_string_lossy())
        ),
    };
    if app.name.to_lowercase().contains("umoja") {
        return Err("Umoja itself can't be removed by the assistant".into());
    }
    Ok(Plan { app, detail })
}

fn still_installed(s: &Source) -> bool {
    match s {
        Source::Snap { name } => Command::new("snap").args(["list", name]).output().is_ok_and(|o| o.status.success()),
        Source::Deb { package } => deb_version(package).is_some(),
        Source::Flatpak { id, user } => Command::new("flatpak")
            .args(["info", if *user { "--user" } else { "--system" }, id])
            .output()
            .is_ok_and(|o| o.status.success()),
        Source::AppImage { path } => path.exists(),
        Source::Manual { desktop } => desktop.exists(),
    }
}

/// How a privileged command was run.
#[derive(Debug, PartialEq)]
pub enum Privileged {
    Ran { ok: bool, output: String },
    Dismissed,
    /// No way to ask for a password here: run it in a terminal.
    NeedsTerminal,
}

pub(crate) fn run(argv: &[&str]) -> (bool, Option<i32>, String) {
    match Command::new(argv[0]).args(&argv[1..]).env("LC_ALL", "C").output() {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.success(), o.status.code(), text.trim().to_string())
        }
        Err(e) => (false, None, e.to_string()),
    }
}

/// Run `argv` as root: `sudo -n` if that needs no password, else pkexec
/// (polkit asks), else report that a terminal is needed.
pub(crate) fn privileged(argv: &[&str]) -> Privileged {
    if have("sudo") && run(&["sudo", "-n", "true"]).0 {
        let mut a = vec!["sudo", "-n"];
        a.extend_from_slice(argv);
        let (ok, _, output) = run(&a);
        return Privileged::Ran { ok, output };
    }
    if have("pkexec") {
        let mut a = vec!["pkexec"];
        a.extend_from_slice(argv);
        let (ok, code, output) = run(&a);
        return match code {
            // 126: the user dismissed the dialog; 127: no agent / not authorized.
            Some(126) => Privileged::Dismissed,
            Some(127) => Privileged::NeedsTerminal,
            _ => Privileged::Ran { ok, output },
        };
    }
    Privileged::NeedsTerminal
}

/// snapd/flatpak errors that mean "polkit couldn't ask".
pub(crate) fn needs_auth(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("access denied") || t.contains("not authorized") || t.contains("authentication") || t.contains("polkit")
}

enum Removal {
    Done(String),
    Failed(String),
    Dismissed,
    Terminal(String),
}

pub(crate) fn last_lines(s: &str) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(4)..].join("\n")
}

fn remove_blocking(app: &App) -> Removal {
    let finish = |p: Privileged, terminal_cmd: String| match p {
        Privileged::Ran { ok: true, output } => Removal::Done(output),
        Privileged::Ran { ok: false, output } => Removal::Failed(last_lines(&output)),
        Privileged::Dismissed => Removal::Dismissed,
        Privileged::NeedsTerminal => Removal::Terminal(terminal_cmd),
    };
    match &app.source {
        Source::Snap { name } => {
            // snapd asks polkit itself; without an agent it says "access denied".
            let (ok, _, out) = run(&["snap", "remove", name]);
            if ok {
                return Removal::Done(out);
            }
            if !needs_auth(&out) {
                return Removal::Failed(last_lines(&out));
            }
            finish(privileged(&["snap", "remove", name]), format!("sudo snap remove {name}"))
        }
        Source::Deb { package } => finish(
            privileged(&["env", "DEBIAN_FRONTEND=noninteractive", "apt-get", "remove", "-y", package]),
            format!("sudo apt-get remove {package}"),
        ),
        Source::Flatpak { id, user } => {
            let scope = if *user { "--user" } else { "--system" };
            let (ok, _, out) = run(&["flatpak", "uninstall", scope, "-y", "--noninteractive", id]);
            if ok {
                return Removal::Done(out);
            }
            if *user || !needs_auth(&out) {
                return Removal::Failed(last_lines(&out));
            }
            finish(
                privileged(&["flatpak", "uninstall", "--system", "-y", "--noninteractive", id]),
                format!("sudo flatpak uninstall --system {id}"),
            )
        }
        Source::AppImage { path } => {
            let mut done = Vec::new();
            for p in std::iter::once(path.clone()).chain(app.desktop_path.clone()) {
                match trash(&p) {
                    Ok(()) => done.push(tilde(&p.to_string_lossy())),
                    Err(e) => return Removal::Failed(format!("couldn't move {} to the Trash: {e}", p.display())),
                }
            }
            Removal::Done(format!("Moved {} to the Trash", done.join(" and ")))
        }
        Source::Manual { desktop } => match trash(desktop) {
            Ok(()) => Removal::Done(format!("Moved {} to the Trash", tilde(&desktop.to_string_lossy()))),
            Err(e) => Removal::Failed(e),
        },
    }
}

fn trash(p: &Path) -> Result<(), String> {
    gio::File::for_path(p).trash(gio::Cancellable::NONE).map_err(|e| e.to_string())
}

// ---- tools ---------------------------------------------------------------------

fn name_schema(extra: Value) -> Value {
    let mut props = json!({"name": {"type": "string", "description": "App name, e.g. \"Discord\""}});
    if let (Some(p), Value::Object(e)) = (props.as_object_mut(), extra) {
        p.extend(e);
    }
    json!({"type": "object", "properties": props, "required": ["name"]})
}

pub fn tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "launch_app",
            "Open an installed application by name (e.g. \"Firefox\", \"Files\", \"Terminal\", \"text editor\"). Command-line apps such as btop open in the user's terminal.",
            json!({"type": "object", "properties": {
                "name": {"type": "string", "description": "App name, or what it's for"}
            }, "required": ["name"]}),
            |a| Box::pin(launch_app(a)),
        )
        .with_summary(|a| format!("Open {}", arg_str(a, "name").unwrap_or("app"))),
        Tool::read_only(
            "find_app",
            "Look up an installed app and how it was installed: snap, deb (apt), Flatpak, AppImage or a hand-made launcher, with its package name and version. Use this instead of asking the user to run snap, apt or flatpak commands.",
            name_schema(json!({})),
            |a| Box::pin(find_app(a)),
        )
        .with_summary(|a| format!("Look up {}", arg_str(a, "name").unwrap_or("the app"))),
        Tool::read_only(
            "list_apps",
            "List the installed apps that have a launcher and how each was installed. Optional source filter: snap, deb, flatpak, appimage or manual.",
            json!({"type": "object", "properties": {
                "source": {"type": "string", "enum": ["snap", "deb", "flatpak", "appimage", "manual"]}
            }}),
            |a| Box::pin(list_apps(a)),
        )
        .with_summary(|_| "List installed apps".into()),
        Tool::new(
            "remove_app",
            "Uninstall an app. Works out how it was installed (snap, deb, Flatpak, AppImage) and removes it that way; the user is shown exactly what will be removed and asked first. Just call it with the app's name, don't tell the user to run commands. Only set source when the app is installed more than one way.",
            name_schema(json!({"source": {"type": "string", "enum": ["snap", "deb", "flatpak", "appimage", "manual"], "description": "Only when asked to pick one"}})),
            |a| Box::pin(remove_app(a)),
        )
        .with_risk(high)
        .with_summary(|a| format!("Remove {}", arg_str(a, "name").unwrap_or("the app")))
        .with_preflight(remove_preflight),
    ]
}

async fn launch_app(a: Value) -> Outcome {
    let Some(name) = arg_str(&a, "name") else { return Outcome::err("name is required") };
    let Some(app) = find_launcher(name) else {
        if let Some((bin, terminal)) = snap_command(name) {
            return launch_command(&bin, terminal);
        }
        return Outcome::err(format!("no installed app matches \"{name}\""));
    };
    match app.launch(&[], Some(&umoja_common::app_scope::launch_context())) {
        Ok(()) => Outcome::ok(format!("Opened {}", app.display_name())).hide(),
        Err(e) => Outcome::err(e),
    }
}

/// A snap's command (btop, ...) that has no launcher snapd exports:
/// `/snap/bin/<name>`, and whether it runs in a terminal. Only snaps:
/// any other program in PATH could be `shutdown`.
fn snap_command(query: &str) -> Option<(PathBuf, bool)> {
    let name = clean_query(query).trim().to_lowercase().replace(' ', "-");
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        return None;
    }
    let bin = Path::new("/snap/bin").join(&name);
    if !bin.exists() {
        return None;
    }
    let snap = name.split('.').next().unwrap_or(&name);
    Some((bin, snap_wants_terminal(Path::new("/snap").join(snap).join("current").as_path())))
}

/// Whether the launcher a snap ships inside itself (not exported) says
/// Terminal=true. A command with no launcher at all is a command-line
/// program, so it runs in a terminal too.
fn snap_wants_terminal(root: &Path) -> bool {
    for dir in ["meta/gui", "usr/share/applications", "usr/local/share/applications", "share/applications"] {
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "desktop") {
                continue;
            }
            let kf = glib::KeyFile::new();
            if kf.load_from_file(&path, glib::KeyFileFlags::NONE).is_ok() {
                return kf.boolean("Desktop Entry", "Terminal").unwrap_or(false);
            }
        }
    }
    true
}

/// Start `bin`, in the user's default terminal when it's a command-line
/// program.
fn launch_command(bin: &Path, terminal: bool) -> Outcome {
    let name = bin.file_name().and_then(|n| n.to_str()).unwrap_or("it").to_string();
    if terminal {
        let argv = [bin.to_string_lossy().into_owned()];
        return match umoja_common::terminal::spawn_program_in_terminal(&argv, &glib::home_dir()) {
            Ok(()) => Outcome::ok(format!("Opened {name} in a terminal")).hide(),
            Err(e) => Outcome::err(format!("couldn't open a terminal for {name}: {e}")),
        };
    }
    match Command::new(bin).current_dir(glib::home_dir()).spawn() {
        Ok(child) => {
            umoja_common::app_scope::move_to_scope(&name, child.id());
            Outcome::ok(format!("Opened {name}")).hide()
        }
        Err(e) => Outcome::err(format!("couldn't start {name}: {e}")),
    }
}

async fn find_app(a: Value) -> Outcome {
    let Some(name) = arg_str(&a, "name").map(String::from) else { return Outcome::err("name is required") };
    let q = name.clone();
    let apps = match blocking(move || lookup(&q)).await {
        Ok(a) => a,
        Err(e) => return Outcome::err(e),
    };
    if apps.is_empty() {
        return Outcome::ok(format!("Nothing called \"{name}\" is installed (checked launchers, snaps, Flatpaks and debs)."));
    }
    let data = json!({"apps": apps.iter().map(|a| json!({
        "name": a.name, "source": a.source.kind(), "desktop_id": a.desktop_id, "detail": a.detail,
    })).collect::<Vec<_>>()});
    Outcome::ok(apps.iter().map(App::describe).collect::<Vec<_>>().join("\n")).with_data(data)
}

async fn list_apps(a: Value) -> Outcome {
    let want = arg_str(&a, "source").map(str::to_lowercase);
    let apps = match blocking(inventory).await {
        Ok(a) => a,
        Err(e) => return Outcome::err(e),
    };
    let lines: Vec<String> = apps
        .iter()
        .filter(|a| want.as_deref().is_none_or(|w| a.source.kind() == w))
        .map(|a| format!("{} ({})", a.name, a.source.kind()))
        .collect();
    if lines.is_empty() {
        return Outcome::ok("No matching apps are installed.");
    }
    Outcome::ok(format!("{} apps:\n{}", lines.len(), lines.join("\n")))
}

fn remove_preflight(a: Value) -> LocalFuture<Result<Option<String>, String>> {
    Box::pin(async move {
        let plan = blocking(move || plan_removal(&a)).await??;
        Ok(Some(format!("{}\n{}", plan.app.describe(), plan.detail)))
    })
}

async fn remove_app(a: Value) -> Outcome {
    let plan = match blocking(move || plan_removal(&a)).await {
        Ok(Ok(p)) => p,
        Ok(Err(e)) | Err(e) => return Outcome::err(e),
    };
    let app = plan.app.clone();
    let name = app.name.clone();
    let removal = match blocking(move || {
        let r = remove_blocking(&app);
        let gone = !still_installed(&app.source);
        (r, gone)
    })
    .await
    {
        Ok(r) => r,
        Err(e) => return Outcome::err(e),
    };
    match removal {
        (Removal::Done(_), true) => Outcome::ok(format!("Removed {name} ({}).", plan.app.source.kind())),
        (Removal::Done(out), false) => Outcome::err(format!("the removal ran but {name} is still installed: {}", last_lines(&out))),
        (Removal::Failed(out), _) => Outcome::err(format!("couldn't remove {name}: {out}")),
        (Removal::Dismissed, _) => Outcome::ok(format!("The password prompt was dismissed, so {name} is still installed.")),
        (Removal::Terminal(cmd), _) => match umoja_common::terminal::spawn_in_terminal(&cmd) {
            Ok(()) => Outcome::ok(format!(
                "Removing {name} needs the administrator password, so a terminal opened running \"{cmd}\". Tell the user to type their password there to finish."
            ))
            .hide(),
            Err(e) => Outcome::err(format!("removing {name} needs the administrator password and no terminal could be opened: {e}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching() {
        assert_eq!(app_score("firefox", "Firefox", "firefox_firefox.desktop", ""), Some(0));
        assert_eq!(app_score("discord", "Discord", "discord_discord.desktop", ""), Some(0));
        assert_eq!(app_score("files", "Files", "org.gnome.Nautilus.desktop", ""), Some(0));
        assert_eq!(app_score("gimp", "GNU Image Manipulation Program", "org.gimp.GIMP.desktop", ""), Some(0));
        assert_eq!(app_score("term", "Terminal", "x.desktop", ""), Some(1));
        assert_eq!(app_score("browser", "Firefox", "firefox.desktop", "web browser internet"), Some(3));
        assert_eq!(app_score("gimp", "Firefox", "firefox.desktop", "web"), None);
        assert_eq!(clean_query("the Discord app"), "Discord");
    }

    #[test]
    fn snap_list() {
        let t = "Name      Version   Rev    Tracking         Publisher     Notes\n\
                 core24    20240920  609    latest/stable    canonical✓    base\n\
                 discord   1.0.168   320    latest/stable    snapcrafters* -\n";
        let rows = parse_snap_list(t);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].name, "discord");
        assert_eq!(rows[1].publisher, "snapcrafters");
        assert!(is_support_snap(&rows[0].name, &rows[0].notes));
        assert!(!is_support_snap("discord", "-"));
        assert!(is_support_snap("gnome-46-2404", "-"));
    }

    #[test]
    fn dpkg_and_apt_output() {
        let s = "discord: /usr/share/applications/discord.desktop\nlibreoffice-calc, libreoffice-core:amd64: /usr/share/applications/x.desktop\n";
        let o = parse_dpkg_s(s);
        assert_eq!(o[0], ("/usr/share/applications/discord.desktop".into(), "discord".into()));
        assert_eq!(o[1].1, "libreoffice-calc");
        let sim = "NOTE: This is only a simulation!\nRemv discord [1.0.159]\nRemv libfoo:amd64 [2]\n";
        assert_eq!(parse_apt_simulation(sim), vec!["discord", "libfoo"]);
    }

    #[test]
    fn flatpak_list() {
        let t = "org.gimp.GIMP\tGNU Image Manipulation Program\t2.10.38\tsystem\ncom.discordapp.Discord\tDiscord\t0.0.70\tuser\n";
        let rows = parse_flatpak_list(t);
        assert_eq!(rows.len(), 2);
        assert!(rows[1].user);
    }

    #[test]
    fn appimages_are_recognised_from_exec() {
        assert_eq!(
            appimage_path("\"/home/k/Apps/Some App.AppImage\" --no-sandbox %U"),
            Some(PathBuf::from("/home/k/Apps/Some App.AppImage"))
        );
        assert_eq!(appimage_path("/usr/bin/discord"), None);
    }
}

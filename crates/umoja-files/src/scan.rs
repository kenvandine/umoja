//! Walking the home folder.
//!
//! The rules match what people expect from GNOME's file search:
//!
//! * hidden files and folders (dot names) are never indexed;
//! * `.gitignore` and `.ignore` files are honoured;
//! * a folder holding `.nomedia`, `.trackerignore` or `CACHEDIR.TAG` (cargo's
//!   `target/`, many caches) is skipped with everything below it;
//! * `~/snap` (per-snap app data) and package-manager folders such as
//!   `node_modules` are skipped;
//! * a git working tree becomes one entry for the repository itself, with
//!   its README as the description, instead of thousands of source files.
//!   Source code is better searched with the tools made for it.

use ignore::WalkBuilder;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

/// Hard cap so a pathological home folder can't exhaust memory.
pub const MAX_FILES: usize = 100_000;

const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "__pycache__",
    "site-packages",
    "venv",
    "bower_components",
    "lost+found",
];

const MARKERS: &[&str] = &[".nomedia", ".trackerignore", "CACHEDIR.TAG"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    File,
    /// A git working tree, indexed as a single entry.
    Repo,
}

#[derive(Debug, Clone)]
pub struct Found {
    pub path: PathBuf,
    pub mtime: i64,
    pub size: u64,
    pub kind: Kind,
}

/// List the files under `root` that the index should hold.
pub fn walk(root: &Path) -> Vec<Found> {
    let mut out = Vec::new();
    let snap = root.join("snap");
    let root_dir = root.to_path_buf();
    let repos: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
    let repos_seen = repos.clone();
    let mut walker = WalkBuilder::new(root);
    walker
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(false)
        // .gitignore files outside a repository still say "don't look here".
        .require_git(false)
        .follow_links(false)
        .same_file_system(true)
        .filter_entry(move |e| {
            let p = e.path();
            if !e.file_type().is_some_and(|t| t.is_dir()) {
                return true;
            }
            if p == snap {
                return false;
            }
            if p != root_dir && p.join(".git").exists() {
                repos_seen.lock().unwrap().push(p.to_path_buf());
                return false;
            }
            let name = e.file_name().to_string_lossy();
            if SKIP_DIRS.contains(&name.as_ref()) {
                return false;
            }
            // Python virtualenvs.
            if p.join("pyvenv.cfg").exists() {
                return false;
            }
            !MARKERS.iter().any(|m| p.join(m).exists())
        });

    for entry in walker.build() {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if let Some(f) = found(entry.path(), Kind::File) {
            out.push(f);
        }
        if out.len() >= MAX_FILES {
            tracing::warn!(max = MAX_FILES, "file index is full; skipping the rest");
            break;
        }
    }
    let repos = std::mem::take(&mut *repos.lock().unwrap());
    out.extend(repos.iter().filter_map(|r| found(r, Kind::Repo)));
    out
}

fn found(path: &Path, kind: Kind) -> Option<Found> {
    let md = std::fs::metadata(path).ok()?;
    let mtime = md
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Some(Found {
        path: path.to_path_buf(),
        mtime,
        size: if kind == Kind::File { md.len() } else { 0 },
        kind,
    })
}

#[cfg(test)]
pub(crate) fn tmp_home(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("umoja-files-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn respects_hidden_ignored_and_repos() {
        let h = tmp_home("walk");
        let w = |p: &str| {
            let p = h.join(p);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, "x").unwrap();
        };
        w("Documents/lease.pdf");
        w("Documents/notes.md");
        w(".config/secret.txt");
        w("Documents/.hidden.txt");
        w("snap/firefox/common/cache.db");
        w("Music/node_modules/x.js");
        w("Downloads/cache/CACHEDIR.TAG");
        w("Downloads/cache/blob.bin");
        w("Pictures/private/.nomedia");
        w("Pictures/private/me.jpg");
        w("Pictures/.gitignore");
        fs::write(h.join("Pictures/.gitignore"), "*.raw\n").unwrap();
        w("Pictures/a.raw");
        w("Pictures/a.jpg");
        w("src/proj/.git/HEAD");
        w("src/proj/README.md");
        w("src/proj/main.rs");

        let mut got: Vec<_> = walk(&h)
            .into_iter()
            .map(|f| {
                let rel = f
                    .path
                    .strip_prefix(&h)
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                (rel, f.kind)
            })
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("Documents/lease.pdf".into(), Kind::File),
                ("Documents/notes.md".into(), Kind::File),
                ("Pictures/a.jpg".into(), Kind::File),
                ("src/proj".into(), Kind::Repo),
            ]
        );
    }
}

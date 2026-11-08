//! umoja-files: find files in the home folder by what they're about.
//!
//! A user service that keeps a small local index of `~` (see [`scan`] for
//! what is left out) and answers `org.umoja.Files.Search` for the overlay
//! and the assistant. Text is embedded by the embedded Lemonade server, so
//! nothing leaves the machine. The service runs at idle CPU and I/O
//! priority, embeds only on mains power, and checks for changes every
//! 20 minutes on AC and hourly on battery.
//!
//! `umoja-files --search QUERY` searches the saved index from a terminal,
//! and `umoja-files --index` runs one indexing pass in the foreground.

mod embed;
mod extract;
mod index;
mod power;
mod scan;

use embed::{EmbedError, Embedder};
use index::{Entry, Index, Vector};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use umoja_common::{dbus, gsettings as gs};

/// Files read and embedded per batch.
const BATCH: usize = 16;
/// Pause between batches, so indexing never hogs a core or the GPU.
const BATCH_PAUSE: Duration = Duration::from_millis(250);
const SNIPPET_CHARS: usize = 1000;
/// Small enough to stay under 512 tokens (the default llama.cpp batch)
/// for most text; denser chunks are halved on retry.
const CHUNK_CHARS: usize = 800;
/// Don't retry a failed model download more often than this.
const PULL_RETRY: Duration = Duration::from_secs(6 * 3600);

#[derive(Debug, Clone, Default)]
struct Status {
    state: String,
    semantic: bool,
}

struct Shared {
    index: RwLock<Index>,
    status: Mutex<Status>,
    embedder: Embedder,
    wake: Notify,
}

impl Shared {
    fn set_state(&self, state: &str) {
        let mut s = self.status.lock().unwrap();
        if s.state != state {
            tracing::info!(state, "file index");
            s.state = state.into();
        }
    }

    fn semantic(&self) -> bool {
        self.status.lock().unwrap().semantic
    }

    async fn search(&self, query: &str, limit: usize) -> Vec<index::Hit> {
        let qvec = if self.semantic() {
            match self.embedder.query(query).await {
                Ok(mut v) => {
                    index::normalize(&mut v);
                    Some(v)
                }
                Err(e) => {
                    tracing::debug!(error = %e, "query embedding failed; matching words only");
                    None
                }
            }
        } else {
            None
        };
        let idx = self.index.read().unwrap();
        idx.search(query, qvec.as_deref(), limit.clamp(1, 50))
    }

    fn save(&self) {
        let idx = self.index.read().unwrap();
        if let Err(e) = idx.save(&Index::path()) {
            tracing::warn!(error = %e, "couldn't save the file index");
        }
    }
}

struct Service(Arc<Shared>);

#[zbus::interface(name = "org.umoja.Files")]
impl Service {
    /// Up to `limit` files matching `query`, best first, as
    /// (path, name, detail, score). `detail` is the folder plus the start
    /// of the file's text.
    async fn search(&self, query: String, limit: u32) -> Vec<(String, String, String, f64)> {
        self.0
            .search(&query, limit as usize)
            .await
            .into_iter()
            .map(|h| (h.path, h.name, h.detail, h.score))
            .collect()
    }

    /// (state, files indexed, files embedded, searching by meaning).
    /// States: starting, scanning, indexing, idle, paused-on-battery.
    fn status(&self) -> (String, u32, u32, bool) {
        let s = self.0.status.lock().unwrap().clone();
        let idx = self.0.index.read().unwrap();
        (
            s.state,
            idx.entries.len() as u32,
            idx.embedded() as u32,
            s.semantic,
        )
    }

    /// Look for changes now instead of at the next scheduled pass.
    fn reindex(&self) {
        self.0.wake.notify_one();
    }
}

fn embedding_model() -> String {
    gs::get_string(
        gs::UMOJA_SHELL,
        "file-search-embedding-model",
        embed::DEFAULT_MODEL,
    )
}

fn enabled() -> bool {
    gs::get_bool(gs::UMOJA_SHELL, "file-search-enabled", true)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    umoja_common::init_logging("umoja-files");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let model = embedding_model();
    let shared = Arc::new(Shared {
        index: RwLock::new(Index::load(&Index::path(), &model)),
        status: Mutex::new(Status {
            state: "starting".into(),
            semantic: false,
        }),
        embedder: Embedder::new(model),
        wake: Notify::new(),
    });

    match args.first().map(String::as_str) {
        Some("--search") => {
            let q = args[1..].join(" ");
            let ok = shared.embedder.query("probe").await.is_ok();
            shared.status.lock().unwrap().semantic = ok;
            println!("# by meaning: {ok}");
            for h in shared.search(&q, 10).await {
                println!("{:.3}  {}\n       {}", h.score, h.path, h.detail);
            }
            return Ok(());
        }
        Some("--index") => {
            let mut pulled = None;
            cycle(&shared, None, &mut pulled).await;
            let s = shared.status.lock().unwrap().clone();
            let idx = shared.index.read().unwrap();
            println!(
                "{} files, {} embedded, by meaning: {}",
                idx.entries.len(),
                idx.embedded(),
                s.semantic
            );
            return Ok(());
        }
        Some(other) => anyhow::bail!("unknown argument {other}; try --search QUERY or --index"),
        None => {}
    }

    let conn = zbus::connection::Builder::session()?
        .name(dbus::FILES_BUS_NAME)?
        .serve_at(dbus::FILES_OBJECT_PATH, Service(shared.clone()))?
        .build()
        .await?;
    tracing::info!(
        files = shared.index.read().unwrap().entries.len(),
        "file search ready"
    );

    // Searches work from the saved index straight away; the first pass
    // waits until login has settled.
    let delay = std::env::var("UMOJA_FILES_START_DELAY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(90);
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
        _ = shared.wake.notified() => {}
    }
    let mut pulled: Option<Instant> = None;
    loop {
        if enabled() {
            cycle(&shared, Some(&conn), &mut pulled).await;
        } else {
            shared.set_state("disabled");
        }
        wait_for_next(&shared).await;
    }
}

/// Sleep until the next pass: 20 minutes on AC, an hour on battery, or
/// sooner when asked to, or when AC comes back with work still to do.
async fn wait_for_next(shared: &Shared) {
    let started_on_mains = power::current().on_mains();
    let period = if started_on_mains { 20 * 60 } else { 60 * 60 };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(period);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return,
            _ = shared.wake.notified() => return,
            _ = tokio::time::sleep(Duration::from_secs(120)) => {
                let paused = shared.status.lock().unwrap().state == "paused-on-battery";
                if paused && power::current().on_mains() {
                    return;
                }
            }
        }
    }
}

/// Probe the embedding model, downloading it when allowed.
async fn ensure_model(
    shared: &Shared,
    conn: Option<&zbus::Connection>,
    pulled: &mut Option<Instant>,
) -> bool {
    match shared.embedder.query("probe").await {
        Ok(_) => return true,
        Err(EmbedError::Other(e)) => {
            tracing::warn!(error = %e, "embedding probe failed");
            return false;
        }
        Err(EmbedError::Unavailable(why)) => tracing::info!(why, "no embedding model yet"),
    }
    if pulled.is_some_and(|t| t.elapsed() < PULL_RETRY) {
        return false;
    }
    let metered = match conn {
        Some(c) => power::network_metered(c).await,
        None => false,
    };
    if metered || !power::current().on_mains() {
        return false;
    }
    *pulled = Some(Instant::now());
    shared.set_state("downloading-model");
    tracing::info!(
        model = shared.embedder.model,
        "downloading the embedding model"
    );
    if let Err(e) = shared.embedder.pull().await {
        tracing::warn!(error = %e, "embedding model download failed");
        return false;
    }
    shared.embedder.query("probe").await.is_ok()
}

async fn cycle(
    shared: &Arc<Shared>,
    conn: Option<&zbus::Connection>,
    pulled: &mut Option<Instant>,
) {
    let mains = power::current().on_mains();
    shared.set_state("scanning");
    let home = index::home();
    let found = tokio::task::spawn_blocking(move || scan::walk(&home))
        .await
        .unwrap_or_default();
    merge(shared, found);

    if !mains {
        shared.set_state("paused-on-battery");
        shared.save();
        return;
    }
    let semantic = ensure_model(shared, conn, pulled).await;
    shared.status.lock().unwrap().semantic = semantic;
    shared.set_state("indexing");

    let mut done = 0usize;
    loop {
        let batch: Vec<Entry> = {
            let idx = shared.index.read().unwrap();
            idx.entries
                .iter()
                .filter(|e| !e.read || (semantic && e.vectors.is_empty()))
                .take(BATCH)
                .cloned()
                .collect()
        };
        if batch.is_empty() {
            break;
        }
        if !power::current().on_mains() {
            shared.set_state("paused-on-battery");
            shared.save();
            return;
        }
        let processed = process(shared, batch, semantic).await;
        let Some(processed) = processed else {
            // Embedding broke mid-pass (Lemonade restarted?): keep what we
            // have and carry on with words only next time.
            shared.status.lock().unwrap().semantic = false;
            break;
        };
        {
            let mut idx = shared.index.write().unwrap();
            let pos: HashMap<String, usize> = idx
                .entries
                .iter()
                .enumerate()
                .map(|(i, e)| (e.path.clone(), i))
                .collect();
            for e in processed {
                if let Some(&i) = pos.get(&e.path) {
                    // Skip it if the file changed again meanwhile.
                    if idx.entries[i].mtime == e.mtime {
                        idx.entries[i] = e;
                    }
                }
            }
        }
        done += 1;
        if done % 25 == 0 {
            shared.save();
        }
        tokio::time::sleep(BATCH_PAUSE).await;
    }
    shared.save();
    shared.set_state("idle");
}

/// Replace the file list with what's on disk, keeping unchanged entries.
fn merge(shared: &Shared, found: Vec<scan::Found>) {
    let mut idx = shared.index.write().unwrap();
    let mut old: HashMap<String, Entry> =
        idx.entries.drain(..).map(|e| (e.path.clone(), e)).collect();
    let before = old.len();
    let mut changed = 0;
    idx.entries = found
        .into_iter()
        .map(|f| {
            let path = f.path.to_string_lossy().into_owned();
            match old.remove(&path) {
                Some(e) if e.mtime == f.mtime && e.size == f.size => e,
                _ => {
                    changed += 1;
                    Entry {
                        path,
                        mtime: f.mtime,
                        size: f.size,
                        repo: f.kind == scan::Kind::Repo,
                        read: false,
                        snippet: String::new(),
                        vectors: Vec::new(),
                    }
                }
            }
        })
        .collect();
    tracing::info!(
        files = idx.entries.len(),
        before,
        changed,
        removed = old.len(),
        "scanned home folder"
    );
}

/// Extract and embed a batch. `None` when embedding failed.
async fn process(shared: &Shared, batch: Vec<Entry>, semantic: bool) -> Option<Vec<Entry>> {
    let (mut entries, chunks) = tokio::task::spawn_blocking(move || {
        let mut chunks: Vec<(usize, String)> = Vec::new();
        let entries: Vec<Entry> = batch
            .into_iter()
            .enumerate()
            .map(|(i, mut e)| {
                let found = scan::Found {
                    path: e.path.clone().into(),
                    mtime: e.mtime,
                    size: e.size,
                    kind: if e.repo {
                        scan::Kind::Repo
                    } else {
                        scan::Kind::File
                    },
                };
                let text = extract::text(&found).unwrap_or_default();
                e.snippet = text.chars().take(SNIPPET_CHARS).collect();
                e.read = true;
                for c in document_chunks(&e.path, e.repo, &text) {
                    chunks.push((i, c));
                }
                e
            })
            .collect();
        (entries, chunks)
    })
    .await
    .ok()?;
    if !semantic {
        return Some(entries);
    }
    let texts: Vec<String> = chunks.iter().map(|(_, t)| t.clone()).collect();
    let vectors: Vec<Option<Vec<f32>>> = match shared.embedder.documents(&texts).await {
        Ok(v) => v.into_iter().map(Some).collect(),
        Err(EmbedError::Unavailable(why)) => {
            tracing::warn!(why, "embedding failed");
            return None;
        }
        Err(e) => {
            // One bad chunk (too many tokens for the model's batch, say)
            // fails the whole request: retry one at a time so it can't
            // stop the rest of the batch.
            tracing::debug!(error = %e, "batch embedding failed; retrying one by one");
            let mut out = Vec::with_capacity(texts.len());
            for t in &texts {
                out.push(embed_one(&shared.embedder, t).await);
            }
            out
        }
    };
    for ((i, _), v) in chunks.iter().zip(vectors) {
        if let Some(q) = v.as_deref().and_then(Vector::quantize) {
            entries[*i].vectors.push(q);
        }
    }
    // A file left without vectors would be picked again forever.
    if entries.iter().any(|e| e.vectors.is_empty()) {
        tracing::warn!("embedding failed for some files");
        return None;
    }
    Some(entries)
}

/// Embed one chunk, halving it while the model says it's too long.
async fn embed_one(embedder: &Embedder, text: &str) -> Option<Vec<f32>> {
    let mut t: String = text.to_string();
    for _ in 0..4 {
        match embedder.documents(std::slice::from_ref(&t)).await {
            Ok(mut v) => return v.pop(),
            Err(e) => {
                let n = t.chars().count();
                if n < 64 {
                    tracing::debug!(error = %e, "chunk embedding failed");
                    return None;
                }
                t = t.chars().take(n / 2).collect();
            }
        }
    }
    None
}

/// What gets embedded for a file: its name and folder, then its text in
/// up to [`index::MAX_CHUNKS`] pieces.
fn document_chunks(path: &str, repo: bool, text: &str) -> Vec<String> {
    let p = Path::new(path);
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let home = index::home().to_string_lossy().into_owned();
    let folder = p
        .parent()
        .map(|f| f.to_string_lossy().replacen(&home, "~", 1))
        .unwrap_or_default();
    // "lease_2026-03.pdf" reads better to the model as "lease 2026 03 pdf".
    let words: String = name.replace(['_', '-', '.'], " ");
    let head = if repo {
        format!("{words}: a software project (git repository) in {folder}")
    } else {
        format!("{words} (in {folder})")
    };
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    for (n, piece) in chars
        .chunks(CHUNK_CHARS)
        .take(index::MAX_CHUNKS)
        .enumerate()
    {
        let piece: String = piece.iter().collect();
        out.push(if n == 0 {
            format!("{head}\n{piece}")
        } else {
            format!("{head}\n…{piece}")
        });
    }
    if out.is_empty() {
        out.push(head);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_carry_the_name() {
        let c = document_chunks("/nowhere/Docs/lease_2026.pdf", false, "");
        assert_eq!(c.len(), 1);
        assert!(c[0].starts_with("lease 2026 pdf (in "));
        let long = "x".repeat(CHUNK_CHARS * 5);
        assert_eq!(
            document_chunks("/a/b.txt", false, &long).len(),
            index::MAX_CHUNKS
        );
        assert!(document_chunks("/a/proj", true, "")[0].contains("git repository"));
    }
}

//! The on-disk index and ranking.
//!
//! Each entry keeps the file's path, mtime, a short snippet and up to
//! [`MAX_CHUNKS`] embedding vectors, quantized to `i8` (a 768-wide vector
//! costs 772 bytes). Ranking mixes three signals:
//!
//! * meaning: the best cosine similarity between the query and a chunk;
//! * words: query words found in the file name, folder or text;
//! * hints: "pdf", "photo", "last spring", "yesterday"... nudge files of
//!   that type or from that time up.
//!
//! Without an embedding model the index still answers from words and hints.

use crate::extract::extension;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

pub const MAX_CHUNKS: usize = 3;
const MAGIC: &[u8; 8] = b"UMJFIDX1";

#[derive(Debug, Clone, PartialEq)]
pub struct Vector {
    pub scale: f32,
    pub q: Vec<i8>,
}

impl Vector {
    /// Normalize and quantize.
    pub fn quantize(v: &[f32]) -> Option<Vector> {
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if !norm.is_finite() || norm == 0.0 {
            return None;
        }
        let max = v.iter().fold(0f32, |m, x| m.max(x.abs())) / norm;
        let scale = max / 127.0;
        let q = v
            .iter()
            .map(|x| ((x / norm) / scale).round().clamp(-127.0, 127.0) as i8)
            .collect();
        Some(Vector { scale, q })
    }

    /// Cosine with a normalized float query.
    pub fn cosine(&self, query: &[f32]) -> f32 {
        if query.len() != self.q.len() {
            return 0.0;
        }
        let dot: f32 = self.q.iter().zip(query).map(|(a, b)| *a as f32 * b).sum();
        dot * self.scale
    }
}

pub fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub path: String,
    pub mtime: i64,
    pub size: u64,
    pub repo: bool,
    /// Text was extracted (or there was none to extract).
    pub read: bool,
    /// The start of the file's text, for word matching and the result line.
    pub snippet: String,
    pub vectors: Vec<Vector>,
}

impl Entry {
    pub fn name(&self) -> &str {
        Path::new(&self.path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&self.path)
    }
}

#[derive(Debug, Default)]
pub struct Index {
    /// Embedding model the vectors came from; vectors from another model
    /// are dropped on load.
    pub model: String,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub path: String,
    pub name: String,
    pub detail: String,
    pub score: f64,
}

impl Index {
    pub fn path() -> PathBuf {
        let cache = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home().join(".cache"));
        cache.join("umoja").join("files").join("index.bin")
    }

    pub fn embedded(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| !e.vectors.is_empty())
            .count()
    }

    pub fn load(path: &Path, model: &str) -> Index {
        match read(path) {
            Ok(mut idx) => {
                if idx.model != model {
                    tracing::info!(
                        old = idx.model,
                        new = model,
                        "embedding model changed; re-embedding"
                    );
                    idx.entries.iter_mut().for_each(|e| e.vectors.clear());
                    idx.model = model.to_string();
                }
                idx
            }
            Err(e) => {
                if path.exists() {
                    tracing::warn!(error = %e, "file index unreadable; rebuilding");
                }
                Index {
                    model: model.into(),
                    entries: Vec::new(),
                }
            }
        }
    }

    /// Write atomically next to `path`.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = path.with_extension("tmp");
        {
            let mut w = BufWriter::new(std::fs::File::create(&tmp)?);
            w.write_all(MAGIC)?;
            put_str(&mut w, &self.model)?;
            w.write_all(&(self.entries.len() as u32).to_le_bytes())?;
            for e in &self.entries {
                put_str(&mut w, &e.path)?;
                w.write_all(&e.mtime.to_le_bytes())?;
                w.write_all(&e.size.to_le_bytes())?;
                w.write_all(&[e.repo as u8 | (e.read as u8) << 1])?;
                put_str(&mut w, &e.snippet)?;
                w.write_all(&[e.vectors.len() as u8])?;
                for v in &e.vectors {
                    w.write_all(&v.scale.to_le_bytes())?;
                    w.write_all(&(v.q.len() as u32).to_le_bytes())?;
                    let bytes: Vec<u8> = v.q.iter().map(|x| *x as u8).collect();
                    w.write_all(&bytes)?;
                }
            }
            w.flush()?;
        }
        std::fs::rename(tmp, path)
    }

    pub fn search(&self, query: &str, qvec: Option<&[f32]>, limit: usize) -> Vec<Hit> {
        let q = Query::parse(query, now());
        if q.words.is_empty() && qvec.is_none() && q.kinds.is_empty() {
            return Vec::new();
        }
        let home = home().to_string_lossy().into_owned();
        let mut hits: Vec<(f64, &Entry)> = self
            .entries
            .iter()
            .filter_map(|e| {
                let words = q.word_score(e);
                let meaning = match qvec {
                    Some(v) if !e.vectors.is_empty() => {
                        e.vectors
                            .iter()
                            .map(|c| c.cosine(v))
                            .fold(f32::MIN, f32::max) as f64
                    }
                    _ => 0.0,
                };
                let hint = q.hint_score(e);
                // Semantic similarity alone has a floor of noise (unrelated
                // text still scores ~0.3–0.4 with small models), so a file
                // needs either matching words or a clearly related meaning.
                let relevant =
                    words > 0.0 || meaning >= q.meaning_floor || (q.words.is_empty() && hint > 0.0);
                relevant.then(|| (meaning + 0.6 * words + hint, e))
            })
            .collect();
        hits.sort_by(|a, b| b.0.total_cmp(&a.0));
        // Drop the long tail of weak matches far behind the best one.
        if let Some(best) = hits.first().map(|h| h.0) {
            hits.retain(|h| h.0 >= best - 0.35);
        }
        hits.truncate(limit);
        hits.into_iter()
            .map(|(score, e)| {
                let folder = Path::new(&e.path)
                    .parent()
                    .map(|p| p.to_string_lossy().replacen(&home, "~", 1))
                    .unwrap_or_default();
                let detail = if e.snippet.is_empty() {
                    folder
                } else {
                    let short: String = e.snippet.chars().take(140).collect();
                    format!("{folder} — {short}")
                };
                Hit {
                    path: e.path.clone(),
                    name: e.name().to_string(),
                    detail,
                    score,
                }
            })
            .collect()
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/".into())
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A parsed query: content words plus type and time hints.
#[derive(Debug, Default)]
pub struct Query {
    pub words: Vec<String>,
    pub kinds: Vec<&'static [&'static str]>,
    pub time: Option<(i64, i64)>,
    pub meaning_floor: f64,
}

const STOP: &[&str] = &[
    "a", "an", "the", "my", "me", "i", "find", "search", "show", "open", "where", "is", "was",
    "are", "file", "files", "folder", "about", "from", "for", "of", "in", "on", "with", "that",
    "which", "some", "any", "all", "to", "it", "and", "or", "what", "did", "put", "get", "look",
    "looking", "last", "this", "ago", "called", "named", "one", "please", "can", "you", "could",
];

const DOCS: &[&str] = &["pdf", "odt", "docx", "doc", "rtf", "txt", "md", "pages"];
const PDF: &[&str] = &["pdf"];
const IMAGES: &[&str] = &[
    "jpg", "jpeg", "png", "heic", "webp", "gif", "tif", "tiff", "raw", "cr2", "nef", "dng", "svg",
];
const VIDEO: &[&str] = &["mp4", "mkv", "webm", "mov", "avi"];
const AUDIO: &[&str] = &["mp3", "flac", "ogg", "opus", "m4a", "wav"];
const SHEETS: &[&str] = &["ods", "xlsx", "xls", "csv"];
const SLIDES: &[&str] = &["odp", "pptx", "ppt", "key"];
const ARCHIVES: &[&str] = &["zip", "tar", "gz", "xz", "zst", "7z", "rar"];

fn kind_for(word: &str) -> Option<&'static [&'static str]> {
    Some(match word {
        "pdf" | "pdfs" => PDF,
        "document" | "documents" | "doc" | "docs" => DOCS,
        "photo" | "photos" | "picture" | "pictures" | "image" | "images" | "screenshot"
        | "screenshots" | "pic" | "pics" => IMAGES,
        "video" | "videos" | "movie" | "movies" | "recording" => VIDEO,
        "song" | "songs" | "music" | "audio" | "podcast" => AUDIO,
        "spreadsheet" | "spreadsheets" | "sheet" | "csv" => SHEETS,
        "presentation" | "presentations" | "slides" | "deck" => SLIDES,
        "archive" | "zip" | "tarball" => ARCHIVES,
        _ => return None,
    })
}

const DAY: i64 = 86_400;

impl Query {
    pub fn parse(query: &str, now: i64) -> Query {
        let lower = query.to_lowercase();
        let tokens: Vec<String> = lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(String::from)
            .collect();
        let mut q = Query {
            meaning_floor: 0.62,
            ..Default::default()
        };
        q.time = time_hint(&tokens, now);
        for t in &tokens {
            if let Some(k) = kind_for(t) {
                q.kinds.push(k);
                continue;
            }
            if STOP.contains(&t.as_str()) || is_time_word(t) || t.len() < 2 {
                continue;
            }
            q.words.push(t.clone());
        }
        q
    }

    /// Fraction of query words found, weighted by where they were found.
    fn word_score(&self, e: &Entry) -> f64 {
        if self.words.is_empty() {
            return 0.0;
        }
        let name = e.name().to_lowercase();
        let folder = e.path.to_lowercase();
        let text = e.snippet.to_lowercase();
        let mut s = 0.0;
        for w in &self.words {
            s += if name.contains(w.as_str()) {
                1.0
            } else if folder.contains(w.as_str()) {
                0.5
            } else if text.contains(w.as_str()) {
                0.4
            } else {
                0.0
            };
        }
        s / self.words.len() as f64
    }

    fn hint_score(&self, e: &Entry) -> f64 {
        let mut s = 0.0;
        if !self.kinds.is_empty() {
            let ext = extension(Path::new(&e.path));
            if self.kinds.iter().any(|k| k.contains(&ext.as_str())) {
                s += 0.15;
            } else {
                s -= 0.1;
            }
        }
        if let Some((from, to)) = self.time {
            if e.mtime >= from && e.mtime < to {
                s += 0.15;
            }
        }
        s
    }
}

fn is_time_word(t: &str) -> bool {
    matches!(
        t,
        "today"
            | "yesterday"
            | "week"
            | "month"
            | "year"
            | "spring"
            | "summer"
            | "autumn"
            | "fall"
            | "winter"
            | "recent"
            | "recently"
            | "days"
            | "weeks"
            | "months"
            | "years"
    ) || t.parse::<u32>().is_ok_and(|y| (1990..2100).contains(&y))
        || month_number(t).is_some()
}

/// "june" or "jun" -> 6. "may" and "march" are also verbs, but in a file
/// search they are nearly always months.
fn month_number(t: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    MONTHS
        .iter()
        .position(|m| t == *m || (t.len() == 3 && m.starts_with(t) && t != "mar" && t != "may"))
        .or_else(|| (t == "sept").then_some(8))
        .map(|i| i as u32 + 1)
}

/// Civil date from a Unix day number (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + (m <= 2) as i64, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn ts(y: i64, m: u32) -> i64 {
    let (y, m) = if m > 12 { (y + 1, m - 12) } else { (y, m) };
    days_from_civil(y, m, 1) * DAY
}

/// Time range named in the query, padded a little either side.
fn time_hint(tokens: &[String], now: i64) -> Option<(i64, i64)> {
    let today = now.div_euclid(DAY) * DAY;
    let (year, month, _) = civil(now.div_euclid(DAY));
    let has = |w: &str| tokens.iter().any(|t| t == w);
    let last = has("last") || has("ago");
    let explicit_year = tokens
        .iter()
        .find_map(|t| t.parse::<i64>().ok().filter(|y| (1990..2100).contains(y)));
    if has("today") {
        return Some((today, now + DAY));
    }
    if has("yesterday") {
        return Some((today - DAY, today));
    }
    if has("week") {
        return Some(if last {
            (today - 14 * DAY, today - 5 * DAY)
        } else {
            (today - 7 * DAY, now + DAY)
        });
    }
    if has("recent") || has("recently") {
        return Some((today - 30 * DAY, now + DAY));
    }
    if has("month") {
        return Some(if last {
            (ts(year, month) - 40 * DAY, ts(year, month) + 5 * DAY)
        } else {
            (ts(year, month), now + DAY)
        });
    }
    // Northern-hemisphere meteorological seasons, by starting month.
    let season = [
        ("spring", 3),
        ("summer", 6),
        ("autumn", 9),
        ("fall", 9),
        ("winter", 12),
    ]
    .into_iter()
    .find(|(w, _)| has(w))
    .map(|(_, m)| m);
    if let Some(start) = season {
        let mut y = explicit_year.unwrap_or(year);
        let begins = |y| ts(y, start);
        if explicit_year.is_none() && (last || begins(y) > now) {
            // "last spring": the most recent spring that has already ended.
            while begins(y) + 92 * DAY > now {
                y -= 1;
            }
        }
        return Some((begins(y) - 15 * DAY, begins(y) + 107 * DAY));
    }
    if let Some(m) = tokens.iter().find_map(|t| month_number(t)) {
        let mut y = explicit_year.unwrap_or(year);
        if explicit_year.is_none() && (ts(y, m) > now || (last && m == month)) {
            y -= 1;
        }
        return Some((ts(y, m) - 5 * DAY, ts(y, m + 1) + 5 * DAY));
    }
    if has("year") {
        return Some(if last {
            (ts(year - 1, 1), ts(year, 1))
        } else {
            (ts(year, 1), now + DAY)
        });
    }
    explicit_year.map(|y| (ts(y, 1), ts(y + 1, 1)))
}

fn put_str(w: &mut impl Write, s: &str) -> std::io::Result<()> {
    w.write_all(&(s.len() as u32).to_le_bytes())?;
    w.write_all(s.as_bytes())
}

fn get_u32(r: &mut impl Read) -> std::io::Result<u32> {
    let mut b = [0; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn get_u64(r: &mut impl Read) -> std::io::Result<u64> {
    let mut b = [0; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn get_u8(r: &mut impl Read) -> std::io::Result<u8> {
    let mut b = [0; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

fn get_str(r: &mut impl Read) -> std::io::Result<String> {
    let n = get_u32(r)? as usize;
    if n > 1 << 20 {
        return Err(std::io::Error::other("string too long"));
    }
    let mut b = vec![0; n];
    r.read_exact(&mut b)?;
    String::from_utf8(b).map_err(std::io::Error::other)
}

fn read(path: &Path) -> std::io::Result<Index> {
    let mut r = BufReader::new(std::fs::File::open(path)?);
    let mut magic = [0; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(std::io::Error::other("not a umoja file index"));
    }
    let model = get_str(&mut r)?;
    let n = get_u32(&mut r)? as usize;
    let mut entries = Vec::with_capacity(n.min(crate::scan::MAX_FILES));
    for _ in 0..n {
        let path = get_str(&mut r)?;
        let mtime = get_u64(&mut r)? as i64;
        let size = get_u64(&mut r)?;
        let flags = get_u8(&mut r)?;
        let snippet = get_str(&mut r)?;
        let nv = get_u8(&mut r)? as usize;
        let mut vectors = Vec::with_capacity(nv);
        for _ in 0..nv {
            let scale = f32::from_bits(get_u32(&mut r)?);
            let dims = get_u32(&mut r)? as usize;
            if dims > 16_384 {
                return Err(std::io::Error::other("vector too wide"));
            }
            let mut b = vec![0u8; dims];
            r.read_exact(&mut b)?;
            vectors.push(Vector {
                scale,
                q: b.into_iter().map(|x| x as i8).collect(),
            });
        }
        entries.push(Entry {
            path,
            mtime,
            size,
            repo: flags & 1 != 0,
            read: flags & 2 != 0,
            snippet,
            vectors,
        });
    }
    Ok(Index { model, entries })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, mtime: i64, snippet: &str, v: Option<&[f32]>) -> Entry {
        Entry {
            path: path.into(),
            mtime,
            size: 1,
            repo: false,
            read: true,
            snippet: snippet.into(),
            vectors: v.and_then(Vector::quantize).into_iter().collect(),
        }
    }

    #[test]
    fn quantized_cosine_is_close() {
        let a = [0.3f32, -0.2, 0.9, 0.1];
        let mut q = a;
        normalize(&mut q);
        let v = Vector::quantize(&a).unwrap();
        assert!((v.cosine(&q) - 1.0).abs() < 0.02);
        let mut o = [-0.9f32, 0.1, 0.0, 0.3];
        normalize(&mut o);
        assert!(v.cosine(&o) < 0.0);
    }

    #[test]
    fn round_trip() {
        let p = crate::scan::tmp_home("index").join("i.bin");
        let idx = Index {
            model: "m".into(),
            entries: vec![entry("/h/a.pdf", 5, "lease", Some(&[1.0, 2.0, 3.0]))],
        };
        idx.save(&p).unwrap();
        let back = Index::load(&p, "m");
        assert_eq!(back.entries, idx.entries);
        // Another model drops the vectors but keeps the files.
        let other = Index::load(&p, "other");
        assert_eq!(other.entries.len(), 1);
        assert!(other.entries[0].vectors.is_empty());
    }

    #[test]
    fn words_and_hints_rank_without_embeddings() {
        let idx = Index {
            model: "m".into(),
            entries: vec![
                entry("/h/Documents/lease-2026.pdf", 0, "", None),
                entry("/h/Documents/lease-notes.txt", 0, "", None),
                entry("/h/Pictures/cat.jpg", 0, "", None),
            ],
        };
        let hits = idx.search("find the lease pdf", None, 5);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].name, "lease-2026.pdf");
        assert!(idx.search("the", None, 5).is_empty());
        let photos = idx.search("photos", None, 5);
        assert_eq!(photos[0].name, "cat.jpg");
    }

    #[test]
    fn meaning_beats_noise() {
        let idx = Index {
            model: "m".into(),
            entries: vec![
                entry("/h/a.pdf", 0, "rental agreement", Some(&[1.0, 0.1, 0.0])),
                entry("/h/b.pdf", 0, "recipe", Some(&[0.0, 0.2, 1.0])),
            ],
        };
        let mut q = [0.9f32, 0.2, 0.05];
        normalize(&mut q);
        let hits = idx.search("apartment contract", Some(&q), 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "a.pdf");
    }

    #[test]
    fn calendar_maths() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(days_from_civil(2026, 11, 8), 20_765);
        assert_eq!(civil(20_765), (2026, 11, 8));
    }

    #[test]
    fn time_hints() {
        let now = days_from_civil(2026, 11, 8) * DAY;
        let t = |q: &str| {
            Query::parse(q, now)
                .time
                .map(|(a, b)| (civil(a / DAY), civil(b / DAY)))
        };
        // Last spring = March–May 2026, padded.
        let (from, to) = t("the PDF about the lease from last spring").unwrap();
        assert_eq!((from.0, from.1), (2026, 2));
        assert_eq!((to.0, to.1), (2026, 6));
        // Winter hasn't started yet in November, so "last winter" is 2025/26.
        let (from, _) = t("photos from last winter").unwrap();
        assert_eq!((from.0, from.1), (2025, 11));
        assert_eq!(t("tax forms 2024").unwrap().0, (2024, 1, 1));
        assert_eq!(t("yesterday").unwrap().0, (2026, 11, 7));
        // A month that hasn't come yet this year means last year's.
        let (from, to) = t("photos from June").unwrap();
        assert_eq!((from.0, from.1, to.0, to.1), (2026, 5, 2026, 7));
        assert_eq!(t("pictures from december").unwrap().0.0, 2025);
        assert_eq!(t("receipts jan 2024").unwrap().0, (2023, 12, 27));
        assert!(Query::parse("photos from june", now).words.is_empty());
        assert!(t("lease").is_none());
        let q = Query::parse("the PDF about the lease from last spring", now);
        assert_eq!(q.words, vec!["lease"]);
        assert_eq!(q.kinds.len(), 1);
    }
}

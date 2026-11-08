//! Pulling searchable text out of files.
//!
//! Only the start of each file is read: enough to tell what it is about,
//! cheap enough to run over a whole home folder. PDFs go through
//! `pdftotext` (poppler-utils) and OpenDocument / Office files through
//! `unzip`; when those tools are missing the file is still found by name.

use crate::scan::{Found, Kind};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

/// Characters of text kept per file.
pub const MAX_TEXT: usize = 4000;
/// Bytes read from a plain-text file.
const READ_BYTES: u64 = 32 * 1024;
/// Larger documents are still found by name, just not opened.
const MAX_DOCUMENT_BYTES: u64 = 64 * 1024 * 1024;

const TEXT_EXT: &[&str] = &[
    "txt", "md", "markdown", "rst", "org", "tex", "csv", "tsv", "html", "htm", "xml", "json",
    "yaml", "yml", "toml", "ini", "conf", "log", "srt", "vtt", "eml", "ics", "vcf", "py", "sh",
    "rs", "c", "h", "cpp", "js", "ts", "go", "java", "rb", "pl", "lua", "sql", "desktop",
];

/// The text to index for `f`, if any can be read.
pub fn text(f: &Found) -> Option<String> {
    let raw = match f.kind {
        Kind::Repo => readme(&f.path)?,
        Kind::File => {
            let ext = extension(&f.path);
            if f.size > MAX_DOCUMENT_BYTES {
                return None;
            }
            match ext.as_str() {
                "pdf" => pdf(&f.path)?,
                "odt" | "ods" | "odp" | "odg" => zip_xml(&f.path, &["content.xml"])?,
                "docx" => zip_xml(&f.path, &["word/document.xml"])?,
                "pptx" => zip_xml(&f.path, &["ppt/slides/slide1.xml", "ppt/slides/slide2.xml"])?,
                "xlsx" => zip_xml(&f.path, &["xl/sharedStrings.xml"])?,
                e if TEXT_EXT.contains(&e) => plain(&f.path)?,
                "" if f.size <= READ_BYTES => plain(&f.path)?,
                _ => return None,
            }
        }
    };
    let t = clean(&raw);
    (!t.is_empty()).then_some(t)
}

pub fn extension(p: &Path) -> String {
    p.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

fn plain(p: &Path) -> Option<String> {
    let mut buf = Vec::new();
    std::fs::File::open(p)
        .ok()?
        .take(READ_BYTES)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.iter().take(1024).any(|b| *b == 0) {
        return None; // binary
    }
    let s = String::from_utf8_lossy(&buf).into_owned();
    let replaced = s.chars().filter(|c| *c == '\u{fffd}').count();
    (replaced * 20 < s.len().max(1)).then_some(s)
}

fn readme(dir: &Path) -> Option<String> {
    for name in [
        "README.md",
        "README",
        "README.rst",
        "README.txt",
        "readme.md",
    ] {
        if let Some(t) = plain(&dir.join(name)) {
            return Some(t);
        }
    }
    None
}

/// Run a helper under coreutils `timeout` (pdftotext can stall on broken
/// files), keeping at most `READ_BYTES` of its output.
fn helper(program: &str, args: &[&std::ffi::OsStr]) -> Option<String> {
    let mut child = Command::new("timeout")
        .args(["-s", "KILL", "5", program])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut out = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let _ = stdout.take(READ_BYTES).read_to_end(&mut out);
    } // dropping the pipe stops a helper that has more to say
    let _ = child.wait();
    (!out.is_empty()).then(|| String::from_utf8_lossy(&out).into_owned())
}

fn pdf(p: &Path) -> Option<String> {
    let a = |s: &'static str| std::ffi::OsStr::new(s);
    helper(
        "pdftotext",
        &[
            a("-q"),
            a("-l"),
            a("3"),
            a("-enc"),
            a("UTF-8"),
            p.as_os_str(),
            a("-"),
        ],
    )
}

fn zip_xml(p: &Path, members: &[&str]) -> Option<String> {
    let mut args = vec![std::ffi::OsStr::new("-p"), p.as_os_str()];
    args.extend(members.iter().map(std::ffi::OsStr::new));
    let xml = helper("unzip", &args)?;
    Some(strip_tags(&xml))
}

/// Drop XML tags, treating paragraph and cell ends as spaces.
pub fn strip_tags(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len() / 2);
    let mut in_tag = false;
    for c in xml.chars() {
        match c {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

/// Collapse whitespace and cut to [`MAX_TEXT`] characters.
pub fn clean(s: &str) -> String {
    let mut out = String::new();
    for w in s.split_whitespace() {
        if out.len() + w.len() + 1 > MAX_TEXT {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_office_xml() {
        let x = r#"<w:p><w:r><w:t>Lease</w:t></w:r></w:p><w:p><w:t>Tom &amp; Jerry</w:t></w:p>"#;
        assert_eq!(clean(&strip_tags(x)), "Lease Tom & Jerry");
    }

    #[test]
    fn reads_text_and_skips_binary() {
        let h = crate::scan::tmp_home("extract");
        let t = h.join("notes.md");
        std::fs::write(&t, "# Lease\n\n  rent   is due monthly\n").unwrap();
        let b = h.join("blob");
        std::fs::write(&b, [0u8, 1, 2, 3, 0, 9]).unwrap();
        let f = |p: &Path, size| Found {
            path: p.into(),
            mtime: 0,
            size,
            kind: Kind::File,
        };
        assert_eq!(
            text(&f(&t, 30)).as_deref(),
            Some("# Lease rent is due monthly")
        );
        assert_eq!(text(&f(&b, 6)), None);
    }

    #[test]
    fn clean_caps_length() {
        let long = "word ".repeat(5000);
        assert!(clean(&long).len() <= MAX_TEXT);
    }
}

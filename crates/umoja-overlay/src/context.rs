//! Context the user can attach to a question: the window they were in, an
//! area of the screen, the text they selected or something they copied.
//!
//! Window and area contents are read with Tesseract OCR from a `grim`
//! capture, since the default assistant model is text only. Nothing leaves
//! the machine: the text goes to the embedded Lemonade like any question.

use std::cell::RefCell;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::process::Command;
use std::rc::Rc;
use umoja_common::i18n;

/// Characters of attached text sent with one question. Qwen3-4B has a big
/// context window, but prompt processing on an iGPU is the slow part.
pub const MAX_CONTEXT_CHARS: usize = 8000;

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Selection,
    Clipboard,
    Window { app_id: String, title: String },
    Area,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Attachment {
    pub kind: Kind,
    pub text: String,
}

impl Attachment {
    pub fn new(kind: Kind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
        }
    }

    pub fn icon(&self) -> &'static str {
        match self.kind {
            Kind::Selection => "edit-select-all-symbolic",
            Kind::Clipboard => "edit-paste-symbolic",
            Kind::Window { .. } => "focus-windows-symbolic",
            Kind::Area => "screenshot-recorded-symbolic",
        }
    }

    /// Short label for the pill above the answer.
    pub fn label(&self) -> String {
        match &self.kind {
            Kind::Selection => i18n::umoja("Selected text"),
            Kind::Clipboard => i18n::umoja("Copied text"),
            Kind::Window { title, app_id } => {
                if title.is_empty() {
                    app_id.clone()
                } else {
                    title.clone()
                }
            }
            Kind::Area => i18n::umoja("Screen area"),
        }
    }

    /// Header for this attachment inside the prompt (not translated: it is
    /// for the model).
    fn heading(&self) -> String {
        match &self.kind {
            Kind::Selection => "Text the user selected".into(),
            Kind::Clipboard => "Text the user copied".into(),
            Kind::Window { app_id, title } => format!(
                "Text read (OCR) from the window the user is in, app \"{app_id}\", titled \"{title}\""
            ),
            Kind::Area => "Text read (OCR) from an area of the screen the user picked".into(),
        }
    }
}

/// What we ask when the user attaches something and just presses Enter.
pub const DEFAULT_QUESTION: &str = "Explain what this is and anything I should do about it.";

/// The context block that goes in front of the user's question, or `None`
/// with nothing attached.
pub fn build_context(attachments: &[Attachment]) -> Option<String> {
    if attachments.is_empty() {
        return None;
    }
    let mut budget = MAX_CONTEXT_CHARS;
    let mut out = String::from(
        "The user attached this context from their screen. Use it to answer; \
         OCR text may contain recognition errors.\n\n",
    );
    for a in attachments {
        let text = truncate_chars(a.text.trim(), budget);
        budget = budget.saturating_sub(text.chars().count());
        out.push_str(&format!(
            "<<< {} >>>\n{}\n<<< end >>>\n\n",
            a.heading(),
            text
        ));
    }
    out.push_str("Question:");
    Some(out)
}

/// Models like to wrap a reformatted result in a code fence anyway.
pub fn strip_fences(text: &str) -> String {
    let t = text.trim();
    if let Some(rest) = t.strip_prefix("```") {
        let body = rest.split_once('\n').map(|(_, b)| b).unwrap_or("");
        return body
            .trim_end()
            .trim_end_matches("```")
            .trim_end()
            .to_string();
    }
    t.to_string()
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// A one-shot text transform offered on clipboard entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    Translate,
    Reformat,
    Summarize,
}

impl Transform {
    pub fn label(self) -> String {
        i18n::umoja(match self {
            Transform::Translate => "Translate",
            Transform::Reformat => "Reformat",
            Transform::Summarize => "Summarize",
        })
    }

    /// System prompt for this transform; `language` is the user's own.
    pub fn system_prompt(self, language: &str) -> String {
        match self {
            Transform::Translate => format!(
                "You are a translator. Translate the user's text into {language}. If it is \
                 already in {language}, translate it into English instead. Keep the meaning, \
                 tone and formatting. Reply with only the translation."
            ),
            Transform::Reformat => "You tidy up text. Fix broken line wrapping, stray \
                whitespace, capitalization and punctuation without changing the wording. If \
                the text is data or code (JSON, YAML, CSV, a table, a shell command, source \
                code), pretty-print it in the same format instead. Reply with only the result, \
                with no code fences and no commentary."
                .into(),
            Transform::Summarize => format!(
                "You summarize text. Give the key points of the user's text in at most three \
                 short sentences or a short bulleted list, in {language}. Reply with only the \
                 summary."
            ),
        }
    }
}

/// English name of the user's language, for prompts ("German", "Swahili").
pub fn user_language() -> String {
    let lang = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|v| !v.is_empty() && v != "C" && v != "POSIX")
        .unwrap_or_default();
    language_name(&lang).to_string()
}

fn language_name(locale: &str) -> &'static str {
    let code = locale
        .split(['_', '.', '@', '-'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match code.as_str() {
        "ar" => "Arabic",
        "bg" => "Bulgarian",
        "ca" => "Catalan",
        "cs" => "Czech",
        "da" => "Danish",
        "de" => "German",
        "el" => "Greek",
        "es" => "Spanish",
        "fi" => "Finnish",
        "fr" => "French",
        "he" => "Hebrew",
        "hi" => "Hindi",
        "hu" => "Hungarian",
        "id" => "Indonesian",
        "it" => "Italian",
        "ja" => "Japanese",
        "ko" => "Korean",
        "nb" | "no" | "nn" => "Norwegian",
        "nl" => "Dutch",
        "pl" => "Polish",
        "pt" => "Portuguese",
        "ro" => "Romanian",
        "ru" => "Russian",
        "sk" => "Slovak",
        "sv" => "Swedish",
        "sw" => "Swahili",
        "th" => "Thai",
        "tr" => "Turkish",
        "uk" => "Ukrainian",
        "vi" => "Vietnamese",
        "zh" => "Chinese",
        _ => "English",
    }
}

/// The window the user was working in: the most recently used one on the
/// current workspace that isn't minimized.
#[derive(Debug, Clone, PartialEq)]
pub struct FocusedWindow {
    pub app_id: String,
    pub title: String,
    pub rect: (i32, i32, i32, i32),
}

pub async fn focused_window() -> Option<FocusedWindow> {
    use umoja_common::dbus;
    let conn = gio::bus_get_future(gio::BusType::Session).await.ok()?;
    let reply = conn
        .call_future(
            Some(dbus::COMPOSITOR_BUS_NAME),
            dbus::COMPOSITOR_OBJECT_PATH,
            dbus::COMPOSITOR_INTERFACE,
            "GetWindowsMru",
            None,
            Some(gtk4::glib::VariantTy::new("(a(tssiiiiu))").unwrap()),
            gio::DBusCallFlags::NONE,
            1000,
        )
        .await
        .map_err(|e| tracing::warn!(error = %e, "GetWindowsMru failed"))
        .ok()?;
    let (rows,) = reply.get::<(Vec<(u64, String, String, i32, i32, i32, i32, u32)>,)>()?;
    pick_focused(rows)
}

/// Flags as in umoja-mir's `WindowDesc`: 1 focused, 2 minimized, 4 on the
/// current workspace.
fn pick_focused(
    rows: Vec<(u64, String, String, i32, i32, i32, i32, u32)>,
) -> Option<FocusedWindow> {
    let usable = |f: u32| f & 4 != 0 && f & 2 == 0;
    let row = rows
        .iter()
        .find(|r| r.7 & 1 != 0 && usable(r.7))
        .or_else(|| rows.iter().find(|r| usable(r.7)))?;
    Some(FocusedWindow {
        app_id: row.1.clone(),
        title: row.2.clone(),
        rect: (row.3, row.4, row.5.max(1), row.6.max(1)),
    })
}

/// Reads a window's text with the overlay out of the way (set by the
/// overlay, which knows how to step aside).
pub type WindowReader =
    Rc<dyn Fn((i32, i32, i32, i32)) -> Pin<Box<dyn Future<Output = Result<String, OcrError>>>>>;

thread_local! {
    static WINDOW_READER: RefCell<Option<WindowReader>> = const { RefCell::new(None) };
}

pub fn set_window_reader(reader: WindowReader) {
    WINDOW_READER.with(|r| *r.borrow_mut() = Some(reader));
}

/// OCR the window at `rect`, for the assistant's read_focused_window tool.
pub async fn read_window(rect: (i32, i32, i32, i32)) -> Result<String, OcrError> {
    let reader = WINDOW_READER.with(|r| r.borrow().clone());
    match reader {
        Some(read) => read(rect).await,
        None => {
            let image = blocking(move || capture(Some(rect))).await?;
            blocking(move || ocr_file(&image.0)).await
        }
    }
}

/// Run blocking `f` on a thread and await its result on the GTK loop.
pub async fn blocking<T, F>(f: F) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(f());
    });
    rx.recv().await.expect("worker thread finished")
}

/// Why reading the screen failed, worded for the user.
#[derive(Debug, Clone, PartialEq)]
pub enum OcrError {
    NotInstalled,
    CaptureFailed,
    NoText,
}

impl OcrError {
    pub fn message(&self) -> String {
        match self {
            OcrError::NotInstalled => i18n::umoja(
                "Reading text from the screen needs Tesseract. Install it with: sudo apt install tesseract-ocr",
            ),
            OcrError::CaptureFailed => i18n::umoja("Couldn't capture the screen."),
            OcrError::NoText => i18n::umoja("No text was found there."),
        }
    }
}

pub fn ocr_available() -> bool {
    gtk4::glib::find_program_in_path("tesseract").is_some()
}

/// Capture `rect` (compositor coordinates, as `grim -g` takes them, or the
/// whole screen) to a temporary PNG. Blocking: call off the GTK thread.
pub fn capture(rect: Option<(i32, i32, i32, i32)>) -> Result<TempImage, OcrError> {
    if !ocr_available() {
        return Err(OcrError::NotInstalled);
    }
    let path = temp_png();
    let mut grim = Command::new("grim");
    if let Some((x, y, w, h)) = rect {
        grim.args(["-g", &format!("{x},{y} {w}x{h}")]);
    }
    match grim.arg(&path).status() {
        Ok(s) if s.success() => Ok(TempImage(path)),
        _ => Err(OcrError::CaptureFailed),
    }
}

/// A capture in the runtime dir, deleted when dropped.
pub struct TempImage(pub std::path::PathBuf);

impl Drop for TempImage {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn temp_png() -> std::path::PathBuf {
    gtk4::glib::user_runtime_dir().join(format!(
        "umoja-context-{}-{}.png",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ))
}

/// OCR an image file. Blocking.
pub fn ocr_file(path: &Path) -> Result<String, OcrError> {
    let out = Command::new("tesseract")
        .arg(path)
        .arg("stdout")
        .args(tesseract_langs())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|_| OcrError::NotInstalled)?;
    let text = clean_ocr(&String::from_utf8_lossy(&out.stdout));
    if text.is_empty() {
        Err(OcrError::NoText)
    } else {
        Ok(text)
    }
}

/// `-l eng+deu` for the installed language packs matching the user's locale.
fn tesseract_langs() -> Vec<String> {
    let Ok(out) = Command::new("tesseract")
        .arg("--list-langs")
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    let installed: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .map(|l| l.trim().to_string())
        .collect();
    let want = tesseract_code(&user_language());
    let mut langs: Vec<&str> = Vec::new();
    if installed.iter().any(|l| l == "eng") {
        langs.push("eng");
    }
    if want != "eng" && installed.iter().any(|l| l == want) {
        langs.push(want);
    }
    if langs.is_empty() {
        Vec::new()
    } else {
        vec!["-l".into(), langs.join("+")]
    }
}

fn tesseract_code(language: &str) -> &'static str {
    match language {
        "German" => "deu",
        "French" => "fra",
        "Spanish" => "spa",
        "Italian" => "ita",
        "Portuguese" => "por",
        "Dutch" => "nld",
        "Polish" => "pol",
        "Russian" => "rus",
        "Swahili" => "swa",
        "Japanese" => "jpn",
        "Chinese" => "chi_sim",
        "Korean" => "kor",
        _ => "eng",
    }
}

/// Drop the blank-line noise Tesseract produces around UI chrome.
pub fn clean_ocr(raw: &str) -> String {
    let mut out = String::new();
    let mut blank = false;
    for line in raw.lines().map(str::trim_end) {
        if line.trim().is_empty() {
            blank = true;
            continue;
        }
        // Lone glyphs are usually icons read as letters.
        if line.trim().chars().count() < 2 {
            continue;
        }
        if blank && !out.is_empty() {
            out.push('\n');
        }
        blank = false;
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_attachments_no_context() {
        assert_eq!(build_context(&[]), None);
    }

    #[test]
    fn context_carries_every_attachment() {
        let c = build_context(&[
            Attachment::new(Kind::Selection, "E: Unable to locate package foo"),
            Attachment::new(
                Kind::Window {
                    app_id: "org.gnome.Ptyxis".into(),
                    title: "Terminal".into(),
                },
                "ken@umoja:~$ sudo apt install foo",
            ),
        ])
        .unwrap();
        assert!(c.contains("Text the user selected"));
        assert!(c.contains("E: Unable to locate package foo"));
        assert!(c.contains("\"org.gnome.Ptyxis\""));
        assert!(c.ends_with("Question:"));
    }

    #[test]
    fn context_is_capped() {
        let big = "x".repeat(MAX_CONTEXT_CHARS * 2);
        let c = build_context(&[
            Attachment::new(Kind::Clipboard, big.clone()),
            Attachment::new(Kind::Selection, big),
        ])
        .unwrap();
        assert!(c.chars().count() < MAX_CONTEXT_CHARS + 600);
    }

    #[test]
    fn code_fences_are_dropped() {
        assert_eq!(strip_fences("```json\n{\"a\": 1}\n```"), "{\"a\": 1}");
        assert_eq!(strip_fences("  plain  "), "plain");
    }

    #[test]
    fn truncates_on_char_boundaries() {
        assert_eq!(truncate_chars("héllo", 2), "hé…");
        assert_eq!(truncate_chars("hi", 5), "hi");
    }

    #[test]
    fn languages() {
        assert_eq!(language_name("de_DE.UTF-8"), "German");
        assert_eq!(language_name("sw_KE"), "Swahili");
        assert_eq!(language_name("en_US.UTF-8"), "English");
        assert_eq!(language_name(""), "English");
        assert_eq!(tesseract_code("German"), "deu");
    }

    #[test]
    fn transform_prompts_name_the_language() {
        assert!(
            Transform::Translate
                .system_prompt("German")
                .contains("into German")
        );
    }

    #[test]
    fn focused_window_skips_minimized_and_other_workspaces() {
        let rows = vec![
            (1, "a".into(), "A".into(), 0, 0, 10, 10, 2 | 4),
            (2, "b".into(), "B".into(), 0, 0, 10, 10, 0),
            (3, "c".into(), "C".into(), 5, 6, 70, 80, 4),
        ];
        let w = pick_focused(rows).unwrap();
        assert_eq!((w.app_id.as_str(), w.rect), ("c", (5, 6, 70, 80)));
        let rows = vec![
            (3, "c".into(), "C".into(), 0, 0, 1, 1, 4),
            (4, "d".into(), "D".into(), 0, 0, 1, 1, 1 | 4),
        ];
        assert_eq!(pick_focused(rows).unwrap().app_id, "d");
        assert_eq!(pick_focused(vec![]), None);
    }

    #[test]
    fn ocr_cleanup() {
        assert_eq!(
            clean_ocr("  \nFile  Edit\n\n\n|\nerror: not found\n\n"),
            "File  Edit\n\nerror: not found"
        );
    }
}

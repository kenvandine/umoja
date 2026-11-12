//! Spoken output: assistant answers (and anything the assistant is asked
//! to read out) spoken in a Piper voice.
//!
//! This is not the screen reader. Orca and speech-dispatcher stay in
//! charge of accessibility; this is a nicer voice for things a sighted
//! user asks to hear. The default voice is Piper's en_GB "alan".
//!
//! Piper comes from the `piper-tts` snap (`piper-tts.piper-cli`), or any
//! `piper` on `PATH` (pip's piper-tts). Voices download from Hugging Face
//! (rhasspy/piper-voices) on first use. For the snap they go to
//! `~/snap/piper-tts/common/voices`, the one folder in the home directory
//! a strictly confined snap can always read; otherwise to
//! `~/.local/share/umoja/voices`. Piper writes raw 16-bit mono PCM, which
//! `pacat` (or `aplay`) plays. Without Piper, `spd-say` is used.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::lemonade::RUNTIME;

pub const SCHEMA: &str = "org.umoja.shell";
pub const DEFAULT_VOICE: &str = "en_GB-alan-medium";
const VOICES_URL: &str = "https://huggingface.co/rhasspy/piper-voices/resolve/main";
const SNAP_CLI: &str = "/snap/bin/piper-tts.piper-cli";
/// Longest text read out in one go.
const MAX_CHARS: usize = 6000;

/// Progress of one [`speak`] call, delivered on the GTK main loop.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The voice is being fetched (first use).
    Downloading,
    Speaking,
    /// Finished, or stopped.
    Done,
    Failed(String),
}

/// Bumped by every speak/stop: a superseded request stops quietly.
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// The processes of the speech playing now.
static PLAYING: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Speak `text` (Markdown is tidied first), stopping anything already
/// playing. `on_event` runs on the GTK main loop.
pub fn speak(text: &str, on_event: impl Fn(Event) + 'static) {
    stop();
    let generation = GENERATION.load(Ordering::SeqCst);
    let text = spoken_text(text);
    if text.is_empty() {
        on_event(Event::Done);
        return;
    }
    let voice = umoja_common::gsettings::get_string(SCHEMA, "speech-voice", DEFAULT_VOICE);
    let rate = speech_rate();
    let (tx, rx) = async_channel::unbounded();
    RUNTIME.spawn(async move {
        let result = run(&text, &voice, rate, generation, &tx).await;
        let _ = tx.send(match result {
            Ok(()) => Event::Done,
            Err(e) => {
                tracing::warn!(error = %e, "speech failed");
                Event::Failed(e)
            }
        })
        .await;
    });
    glib::spawn_future_local(async move {
        while let Ok(ev) = rx.recv().await {
            let last = matches!(ev, Event::Done | Event::Failed(_));
            on_event(ev);
            if last {
                break;
            }
        }
    });
}

/// Stop whatever is being spoken.
pub fn stop() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    let pids = std::mem::take(&mut *PLAYING.lock().unwrap());
    for pid in pids {
        // SAFETY: plain kill(2) on a child we started and haven't reaped.
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
}

/// `speech-rate` (1.0 = normal), clamped to something listenable.
fn speech_rate() -> f64 {
    umoja_common::gsettings::settings(SCHEMA)
        .filter(|s| umoja_common::gsettings::has_key(s, "speech-rate"))
        .map(|s| gio::prelude::SettingsExt::double(&s, "speech-rate"))
        .unwrap_or(1.0)
        .clamp(0.5, 2.0)
}

async fn run(
    text: &str,
    voice: &str,
    rate: f64,
    generation: u64,
    tx: &async_channel::Sender<Event>,
) -> Result<(), String> {
    let engine = tokio::task::spawn_blocking(find_engine)
        .await
        .map_err(|e| e.to_string())?;
    let current = || GENERATION.load(Ordering::SeqCst) == generation;
    let (cli, voices) = match engine {
        Some(Engine::Piper { cli, voices }) => (cli, voices),
        None => return Err("no voice installed: sudo snap install piper-tts".into()),
        // No Piper: speech-dispatcher's voice beats saying nothing.
        Some(Engine::SpeechDispatcher) => {
        if !current() {
            return Ok(());
        }
        let _ = tx.send(Event::Speaking).await;
        let text = text.to_owned();
        return tokio::task::spawn_blocking(move || speak_with_spd(&text, generation))
            .await
            .map_err(|e| e.to_string())?;
        }
    };
    let voice = if valid_voice(voice) { voice } else { DEFAULT_VOICE };
    let model = voices.join(format!("{voice}.onnx"));
    let config = voices.join(format!("{voice}.onnx.json"));
    if !model.exists() || !config.exists() {
        let _ = tx.send(Event::Downloading).await;
        download_voice(voice, &voices).await?;
    }
    if !current() {
        return Ok(());
    }
    let rate_hz = sample_rate(&config);
    let _ = tx.send(Event::Speaking).await;
    let text = text.to_owned();
    tokio::task::spawn_blocking(move || {
        speak_with_piper(&cli, &model, &config, rate, rate_hz, &text, generation)
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---- engines --------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Engine {
    Piper { cli: PathBuf, voices: PathBuf },
    SpeechDispatcher,
}

fn home() -> PathBuf {
    glib::home_dir()
}

fn find_engine() -> Option<Engine> {
    if Path::new(SNAP_CLI).exists() {
        return Some(Engine::Piper {
            cli: SNAP_CLI.into(),
            voices: home().join("snap/piper-tts/common/voices"),
        });
    }
    // Ubuntu's "piper" package is a gaming-mouse app: only take a piper
    // whose help talks about voice models.
    if let Some(cli) = glib::find_program_in_path("piper")
        && Command::new(&cli)
            .arg("--help")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("--model"))
    {
        return Some(Engine::Piper {
            cli,
            voices: glib::user_data_dir().join("umoja/voices"),
        });
    }
    glib::find_program_in_path("spd-say").map(|_| Engine::SpeechDispatcher)
}

/// Remember a child so [`stop`] can end it; false if already superseded.
fn track(child: &Child, generation: u64) -> bool {
    let mut playing = PLAYING.lock().unwrap();
    if GENERATION.load(Ordering::SeqCst) != generation {
        return false;
    }
    playing.push(child.id());
    true
}

fn untrack(children: &[&Child]) {
    let mut playing = PLAYING.lock().unwrap();
    playing.retain(|p| !children.iter().any(|c| c.id() == *p));
}

fn player(sample_rate: u32) -> Option<Command> {
    if let Some(pacat) = glib::find_program_in_path("pacat") {
        let mut c = Command::new(pacat);
        c.args([
            "--playback",
            "--raw",
            "--format=s16le",
            "--channels=1",
            &format!("--rate={sample_rate}"),
            "--client-name=Umoja",
            "--stream-name=Umoja speech",
            "--property=application.icon_name=audio-speakers-symbolic",
        ]);
        return Some(c);
    }
    let aplay = glib::find_program_in_path("aplay")?;
    let mut c = Command::new(aplay);
    c.args(["-q", "-t", "raw", "-f", "S16_LE", "-c", "1", "-r", &sample_rate.to_string()]);
    Some(c)
}

fn speak_with_piper(
    cli: &Path,
    model: &Path,
    config: &Path,
    rate: f64,
    sample_rate: u32,
    text: &str,
    generation: u64,
) -> Result<(), String> {
    use std::io::Write;
    let mut play = player(sample_rate).ok_or("no audio player (pacat or aplay) installed")?;
    // The snap's wrapper passes its own --model first; piper keeps the last.
    let mut piper = Command::new(cli)
        .arg("--model")
        .arg(model)
        .arg("--config")
        .arg(config)
        .arg("--length-scale")
        .arg(format!("{:.2}", 1.0 / rate))
        .arg("--output-raw")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start piper: {e}"))?;
    let audio = piper.stdout.take().expect("piped");
    let mut out = match play.stdin(audio).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = piper.kill();
            let _ = piper.wait();
            return Err(format!("could not start the audio player: {e}"));
        }
    };
    if !(track(&piper, generation) && track(&out, generation)) {
        let _ = piper.kill();
        let _ = out.kill();
    } else if let Some(mut stdin) = piper.stdin.take() {
        // One utterance per line.
        let _ = stdin.write_all(text.as_bytes());
        let _ = stdin.write_all(b"\n");
    }
    let piper_status = piper.wait();
    let _ = out.wait();
    untrack(&[&piper, &out]);
    match piper_status {
        Ok(s) if s.success() => Ok(()),
        // Stopped on purpose.
        _ if GENERATION.load(Ordering::SeqCst) != generation => Ok(()),
        Ok(s) => Err(format!("piper failed ({s})")),
        Err(e) => Err(e.to_string()),
    }
}

fn speak_with_spd(text: &str, generation: u64) -> Result<(), String> {
    use std::io::Write;
    let mut child = Command::new("spd-say")
        .args(["--wait", "--pipe-mode"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start spd-say: {e}"))?;
    if !track(&child, generation) {
        let _ = child.kill();
    } else if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    let _ = child.wait();
    untrack(&[&child]);
    Ok(())
}

// ---- voices ---------------------------------------------------------------------

/// "en_GB-alan-medium": language_REGION-name-quality, nothing that could
/// leave the voices folder or the URL path.
fn valid_voice(voice: &str) -> bool {
    let parts: Vec<&str> = voice.split('-').collect();
    parts.len() == 3
        && parts[0].len() >= 2
        && parts.iter().all(|p| {
            !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}

/// The voice's folder in rhasspy/piper-voices: en/en_GB/alan/medium.
fn voice_path(voice: &str) -> Option<String> {
    if !valid_voice(voice) {
        return None;
    }
    let mut parts = voice.split('-');
    let locale = parts.next()?;
    let name = parts.next()?;
    let quality = parts.next()?;
    let lang = locale.split('_').next()?;
    Some(format!("{lang}/{locale}/{name}/{quality}"))
}

async fn download_voice(voice: &str, dir: &Path) -> Result<(), String> {
    let folder = voice_path(voice).ok_or_else(|| format!("bad voice name {voice}"))?;
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    // The config first: a model without one is no use.
    for file in [format!("{voice}.onnx.json"), format!("{voice}.onnx")] {
        let dest = dir.join(&file);
        if dest.exists() {
            continue;
        }
        let url = format!("{VOICES_URL}/{folder}/{file}?download=true");
        tracing::info!(%url, "downloading Piper voice");
        let part = dir.join(format!("{file}.part"));
        let result = fetch(&client, &url, &part).await;
        if let Err(e) = result {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(format!("could not download the {voice} voice: {e}"));
        }
        tokio::fs::rename(&part, &dest).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn fetch(client: &reqwest::Client, url: &str, dest: &Path) -> Result<(), String> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    let resp = client
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?;
    let mut file = tokio::fs::File::create(dest).await.map_err(|e| e.to_string())?;
    let mut body = resp.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
    }
    file.flush().await.map_err(|e| e.to_string())
}

/// The voice's sample rate from its .onnx.json (most are 22050 Hz).
fn sample_rate(config: &Path) -> u32 {
    std::fs::read_to_string(config)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v["audio"]["sample_rate"].as_u64())
        .and_then(|r| u32::try_from(r).ok())
        .filter(|r| (8000..=96000).contains(r))
        .unwrap_or(22050)
}

// ---- text -----------------------------------------------------------------------

/// What to say for an answer written in Markdown: no code blocks, link
/// targets, emphasis marks or list bullets; one line per paragraph or
/// list item (Piper speaks a line at a time).
pub fn spoken_text(markdown: &str) -> String {
    let mut lines = Vec::new();
    let mut in_code = false;
    let mut skipped_code = false;
    for raw in markdown.lines() {
        let line = raw.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            in_code = !in_code;
            if in_code && !skipped_code {
                skipped_code = true;
                lines.push(String::from("(Code shown on screen.)"));
            }
            continue;
        }
        if in_code || line.is_empty() {
            continue;
        }
        let line = line.trim_start_matches('#').trim_start();
        let line = line.strip_prefix("> ").unwrap_or(line);
        let line = strip_bullet(line);
        let line = strip_inline(line);
        if !line.trim().is_empty() {
            lines.push(line.trim().to_owned());
        }
    }
    let mut text = lines.join("\n");
    if text.chars().count() > MAX_CHARS {
        text = text.chars().take(MAX_CHARS).collect();
    }
    text
}

fn strip_bullet(line: &str) -> &str {
    for b in ["- ", "* ", "+ ", "• "] {
        if let Some(rest) = line.strip_prefix(b) {
            return rest;
        }
    }
    // "1. " / "2) "
    let digits = line.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 && digits < 4 {
        let rest = &line[digits..];
        if let Some(r) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return r;
        }
    }
    line
}

/// `[text](url)` -> text, and drop `*`, `_` emphasis and backticks.
fn strip_inline(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '['
            && let Some(close) = chars[i..].iter().position(|&c| c == ']').map(|p| p + i)
            && chars.get(close + 1) == Some(&'(')
            && let Some(end) = chars[close..].iter().position(|&c| c == ')').map(|p| p + close)
        {
            out.extend(&chars[i + 1..close]);
            i = end + 1;
            continue;
        }
        match c {
            '`' | '*' => {}
            // Emphasis underscores, not snake_case ones.
            '_' if !(i > 0
                && chars[i - 1].is_alphanumeric()
                && chars.get(i + 1).is_some_and(|n| n.is_alphanumeric())) => {}
            _ => out.push(c),
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_names_map_to_their_folder() {
        assert_eq!(voice_path("en_GB-alan-medium").as_deref(), Some("en/en_GB/alan/medium"));
        assert_eq!(
            voice_path("en_US-libritts_r-medium").as_deref(),
            Some("en/en_US/libritts_r/medium")
        );
        assert_eq!(voice_path("../../etc-passwd-x"), None);
        assert_eq!(voice_path("en_GB-alan"), None);
        assert_eq!(voice_path("en_GB-al an-medium"), None);
        assert_eq!(voice_path(""), None);
    }

    #[test]
    fn markdown_is_tidied_for_speaking() {
        let md = "## Steps\n\nRun **this** in a [terminal](https://example.com):\n\n```sh\nsudo apt update\n```\n\n1. Open `Settings`\n- Pick _Displays_\n> Note: my_file stays\n";
        assert_eq!(
            spoken_text(md),
            "Steps\nRun this in a terminal:\n(Code shown on screen.)\nOpen Settings\nPick Displays\nNote: my_file stays"
        );
    }

    #[test]
    fn empty_and_code_only_answers() {
        assert_eq!(spoken_text("  \n\n"), "");
        assert_eq!(spoken_text("```\nls\n```"), "(Code shown on screen.)");
    }

    #[test]
    fn long_text_is_capped() {
        let long = "word ".repeat(5000);
        assert!(spoken_text(&long).chars().count() <= MAX_CHARS);
    }

    #[test]
    fn sample_rate_falls_back() {
        assert_eq!(sample_rate(Path::new("/nonexistent")), 22050);
        let dir = std::env::temp_dir().join(format!("umoja-speech-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("v.onnx.json");
        std::fs::write(&f, r#"{"audio": {"sample_rate": 16000}}"#).unwrap();
        assert_eq!(sample_rate(&f), 16000);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

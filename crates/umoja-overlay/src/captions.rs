//! Live captions for whatever the computer plays: videos, calls, podcasts.
//!
//! The default output's monitor is recorded with `parec` (16 kHz mono),
//! cut into short phrases at pauses, and each phrase is transcribed by the
//! embedded Lemonade's speech-to-text (`/api/v1/audio/transcriptions`, a
//! Whisper model, downloaded on first use). The text shows in a bubble at
//! the bottom of the screen; **Summarize** hands what was said to Ask
//! Umoja. Everything stays on this computer, and the transcript is only
//! kept in memory until captions are turned off.

use crate::lemonade::RUNTIME;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::RefCell;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::time::Duration;
use umoja_common::gsettings as gs;
use umoja_common::{i18n, lemonade};

pub const RATE: usize = 16_000;
/// A phrase is cut after a pause once it is this long...
const MIN_SAMPLES: usize = RATE * 2;
/// ...and always at this length.
const MAX_SAMPLES: usize = RATE * 7;
/// 100 ms frames for the pause detector.
const FRAME: usize = RATE / 10;
/// RMS (of i16 samples) below which a frame counts as quiet.
const QUIET: f64 = 300.0;
/// Characters shown in the bubble.
const SHOWN: usize = 220;
/// Transcript kept for Summarize.
const KEPT: usize = 40_000;

pub const DEFAULT_MODEL: &str = "Whisper-Base";

fn model() -> String {
    gs::settings(gs::UMOJA_SHELL)
        .filter(|s| gs::has_key(s, "captions-model"))
        .map(|s| s.string("captions-model").to_string())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.into())
}

fn rms(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / samples.len() as f64).sqrt()
}

/// Cuts a sample stream into phrases at pauses.
#[derive(Default)]
pub struct Segmenter {
    buf: Vec<i16>,
    quiet_frames: usize,
}

impl Segmenter {
    /// Add samples; returns finished phrases (silent ones are dropped).
    pub fn push(&mut self, samples: &[i16]) -> Vec<Vec<i16>> {
        let mut out = Vec::new();
        for frame in samples.chunks(FRAME) {
            let quiet = rms(frame) < QUIET;
            if quiet && self.buf.is_empty() {
                continue; // don't start a phrase with silence
            }
            self.buf.extend_from_slice(frame);
            self.quiet_frames = if quiet { self.quiet_frames + 1 } else { 0 };
            let pause = self.quiet_frames >= 4 && self.buf.len() >= MIN_SAMPLES;
            if pause || self.buf.len() >= MAX_SAMPLES {
                let phrase = std::mem::take(&mut self.buf);
                self.quiet_frames = 0;
                if rms(&phrase) >= QUIET {
                    out.push(phrase);
                }
            }
        }
        out
    }
}

/// 16-bit mono WAV for `samples`.
pub fn wav(samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut v = Vec::with_capacity(44 + data_len as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&1u16.to_le_bytes()); // mono
    v.extend_from_slice(&(RATE as u32).to_le_bytes());
    v.extend_from_slice(&(RATE as u32 * 2).to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

/// Whisper's text for a phrase, without its noise markers and the stock
/// phrases it invents for music and silence.
pub fn clean(text: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in text.chars() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let t = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = t.to_lowercase();
    const INVENTED: &[&str] = &["thank you.", "thanks for watching!", "thank you for watching.", "you", "♪", "."];
    if INVENTED.contains(&lower.as_str()) { String::new() } else { t }
}

fn multipart(boundary: &str, model: &str, prompt: &str, audio: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    let field = |b: &mut Vec<u8>, name: &str, value: &str| {
        b.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
    };
    field(&mut b, "model", model);
    field(&mut b, "response_format", "json");
    if !prompt.is_empty() {
        field(&mut b, "prompt", prompt);
    }
    b.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"phrase.wav\"\r\nContent-Type: audio/wav\r\n\r\n").as_bytes(),
    );
    b.extend_from_slice(audio);
    b.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    b
}

#[derive(Debug)]
enum SttError {
    /// The model isn't downloaded yet.
    NoModel,
    Other(String),
}

async fn transcribe(model: String, prompt: String, samples: Vec<i16>) -> Result<String, SttError> {
    let boundary = format!("umoja{:x}", glib::random_int());
    let body = multipart(&boundary, &model, &prompt, &wav(&samples));
    let url = format!("{}/api/v1/audio/transcriptions", lemonade::base_url());
    let resp = reqwest::Client::new()
        .post(url)
        .header("Content-Type", format!("multipart/form-data; boundary={boundary}"))
        .timeout(Duration::from_secs(120))
        .body(body)
        .send()
        .await
        .map_err(|e| SttError::Other(e.to_string()))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.as_u16() == 404 || text.contains("not found") || text.contains("not installed") || text.contains("not downloaded") {
        return Err(SttError::NoModel);
    }
    if !status.is_success() {
        return Err(SttError::Other(format!("HTTP {status}: {}", text.chars().take(200).collect::<String>())));
    }
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| SttError::Other(e.to_string()))?;
    Ok(v["text"].as_str().unwrap_or("").to_string())
}

async fn pull(model: String) -> Result<(), String> {
    let resp = reqwest::Client::new()
        .post(lemonade::pull_url())
        .timeout(Duration::from_secs(1800))
        .json(&serde_json::json!({"model_name": model, "stream": false}))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let ok = resp.status().is_success();
    let body = resp.text().await.unwrap_or_default();
    if ok && !body.contains("\"error\"") { Ok(()) } else { Err(body.chars().take(200).collect()) }
}

struct Captions {
    window: gtk::Window,
    label: gtk::Label,
    status: gtk::Label,
    recorder: RefCell<Option<Child>>,
    transcript: RefCell<String>,
    /// Bumped on stop, so a late phrase from the last run is dropped.
    run: std::cell::Cell<u32>,
}

thread_local! {
    static CAPTIONS: RefCell<Option<Rc<Captions>>> = const { RefCell::new(None) };
}

pub type Summarize = Rc<dyn Fn(String)>;

/// Turn captions on or off. `summarize` gets the transcript when the user
/// presses Summarize.
pub fn toggle(app: &gtk::Application, summarize: Summarize) {
    let c = CAPTIONS.with(|c| c.borrow().clone()).unwrap_or_else(|| {
        let c = Captions::new(app, summarize);
        CAPTIONS.with(|s| *s.borrow_mut() = Some(c.clone()));
        c
    });
    if c.recorder.borrow().is_some() {
        c.stop();
    } else {
        c.start();
    }
}

impl Captions {
    fn new(app: &gtk::Application, summarize: Summarize) -> Rc<Self> {
        let window = gtk::Window::builder().application(app).css_classes(["captions"]).build();
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_namespace("umoja-captions");
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_anchor(Edge::Bottom, true);
        window.set_margin(Edge::Bottom, 96);
        umoja_common::blur::blur_behind(&window);

        let label = gtk::Label::builder()
            .wrap(true)
            .lines(2)
            .ellipsize(gtk::pango::EllipsizeMode::Start)
            .max_width_chars(70)
            .width_chars(50)
            .xalign(0.0)
            .css_classes(["captions-text"])
            .build();
        label.set_accessible_role(gtk::AccessibleRole::Status);
        let status = gtk::Label::builder().xalign(0.0).css_classes(["captions-status", "dim-label"]).build();
        let summarize_b = gtk::Button::builder()
            .label(i18n::umoja("Summarize"))
            .css_classes(["flat", "captions-button"])
            .tooltip_text(i18n::umoja("Ask Umoja to sum up what was said"))
            .build();
        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .css_classes(["flat", "circular", "captions-button"])
            .tooltip_text(i18n::umoja("Turn Off Live Captions"))
            .build();
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let caption = gtk::Label::builder().label(i18n::umoja("Live Captions")).xalign(0.0).hexpand(true).css_classes(["captions-title"]).build();
        bar.append(&caption);
        bar.append(&status);
        bar.append(&summarize_b);
        bar.append(&close);
        let col = gtk::Box::new(gtk::Orientation::Vertical, 4);
        col.append(&bar);
        col.append(&label);
        window.set_child(Some(&col));

        let this = Rc::new(Self {
            window,
            label,
            status,
            recorder: RefCell::new(None),
            transcript: RefCell::default(),
            run: std::cell::Cell::new(0),
        });
        let weak = Rc::downgrade(&this);
        close.connect_clicked(move |_| {
            if let Some(c) = weak.upgrade() {
                c.stop();
            }
        });
        let weak = Rc::downgrade(&this);
        summarize_b.connect_clicked(move |_| {
            if let Some(c) = weak.upgrade() {
                let text = c.transcript.borrow().trim().to_string();
                if !text.is_empty() {
                    summarize(text);
                }
            }
        });
        this
    }

    fn set_status(&self, s: &str) {
        self.status.set_text(s);
    }

    fn show_text(&self) {
        let t = self.transcript.borrow();
        let n = t.chars().count();
        let tail: String = t.chars().skip(n.saturating_sub(SHOWN)).collect();
        self.label.set_text(tail.trim());
    }

    fn start(self: &Rc<Self>) {
        let child = Command::new("parec")
            .args(["--device=@DEFAULT_MONITOR@", "--raw", "--format=s16le", "--rate=16000", "--channels=1", "--latency-msec=100"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "live captions: parec isn't available");
                self.transcript.replace(String::new());
                self.label.set_text(&i18n::umoja("Live captions need the pulseaudio-utils package (sudo apt install pulseaudio-utils)."));
                self.window.present();
                return;
            }
        };
        let Some(mut out) = child.stdout.take() else { return };
        self.recorder.replace(Some(child));
        self.transcript.replace(String::new());
        self.label.set_text(&i18n::umoja("Listening…"));
        self.set_status("");
        self.window.present();
        let run = self.run.get() + 1;
        self.run.set(run);

        // Reader thread: raw samples → phrases.
        let (tx, rx) = async_channel::unbounded::<Vec<i16>>();
        std::thread::Builder::new()
            .name("umoja-captions".into())
            .spawn(move || {
                let mut seg = Segmenter::default();
                let mut bytes = vec![0u8; FRAME * 2];
                loop {
                    match out.read_exact(&mut bytes) {
                        Ok(()) => {
                            let samples: Vec<i16> = bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
                            for phrase in seg.push(&samples) {
                                if tx.send_blocking(phrase).is_err() {
                                    return;
                                }
                            }
                        }
                        Err(_) => return,
                    }
                }
            })
            .ok();

        // Phrases → text, one at a time, in order.
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let model = model();
            let mut pulled = false;
            while let Ok(phrase) = rx.recv().await {
                // Fell behind (a slow first load): skip to the newest.
                let phrase = {
                    let mut p = phrase;
                    while let Ok(next) = rx.try_recv() {
                        p = next;
                    }
                    p
                };
                let Some(c) = weak.upgrade() else { return };
                if c.run.get() != run {
                    return;
                }
                let prompt: String = {
                    let t = c.transcript.borrow();
                    let n = t.chars().count();
                    t.chars().skip(n.saturating_sub(200)).collect()
                };
                drop(c);
                let (m, p, s) = (model.clone(), prompt.clone(), phrase.clone());
                let mut r = RUNTIME.spawn(transcribe(m, p, s)).await.unwrap_or(Err(SttError::Other("crashed".into())));
                if matches!(r, Err(SttError::NoModel)) && !pulled {
                    pulled = true;
                    if let Some(c) = weak.upgrade() {
                        c.set_status(&i18n::umoja("Downloading the speech model…"));
                    }
                    let pr = RUNTIME.spawn(pull(model.clone())).await.unwrap_or(Err("crashed".into()));
                    if let Some(c) = weak.upgrade() {
                        c.set_status("");
                        if let Err(e) = pr {
                            c.label.set_text(&format!("{}: {e}", i18n::umoja("Couldn't download the speech model")));
                            continue;
                        }
                    }
                    r = RUNTIME.spawn(transcribe(model.clone(), prompt, phrase)).await.unwrap_or(Err(SttError::Other("crashed".into())));
                }
                let Some(c) = weak.upgrade() else { return };
                if c.run.get() != run {
                    return;
                }
                match r {
                    Ok(text) => {
                        let text = clean(&text);
                        if !text.is_empty() {
                            {
                                let mut t = c.transcript.borrow_mut();
                                if !t.is_empty() {
                                    t.push(' ');
                                }
                                t.push_str(&text);
                                if t.len() > KEPT {
                                    let cut = t.len() - KEPT;
                                    let cut = (cut..t.len()).find(|i| t.is_char_boundary(*i)).unwrap_or(0);
                                    t.drain(..cut);
                                }
                            }
                            c.show_text();
                        }
                    }
                    Err(e) => {
                        let why = match e {
                            SttError::Other(m) => m,
                            SttError::NoModel => "no speech model".into(),
                        };
                        tracing::warn!(error = %why, "live captions: transcription failed");
                        c.set_status(&i18n::umoja("Speech-to-text isn't answering"));
                    }
                }
            }
        });
    }

    fn stop(&self) {
        self.run.set(self.run.get() + 1);
        if let Some(mut c) = self.recorder.borrow_mut().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.window.set_visible(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, amp: i16) -> Vec<i16> {
        (0..n).map(|i| if i % 2 == 0 { amp } else { -amp }).collect()
    }

    #[test]
    fn phrases_cut_at_pauses() {
        let mut s = Segmenter::default();
        assert!(s.push(&tone(RATE, 0)).is_empty(), "silence alone is nothing");
        let mut audio = tone(RATE * 3, 3000);
        audio.extend(tone(RATE / 2, 0));
        let out = s.push(&audio);
        assert_eq!(out.len(), 1);
        assert!(out[0].len() >= RATE * 3);
        // Speech that never pauses is cut at the maximum.
        let out = s.push(&tone(RATE * 15, 3000));
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|p| p.len() == MAX_SAMPLES));
    }

    #[test]
    fn wav_header() {
        let w = wav(&[1, -1]);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(w.len(), 48);
        assert_eq!(u32::from_le_bytes([w[40], w[41], w[42], w[43]]), 4);
    }

    #[test]
    fn whisper_noise_is_dropped() {
        assert_eq!(clean(" [BLANK_AUDIO] "), "");
        assert_eq!(clean("(music) Hello there"), "Hello there");
        assert_eq!(clean("Thank you."), "");
        assert_eq!(clean("Thank you for coming, everyone."), "Thank you for coming, everyone.");
    }
}

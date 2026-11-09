//! Makes sure the assistant's model is on disk, downloading it in the
//! background at login (and whenever `lemonade-model` changes) instead of
//! on the first question, with a progress notification and a status line
//! in the overlay.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use futures_util::StreamExt;
use gio::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use serde::Deserialize;
use umoja_common::{gsettings as gs, i18n, lemonade};

use crate::lemonade::{DEFAULT_MODEL, RUNTIME};

#[derive(Debug, Clone, PartialEq)]
pub enum ModelEvent {
    /// Overall progress, 0-100, and the total size in bytes if known.
    Progress {
        percent: f64,
        total_bytes: Option<u64>,
    },
    /// The model is ready; `downloaded` says whether we fetched anything.
    Ready {
        downloaded: bool,
    },
    Failed(String),
}

#[derive(Deserialize, Default)]
struct PullData {
    #[serde(default)]
    bytes_downloaded: u64,
    #[serde(default)]
    bytes_previously_downloaded: u64,
    #[serde(default)]
    bytes_total: u64,
    #[serde(default)]
    total_download_size: u64,
    #[serde(default)]
    file_index: u32,
    #[serde(default)]
    error: Option<String>,
}

/// One server-sent event from `POST /api/v1/pull`, already folded into
/// overall progress.
#[derive(Debug, PartialEq)]
enum PullEvent {
    /// Overall percent across all files, and the total size.
    Progress {
        percent: f64,
        total: u64,
        fetching: bool,
    },
    Complete,
    Error(String),
}

/// Parses lemond's pull stream (v11.9). Its progress events are per file
/// (`file_index` of `total_files`), arrive by the hundred thousand, and
/// start each file with a bogus tiny `bytes_total` (a redirect body)
/// that briefly reads 100%. The final `complete` event has every field
/// zeroed. So: keep the largest `bytes_total` seen for each file, ignore
/// readings against smaller totals, and add the files up against
/// `total_download_size`.
#[derive(Default)]
struct SseParser {
    event: String,
    total: u64,
    /// file_index -> (bytes_total, bytes_downloaded)
    files: std::collections::BTreeMap<u32, (u64, u64)>,
    /// Whether any bytes actually came over the network.
    fetching: bool,
}

impl SseParser {
    fn line(&mut self, line: &str) -> Option<PullEvent> {
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(name) = line.strip_prefix("event:") {
            self.event = name.trim().to_string();
            return None;
        }
        let data = line.strip_prefix("data:")?.trim();
        let d: PullData = serde_json::from_str(data).unwrap_or_default();
        let event = std::mem::take(&mut self.event);
        match event.as_str() {
            "complete" => return Some(PullEvent::Complete),
            "error" => {
                return Some(PullEvent::Error(
                    d.error.unwrap_or_else(|| data.to_string()),
                ));
            }
            _ if d.error.is_some() => return Some(PullEvent::Error(d.error.unwrap_or_default())),
            _ => {}
        }
        if d.total_download_size > 0 {
            self.total = self.total.max(d.total_download_size);
        }
        let entry = self.files.entry(d.file_index).or_insert((0, 0));
        if d.bytes_total > entry.0 {
            *entry = (d.bytes_total, d.bytes_downloaded);
        } else if d.bytes_total == entry.0 {
            entry.1 = entry.1.max(d.bytes_downloaded);
        } else {
            return None; // a reading against the bogus small total
        }
        if d.bytes_downloaded > d.bytes_previously_downloaded {
            self.fetching = true;
        }
        if self.total == 0 {
            return None;
        }
        let done: u64 = self.files.values().map(|(_, b)| *b).sum();
        Some(PullEvent::Progress {
            percent: (done as f64 * 100.0 / self.total as f64).clamp(0.0, 100.0),
            total: self.total,
            fetching: self.fetching,
        })
    }
}

/// Wait for the embedded server, then pull `model`, reporting to `tx`.
async fn ensure(model: String, tx: async_channel::Sender<ModelEvent>) {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .build()
        .unwrap_or_default();

    // umoja-lemonade.service starts alongside us; give it a few minutes.
    let mut up = false;
    for _ in 0..180 {
        if matches!(client.get(lemonade::health_url()).send().await, Ok(r) if r.status().is_success())
        {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    if !up {
        tracing::warn!("embedded Lemonade never came up; not pulling the model");
        return;
    }

    let resp = client
        .post(lemonade::pull_url())
        .json(&serde_json::json!({ "model_name": model, "stream": true }))
        .send()
        .await
        .and_then(|r| r.error_for_status());
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.send(ModelEvent::Failed(e.to_string())).await;
            return;
        }
    };

    let mut parser = SseParser::default();
    let mut downloaded = false;
    let mut last_step = -1;
    let mut buf = String::new();
    let mut body = resp.bytes_stream();
    while let Some(chunk) = body.next().await {
        let Ok(chunk) = chunk else {
            let _ = tx
                .send(ModelEvent::Failed(i18n::umoja(
                    "Connection to the assistant service was lost",
                )))
                .await;
            return;
        };
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(nl) = buf.find('\n') {
            let line: String = buf.drain(..=nl).collect();
            match parser.line(&line) {
                Some(PullEvent::Progress {
                    percent,
                    total,
                    fetching,
                }) => {
                    // Cached models just report what's on disk; and lemond
                    // sends ~10k events a second, so only pass on whole
                    // percent steps.
                    let step = percent.floor() as i32;
                    if !fetching || step == last_step {
                        continue;
                    }
                    last_step = step;
                    downloaded = true;
                    let _ = tx
                        .send(ModelEvent::Progress {
                            percent,
                            total_bytes: Some(total),
                        })
                        .await;
                }
                Some(PullEvent::Complete) => {
                    let _ = tx.send(ModelEvent::Ready { downloaded }).await;
                    return;
                }
                Some(PullEvent::Error(e)) => {
                    let _ = tx.send(ModelEvent::Failed(e)).await;
                    return;
                }
                None => {}
            }
        }
    }
    let _ = tx.send(ModelEvent::Ready { downloaded }).await;
}

/// Shows download progress as a notification and through `on_status`.
pub struct ModelWatcher {
    notification_id: Cell<u32>,
    /// The user closed the progress notification: stop re-posting it.
    progress_dismissed: Cell<bool>,
    last_percent: Cell<i32>,
    generation: Cell<u32>,
    on_status: Box<dyn Fn(Option<String>)>,
    _settings: RefCell<Option<gio::Settings>>,
}

impl ModelWatcher {
    pub fn start(on_status: impl Fn(Option<String>) + 'static) -> Rc<Self> {
        let this = Rc::new(Self {
            notification_id: Cell::new(0),
            progress_dismissed: Cell::new(false),
            last_percent: Cell::new(-1),
            generation: Cell::new(0),
            on_status: Box::new(on_status),
            _settings: RefCell::default(),
        });
        if !gs::get_bool(gs::UMOJA_SHELL, "lemonade-enabled", true) {
            return this;
        }
        this.watch_closed();
        let weak = Rc::downgrade(&this);
        // `watch` runs once now, and again when the model setting changes.
        let s = gs::watch(gs::UMOJA_SHELL, "lemonade-model", move |_, _| {
            if let Some(this) = weak.upgrade() {
                this.pull(current_model());
            }
        });
        if s.is_none() {
            this.pull(current_model());
        }
        *this._settings.borrow_mut() = s;
        this
    }

    /// Notice when the user dismisses our progress notification, so the
    /// next percent doesn't pop it straight back up.
    fn watch_closed(self: &Rc<Self>) {
        let Ok(conn) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
            return;
        };
        let weak = Rc::downgrade(self);
        // The subscription lives as long as the session bus connection.
        let _ = conn.signal_subscribe(
            Some("org.freedesktop.Notifications"),
            Some("org.freedesktop.Notifications"),
            Some("NotificationClosed"),
            Some("/org/freedesktop/Notifications"),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, params| {
                let Some(this) = weak.upgrade() else { return };
                let Some((id, reason)) = params.get::<(u32, u32)>() else {
                    return;
                };
                if id != 0 && id == this.notification_id.get() {
                    this.notification_id.set(0);
                    // 2 = dismissed by the user.
                    if reason == 2 && this.last_percent.get() >= 0 {
                        this.progress_dismissed.set(true);
                    }
                }
            },
        );
    }

    /// Start the download once we're online on an unmetered connection
    /// (GNOME's rule for background downloads); until then, wait.
    fn pull(self: &Rc<Self>, model: String) {
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        self.last_percent.set(-1);
        self.progress_dismissed.set(false);
        let monitor = gio::NetworkMonitor::default();
        if online(&monitor) {
            self.start_pull(model, generation);
            return;
        }
        tracing::info!(
            model,
            "waiting for an unmetered connection to fetch the model"
        );
        let weak = Rc::downgrade(self);
        let handler: Rc<RefCell<Option<glib::SignalHandlerId>>> = Rc::default();
        let h = handler.clone();
        let id = monitor.connect_network_changed(move |m, _| {
            let Some(this) = weak.upgrade() else { return };
            if this.generation.get() != generation {
                if let Some(id) = h.borrow_mut().take() {
                    m.disconnect(id);
                }
                return;
            }
            if online(m) {
                if let Some(id) = h.borrow_mut().take() {
                    m.disconnect(id);
                }
                this.start_pull(model.clone(), generation);
            }
        });
        *handler.borrow_mut() = Some(id);
    }

    fn start_pull(self: &Rc<Self>, model: String, generation: u32) {
        let (tx, rx) = async_channel::unbounded();
        RUNTIME.spawn(ensure(model.clone(), tx));
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(ev) = rx.recv().await {
                let Some(this) = weak.upgrade() else { return };
                if this.generation.get() != generation {
                    return; // the model setting changed meanwhile
                }
                this.handle(&model, ev);
            }
        });
    }

    fn handle(&self, model: &str, ev: ModelEvent) {
        let name = pretty_model_name(model);
        match ev {
            ModelEvent::Progress {
                percent,
                total_bytes,
            } => {
                let p = percent.floor() as i32;
                if p == self.last_percent.get() {
                    return;
                }
                self.last_percent.set(p);
                let body = match total_bytes {
                    Some(t) if t > 0 => format!(
                        "{name} · {} of {}",
                        glib::format_size((t as f64 * percent / 100.0) as u64),
                        glib::format_size(t)
                    ),
                    _ => name.clone(),
                };
                (self.on_status)(Some(format!(
                    "{} {p}%",
                    i18n::umoja("Downloading the assistant's model…")
                )));
                if !self.progress_dismissed.get() {
                    self.notify(&i18n::umoja("Downloading AI model"), &body, Some(p));
                }
            }
            ModelEvent::Ready { downloaded } => {
                (self.on_status)(None);
                if downloaded {
                    self.notify(
                        &i18n::umoja("AI assistant ready"),
                        &format!(
                            "{name} · {}",
                            i18n::umoja("Ask anything from the Activities search")
                        ),
                        None,
                    );
                }
                tracing::info!(model, downloaded, "assistant model ready");
            }
            ModelEvent::Failed(e) => {
                tracing::warn!(model, error = %e, "model download failed");
                (self.on_status)(Some(i18n::umoja(
                    "The assistant's model couldn't be downloaded",
                )));
                self.notify(
                    &i18n::umoja("Couldn't download AI model"),
                    &format!("{name}: {e}"),
                    None,
                );
            }
        }
    }

    fn notify(&self, summary: &str, body: &str, value: Option<i32>) {
        let Ok(conn) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
            return;
        };
        let hints = glib::VariantDict::new(None);
        hints.insert("desktop-entry", "umoja-assistant");
        if let Some(v) = value {
            hints.insert("value", v);
        }
        let args = (
            i18n::umoja("AI Assistant"),
            self.notification_id.get(),
            "folder-download",
            summary,
            body,
            Vec::<String>::new(),
            hints.end(),
            -1i32,
        )
            .to_variant();
        match conn.call_sync(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
            "Notify",
            Some(&args),
            Some(glib::VariantTy::new("(u)").expect("type")),
            gio::DBusCallFlags::NONE,
            1000,
            gio::Cancellable::NONE,
        ) {
            Ok(r) => {
                if let Some((id,)) = r.get::<(u32,)>() {
                    self.notification_id.set(id);
                }
            }
            Err(e) => tracing::debug!(error = %e, "no notification server"),
        }
    }
}

fn online(m: &gio::NetworkMonitor) -> bool {
    m.is_network_available()
        && m.connectivity() == gio::NetworkConnectivity::Full
        && !m.is_network_metered()
}

fn current_model() -> String {
    gs::get_string(gs::UMOJA_SHELL, "lemonade-model", DEFAULT_MODEL)
}

/// "Qwen3-4B-Instruct-2507-GGUF" -> "Qwen3 4B Instruct 2507".
fn pretty_model_name(id: &str) -> String {
    id.trim_end_matches("-GGUF")
        .trim_end_matches("-ONNX")
        .replace(['-', '_'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_events() {
        let mut p = SseParser::default();
        let ev = |p: &mut SseParser, name: &str, data: &str| {
            assert_eq!(p.line(&format!("event: {name}\n")), None);
            p.line(&format!("data: {data}\n"))
        };
        // Recorded from lemond 11.9 pulling Qwen3-0.6B-GGUF (2 files).
        let t = r#""total_download_size":1000,"total_files":2"#;
        assert_eq!(
            ev(
                &mut p,
                "progress",
                &format!(
                    r#"{{"bytes_downloaded":0,"bytes_previously_downloaded":0,"bytes_total":900,"file_index":1,"percent":0,{t}}}"#
                )
            ),
            Some(PullEvent::Progress {
                percent: 0.0,
                total: 1000,
                fetching: false
            })
        );
        // Bogus 100% against a tiny total is ignored.
        assert_eq!(
            ev(
                &mut p,
                "progress",
                &format!(
                    r#"{{"bytes_downloaded":10,"bytes_previously_downloaded":0,"bytes_total":10,"file_index":1,"percent":100,{t}}}"#
                )
            ),
            None
        );
        assert_eq!(
            ev(
                &mut p,
                "progress",
                &format!(
                    r#"{{"bytes_downloaded":450,"bytes_previously_downloaded":0,"bytes_total":900,"file_index":1,"percent":50,{t}}}"#
                )
            ),
            Some(PullEvent::Progress {
                percent: 45.0,
                total: 1000,
                fetching: true
            })
        );
        assert_eq!(
            ev(
                &mut p,
                "progress",
                &format!(
                    r#"{{"bytes_downloaded":900,"bytes_previously_downloaded":0,"bytes_total":900,"file_index":1,"percent":100,{t}}}"#
                )
            ),
            Some(PullEvent::Progress {
                percent: 90.0,
                total: 1000,
                fetching: true
            })
        );
        assert_eq!(
            ev(
                &mut p,
                "progress",
                &format!(
                    r#"{{"bytes_downloaded":100,"bytes_previously_downloaded":0,"bytes_total":100,"file_index":2,"percent":100,{t}}}"#
                )
            ),
            Some(PullEvent::Progress {
                percent: 100.0,
                total: 1000,
                fetching: true
            })
        );
        assert_eq!(
            ev(
                &mut p,
                "complete",
                r#"{"bytes_downloaded":0,"bytes_total":0,"file":"","file_index":2,"percent":100,"total_download_size":0,"total_files":2}"#
            ),
            Some(PullEvent::Complete)
        );
        // Unknown models come back as an error event on HTTP 200.
        assert_eq!(
            ev(
                &mut p,
                "error",
                r#"{"code":"unknown_model","error":"must include the user namespace"}"#
            ),
            Some(PullEvent::Error("must include the user namespace".into()))
        );
    }

    #[test]
    fn cached_models_are_not_fetching() {
        let mut p = SseParser::default();
        p.line("event: progress");
        let e = p.line(r#"data: {"bytes_downloaded":2497281120,"bytes_previously_downloaded":2497281120,"bytes_total":2497281120,"file_index":1,"percent":100,"total_download_size":2497281120,"total_files":1}"#);
        assert_eq!(
            e,
            Some(PullEvent::Progress {
                percent: 100.0,
                total: 2497281120,
                fetching: false
            })
        );
    }

    #[test]
    fn model_names() {
        assert_eq!(
            pretty_model_name("Qwen3-4B-Instruct-2507-GGUF"),
            "Qwen3 4B Instruct 2507"
        );
    }
}

//! "Cloud" page of the assistant model picker: use an OpenAI-compatible
//! endpoint (server URL, API key, model) instead of the embedded Lemonade.
//!
//! The URL and model go to GSettings (`org.umoja.shell cloud-base-url`,
//! `cloud-model`); the key goes to the keyring. "Test" runs one streamed,
//! tool-enabled chat through the same client the assistant uses, so a pass
//! means questions and desktop actions will work too.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::{glib, prelude::*};
use gtk4 as gtk;
use serde_json::json;
use umoja_common::assistant_backend::{self as backend, Endpoint};
use umoja_common::{gsettings as gs, i18n};

use crate::lemonade::{ChatEvent, LemonadeClient, RUNTIME};

const TEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct CloudForm {
    pub page: gtk::Box,
    url: gtk::Entry,
    key: gtk::PasswordEntry,
    model: gtk::Entry,
    status: gtk::Label,
    test: gtk::Button,
    apply: gtk::Button,
    forget: gtk::Button,
    load_models: gtk::Button,
    models: gtk::ListBox,
    models_box: gtk::ScrolledWindow,
    /// A key is already in the keyring.
    has_key: Rc<Cell<bool>>,
}

impl CloudForm {
    pub fn new(popover: &gtk::Popover) -> Self {
        let note = gtk::Label::builder()
            .label(i18n::umoja(
                "Use a model from any OpenAI-compatible service. Your questions and anything \
                 you attach to them are sent to it. Notification summaries and file search \
                 stay on this computer.",
            ))
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(44)
            .css_classes(["dim-label", "caption"])
            .build();

        let url = gtk::Entry::builder()
            .placeholder_text("https://api.openai.com/v1")
            .input_purpose(gtk::InputPurpose::Url)
            .hexpand(true)
            .build();
        let key = gtk::PasswordEntry::builder()
            .show_peek_icon(true)
            .hexpand(true)
            .build();
        let model = gtk::Entry::builder()
            .placeholder_text(i18n::umoja("Type a name or load the list"))
            .hexpand(true)
            .build();
        let load_models = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text(i18n::umoja("Load the models this server offers"))
            .build();
        let model_row = gtk::Box::builder().css_classes(["linked"]).build();
        model_row.append(&model);
        model_row.append(&load_models);
        // The server's models, filtered by what's typed in the model field.
        let models = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["model-list"])
            .build();
        let models_box = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .min_content_height(160)
            .max_content_height(240)
            .propagate_natural_height(true)
            .visible(false)
            .child(&models)
            .build();

        let grid = gtk::Grid::builder()
            .row_spacing(6)
            .column_spacing(12)
            .margin_top(6)
            .build();
        for (row, (label, widget)) in [
            (i18n::umoja("Server URL"), url.upcast_ref::<gtk::Widget>()),
            (i18n::umoja("API Key"), key.upcast_ref()),
            (i18n::umoja("Model"), model_row.upcast_ref()),
        ]
        .into_iter()
        .enumerate()
        {
            let l = gtk::Label::builder().label(label).xalign(1.0).build();
            l.set_mnemonic_widget(Some(if row == 2 { model.upcast_ref() } else { widget }));
            grid.attach(&l, 0, row as i32, 1, 1);
            grid.attach(widget, 1, row as i32, 1, 1);
        }

        let status = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(44)
            .selectable(true)
            .visible(false)
            .css_classes(["caption"])
            .build();

        let forget = gtk::Button::builder()
            .label(i18n::umoja("Remove"))
            .tooltip_text(i18n::umoja(
                "Forget the API key and go back to the model on this computer",
            ))
            .css_classes(["destructive-action"])
            .build();
        let test = gtk::Button::with_label(&i18n::umoja("Test"));
        let apply = gtk::Button::builder()
            .label(i18n::umoja("Use Cloud Model"))
            .css_classes(["suggested-action"])
            .build();
        let spacer = gtk::Box::builder().hexpand(true).build();
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        buttons.set_margin_top(6);
        buttons.append(&forget);
        buttons.append(&spacer);
        buttons.append(&test);
        buttons.append(&apply);

        let page = gtk::Box::new(gtk::Orientation::Vertical, 6);
        page.append(&note);
        page.append(&grid);
        page.append(&models_box);
        page.append(&status);
        page.append(&buttons);

        let form = Self {
            page,
            url,
            key,
            model,
            status,
            test,
            apply,
            forget,
            load_models,
            models,
            models_box,
            has_key: Rc::new(Cell::new(false)),
        };

        let f = form.clone();
        form.load_models.connect_clicked(move |_| f.fetch_models());
        let f = form.clone();
        form.url.connect_activate(move |_| f.fetch_models());
        let m = form.model.clone();
        form.models.set_filter_func(move |row| {
            let q = m.text().trim().to_lowercase();
            q.is_empty() || row.widget_name().to_lowercase().contains(&q)
        });
        let l = form.models.clone();
        form.model.connect_changed(move |_| l.invalidate_filter());
        let m = form.model.clone();
        form.models.connect_row_activated(move |_, row| {
            m.set_text(&row.widget_name());
            m.set_position(-1);
        });

        let f = form.clone();
        form.test.connect_clicked(move |_| f.run_test());
        let f = form.clone();
        let p = popover.clone();
        form.apply.connect_clicked(move |_| f.apply(&p));
        let f = form.clone();
        form.forget.connect_clicked(move |_| f.forget());
        // Enter in the last field tests the settings.
        let f = form.clone();
        form.model.connect_activate(move |_| f.run_test());
        form
    }

    /// Fill the form from the saved settings (called when the popover opens).
    /// Unsaved typing is kept: going off to copy the API key from a browser
    /// closes the overlay, and the pasted key must survive reopening it.
    pub fn load(&self) {
        let (url, model) = backend::cloud_settings();
        if self.url.text().trim().is_empty() {
            self.url.set_text(&url);
        }
        if self.model.text().trim().is_empty() {
            self.model.set_text(&model);
        }
        if backend::cloud_selected() && backend::current().cloud {
            self.show_status(
                &i18n::umoja("In use: %s on %s.")
                    .replacen("%s", model.trim(), 1)
                    .replacen("%s", &backend::current().host(), 1),
                Some(true),
            );
        } else {
            self.status.set_visible(false);
        }
        self.forget.set_visible(backend::cloud_selected());
        self.set_key_hint(false);
        let form = self.clone();
        let (tx, rx) = async_channel::bounded(1);
        RUNTIME.spawn_blocking(move || {
            let _ = tx.send_blocking(backend::token::lookup().is_some());
        });
        glib::spawn_future_local(async move {
            let stored = rx.recv().await.unwrap_or(false);
            form.has_key.set(stored);
            form.set_key_hint(stored);
            if stored {
                form.forget.set_visible(true);
            }
        });
    }

    fn set_key_hint(&self, stored: bool) {
        self.key.set_property(
            "placeholder-text",
            if stored {
                i18n::umoja("Saved in your keyring")
            } else {
                i18n::umoja("Not needed for some local servers")
            },
        );
    }

    fn show_status(&self, text: &str, ok: Option<bool>) {
        self.status.set_text(text);
        self.status.set_visible(!text.is_empty());
        self.status.remove_css_class("error");
        self.status.remove_css_class("success");
        match ok {
            Some(true) => self.status.add_css_class("success"),
            Some(false) => self.status.add_css_class("error"),
            None => {}
        }
    }

    /// The entered URL and model, or a message about what's missing.
    fn entered(&self) -> Result<(String, String), String> {
        let url = self.url.text().trim().to_string();
        let model = self.model.text().trim().to_string();
        if !backend::valid_base_url(&url) {
            return Err(i18n::umoja(
                "Enter the server URL, starting with https:// (for example https://api.openai.com/v1).",
            ));
        }
        if model.is_empty() {
            return Err(i18n::umoja("Enter the name of the model to use."));
        }
        Ok((url, model))
    }

    fn set_busy(&self, busy: bool) {
        self.test.set_sensitive(!busy);
        self.apply.set_sensitive(!busy);
        self.forget.set_sensitive(!busy);
        self.load_models.set_sensitive(!busy);
    }

    /// Fetch the server's model list into the picker under the model field.
    fn fetch_models(&self) {
        let url = self.url.text().trim().to_string();
        if !backend::valid_base_url(&url) {
            return self.show_status(
                &i18n::umoja("Enter the server URL first, starting with https://."),
                Some(false),
            );
        }
        let typed = backend::clean_key(&self.key.text());
        self.set_busy(true);
        self.show_status(&i18n::umoja("Loading models…"), None);
        let (tx, rx) = async_channel::bounded(1);
        RUNTIME.spawn(async move {
            let key = if typed.is_empty() {
                tokio::task::spawn_blocking(backend::token::lookup)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default()
            } else {
                typed
            };
            let _ = tx.send(list_models(&url, &key).await).await;
        });
        let form = self.clone();
        glib::spawn_future_local(async move {
            form.set_busy(false);
            match rx.recv().await {
                Ok(Ok(models)) if models.is_empty() => form.show_status(
                    &i18n::umoja(
                        "The server didn't list any models. Type the model's name instead.",
                    ),
                    Some(false),
                ),
                Ok(Ok(models)) => {
                    form.fill_models(&models);
                    let tools = models.iter().filter(|m| m.tools == Some(true)).count();
                    let mut msg = i18n::umoja("%d models. Type in the model field to filter.")
                        .replacen("%d", &models.len().to_string(), 1);
                    if tools > 0 {
                        msg.push(' ');
                        msg.push_str(&i18n::umoja(
                            "Models marked “Tools” can change settings and open apps.",
                        ));
                    }
                    form.show_status(&msg, None);
                }
                Ok(Err(e)) => form.show_status(&e, Some(false)),
                Err(_) => {}
            }
        });
    }

    fn fill_models(&self, models: &[RemoteModel]) {
        while let Some(row) = self.models.first_child() {
            self.models.remove(&row);
        }
        for m in models {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.set_margin_top(4);
            row.set_margin_bottom(4);
            row.set_margin_start(6);
            row.set_margin_end(6);
            let text = gtk::Box::new(gtk::Orientation::Vertical, 1);
            text.set_hexpand(true);
            text.append(
                &gtk::Label::builder()
                    .label(&m.id)
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::Middle)
                    .build(),
            );
            let mut details = Vec::new();
            if let Some(n) = m.name.as_deref().filter(|n| *n != m.id) {
                details.push(n.to_string());
            }
            if let Some(c) = m.context {
                details.push(format!("{}k context", c / 1000));
            }
            if !details.is_empty() {
                text.append(
                    &gtk::Label::builder()
                        .label(details.join(" · "))
                        .xalign(0.0)
                        .ellipsize(gtk::pango::EllipsizeMode::End)
                        .css_classes(["dim-label", "caption"])
                        .build(),
                );
            }
            row.append(&text);
            if m.tools == Some(true) {
                row.append(
                    &gtk::Label::builder()
                        .label(i18n::umoja("Tools"))
                        .css_classes(["dim-label", "caption"])
                        .tooltip_text(i18n::umoja(
                            "Supports tool calling, which desktop actions need",
                        ))
                        .build(),
                );
            }
            let item = gtk::ListBoxRow::builder()
                .child(&row)
                .activatable(true)
                .name(&m.id)
                .build();
            self.models.append(&item);
        }
        self.models_box.set_visible(true);
        self.models.invalidate_filter();
    }

    fn run_test(&self) {
        let (url, model) = match self.entered() {
            Ok(v) => v,
            Err(e) => return self.show_status(&e, Some(false)),
        };
        let typed = backend::clean_key(&self.key.text());
        self.set_busy(true);
        self.show_status(&i18n::umoja("Testing…"), None);
        let ep = Endpoint::cloud(&url, &model);
        let (tx, rx) = async_channel::bounded(1);
        RUNTIME.spawn(async move {
            let key = if typed.is_empty() {
                tokio::task::spawn_blocking(backend::token::lookup)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default()
            } else {
                typed
            };
            let result = match tokio::time::timeout(TEST_TIMEOUT, test(ep, key)).await {
                Ok(r) => r,
                Err(_) => (false, "No answer within a minute. The server may be overloaded, or the URL may point at the wrong service.".into()),
            };
            let _ = tx.send(result).await;
        });
        let form = self.clone();
        glib::spawn_future_local(async move {
            if let Ok((ok, msg)) = rx.recv().await {
                form.show_status(&msg, Some(ok));
            }
            form.set_busy(false);
        });
    }

    fn apply(&self, popover: &gtk::Popover) {
        let (url, model) = match self.entered() {
            Ok(v) => v,
            Err(e) => return self.show_status(&e, Some(false)),
        };
        let typed = backend::clean_key(&self.key.text());
        self.set_busy(true);
        let (tx, rx) = async_channel::bounded(1);
        RUNTIME.spawn_blocking(move || {
            let r = if typed.is_empty() {
                Ok(())
            } else {
                backend::token::store(&typed)
            };
            let _ = tx.send_blocking(r);
        });
        let form = self.clone();
        let popover = popover.clone();
        glib::spawn_future_local(async move {
            form.set_busy(false);
            match rx.recv().await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    return form.show_status(
                        &i18n::umoja("Couldn't save the API key in your keyring: %s")
                            .replacen("%s", &e, 1),
                        Some(false),
                    );
                }
                Err(_) => return,
            }
            let Some(s) =
                gs::settings(gs::UMOJA_SHELL).filter(|s| gs::has_key(s, backend::BACKEND_KEY))
            else {
                // GIO aborts on keys an older installed schema doesn't have.
                return form.show_status(
                    &i18n::umoja(
                        "The installed settings schema is out of date. Reinstall Umoja, then try again.",
                    ),
                    Some(false),
                );
            };
            {
                let _ = s.set_string(backend::CLOUD_URL_KEY, &url);
                let _ = s.set_string(backend::CLOUD_MODEL_KEY, &model);
                let _ = s.set_string(backend::BACKEND_KEY, backend::CLOUD);
            }
            tracing::info!(%url, %model, "assistant switched to the cloud model");
            // Saved: the field goes back to "Saved in your keyring".
            if !form.key.text().trim().is_empty() {
                form.has_key.set(true);
                form.key.set_text("");
                form.set_key_hint(true);
            }
            popover.popdown();
        });
    }

    fn forget(&self) {
        self.set_busy(true);
        let (tx, rx) = async_channel::bounded(1);
        RUNTIME.spawn_blocking(move || {
            backend::token::clear();
            let _ = tx.send_blocking(());
        });
        let form = self.clone();
        glib::spawn_future_local(async move {
            let _ = rx.recv().await;
            if let Some(s) = gs::settings(gs::UMOJA_SHELL)
                && gs::has_key(&s, backend::BACKEND_KEY)
            {
                let _ = s.set_string(backend::BACKEND_KEY, backend::LOCAL);
            }
            form.has_key.set(false);
            form.key.set_text("");
            form.set_key_hint(false);
            form.forget.set_visible(false);
            form.set_busy(false);
            form.show_status(
                &i18n::umoja(
                    "Removed the API key. The assistant uses the model on this computer again.",
                ),
                Some(true),
            );
        });
    }
}

/// One entry of a server's `GET /models` list.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteModel {
    pub id: String,
    /// Display name (OpenRouter has one).
    pub name: Option<String>,
    /// Context window in tokens, when the server says.
    pub context: Option<u64>,
    /// Whether it accepts `tools`, when the server says (OpenRouter's
    /// `supported_parameters`). None means unknown.
    pub tools: Option<bool>,
}

/// Parse an OpenAI-style model list (`{"data": [{"id": …}, …]}`; a bare
/// array is accepted too), sorted by id. Embedding, image, audio and
/// moderation models are left out when the server labels them.
pub fn parse_models(v: &serde_json::Value) -> Vec<RemoteModel> {
    let items = v
        .get("data")
        .or_else(|| v.get("models"))
        .unwrap_or(v)
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut out: Vec<RemoteModel> = items
        .iter()
        .filter_map(|m| {
            let id = m.get("id").or_else(|| m.get("name"))?.as_str()?.to_string();
            // OpenRouter: architecture.output_modalities must include text.
            if let Some(out) = m
                .pointer("/architecture/output_modalities")
                .and_then(|o| o.as_array())
                && !out.iter().any(|o| o == "text")
            {
                return None;
            }
            // Others label the kind ("type": "embedding" / "image"...).
            if let Some(t) = m.get("type").and_then(|t| t.as_str())
                && !matches!(t, "chat" | "language" | "llm" | "model" | "text")
            {
                return None;
            }
            let tools = m
                .get("supported_parameters")
                .and_then(|p| p.as_array())
                .map(|p| p.iter().any(|x| x == "tools"));
            Some(RemoteModel {
                name: m.get("name").and_then(|n| n.as_str()).map(str::to_string),
                context: m
                    .get("context_length")
                    .or_else(|| m.get("context_window"))
                    .and_then(|c| c.as_u64()),
                tools,
                id,
            })
        })
        .collect();
    out.sort_by_cached_key(|m| m.id.to_lowercase());
    out.dedup_by(|a, b| a.id == b.id);
    out
}

async fn list_models(base_url: &str, key: &str) -> Result<Vec<RemoteModel>, String> {
    let ep = Endpoint::cloud(base_url, "");
    let mut req = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?
        .get(backend::models_url(base_url));
    if !key.is_empty() {
        req = req.bearer_auth(key);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| crate::lemonade::describe_cloud(&ep.host(), &e))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(match status.as_u16() {
            401 | 403 => backend::describe_status(&ep, status.as_u16(), &text),
            _ => format!(
                "{} didn't give a model list ({status}). Type the model's name instead.",
                ep.host()
            ),
        });
    }
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| format!("{} didn't answer like an OpenAI-compatible server. Check the URL (it usually ends in /v1).", ep.host()))?;
    Ok(parse_models(&v))
}

/// The tool the test asks the model to call.
fn ping_tool() -> serde_json::Value {
    json!({
        "type": "function",
        "function": {
            "name": "ping",
            "description": "Confirms that the connection works. Call it whenever you are asked to.",
            "parameters": {"type": "object", "properties": {}, "required": []},
        }
    })
}

/// One streamed, tool-enabled chat against `ep`. Returns (passed, message).
pub async fn test(ep: Endpoint, key: String) -> (bool, String) {
    let client = LemonadeClient::for_endpoint(ep.clone(), Some(key.clone()));
    let messages = vec![
        json!({"role": "system", "content": "You are checking a connection. When asked to call the ping tool, call it."}),
        json!({"role": "user", "content": "Call the ping tool now."}),
    ];
    let (tx, rx) = async_channel::unbounded();
    let t0 = Instant::now();
    client.stream_chat(messages, vec![ping_tool()], tx).await;
    let secs = t0.elapsed().as_secs_f32();
    let mut called = false;
    let mut answered = false;
    while let Ok(ev) = rx.try_recv() {
        match ev {
            ChatEvent::Text(t) => answered |= !t.trim().is_empty(),
            ChatEvent::ToolCalls(c) => called |= c.iter().any(|c| c.name == "ping"),
            ChatEvent::Done => {}
            ChatEvent::Error(e) if key.is_empty() && e.contains("rejected the API key") => {
                return (
                    false,
                    "No API key was sent: the API Key field is empty and none is saved in your \
                     keyring. Paste your key into API Key and press Test again."
                        .into(),
                );
            }
            ChatEvent::Error(e) => {
                let hint = model_hint(&ep, &key).await.unwrap_or_default();
                return (false, format!("{e}{hint}"));
            }
        }
    }
    if called {
        (
            true,
            format!(
                "It works. {} answered in {secs:.1} s and can use desktop tools.",
                ep.model
            ),
        )
    } else if answered {
        (
            true,
            format!(
                "Connected: {} answered in {secs:.1} s, but it didn't call a tool when asked, \
                 so the assistant may not be able to change settings or open apps with it.",
                ep.model
            ),
        )
    } else {
        (
            false,
            format!("{} accepted the request but sent back nothing.", ep.host()),
        )
    }
}

/// " Models it offers include: …" when the server lists models and the
/// configured one isn't among them.
async fn model_hint(ep: &Endpoint, key: &str) -> Option<String> {
    let base = ep.chat_url.trim_end_matches("/chat/completions");
    let ids: Vec<String> = list_models(base, key)
        .await
        .ok()?
        .into_iter()
        .map(|m| m.id)
        .collect();
    let hint = model_suggestions(&ep.model, &ids)?;
    Some(format!(" Models it offers include: {hint}."))
}

/// Up to six of `ids` to suggest instead of `model`, or None when `model`
/// is listed (so the problem is something else) or the list is empty.
fn model_suggestions(model: &str, ids: &[String]) -> Option<String> {
    if ids.is_empty() || ids.iter().any(|i| i == model) {
        return None;
    }
    let lower = model.to_lowercase();
    let mut picks: Vec<&String> = ids
        .iter()
        .filter(|i| {
            let i = i.to_lowercase();
            !lower.is_empty() && (i.contains(&lower) || lower.contains(&i))
        })
        .collect();
    for i in ids {
        if picks.len() >= 6 {
            break;
        }
        if !picks.contains(&i) {
            picks.push(i);
        }
    }
    picks.truncate(6);
    Some(
        picks
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions() {
        let ids: Vec<String> = ["gpt-4.1", "gpt-4.1-mini", "o3", "text-embedding-3"]
            .map(String::from)
            .to_vec();
        assert_eq!(model_suggestions("gpt-4.1", &ids), None);
        assert_eq!(
            model_suggestions("gpt-4.1-min", &ids).unwrap(),
            "gpt-4.1, gpt-4.1-mini, o3, text-embedding-3"
        );
        assert_eq!(model_suggestions("x", &[]), None);
    }

    /// Runs the settings page's model list and "Test" against a real
    /// server. Defaults to the embedded Lemonade posing as a cloud
    /// endpoint; set UMOJA_CLOUD_URL, UMOJA_CLOUD_MODEL and UMOJA_CLOUD_KEY
    /// for another one (e.g. https://openrouter.ai/api/v1). Run with
    /// `cargo test -p umoja-overlay live_cloud -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_cloud() {
        let url = std::env::var("UMOJA_CLOUD_URL")
            .unwrap_or_else(|_| format!("{}/api/v1", umoja_common::lemonade::base_url()));
        let model = std::env::var("UMOJA_CLOUD_MODEL")
            .unwrap_or_else(|_| umoja_common::lemonade::DEFAULT_MODEL.into());
        let key = std::env::var("UMOJA_CLOUD_KEY").unwrap_or_default();
        RUNTIME.block_on(async {
            let models = list_models(&url, &key).await.expect("model list");
            println!(
                "{} models, {} with tools",
                models.len(),
                models.iter().filter(|m| m.tools == Some(true)).count()
            );
            let (ok, msg) = test(Endpoint::cloud(&url, &model), key.clone()).await;
            println!("test: {ok} {msg}");
            assert!(ok, "{msg}");
            let (ok, msg) = test(Endpoint::cloud(&url, "no-such-model-xyz"), key.clone()).await;
            println!("bad model: {ok} {msg}");
            assert!(!ok);
        });
    }

    #[test]
    fn parses_openrouter_and_openai_lists() {
        let openrouter = json!({"data": [
            {"id": "openai/gpt-4.1-mini", "name": "OpenAI: GPT-4.1 Mini", "context_length": 1047576,
             "architecture": {"output_modalities": ["text"]},
             "supported_parameters": ["tools", "tool_choice", "temperature"]},
            {"id": "anthropic/claude-x", "name": "Anthropic: Claude X", "context_length": 200000,
             "architecture": {"output_modalities": ["text"]},
             "supported_parameters": ["temperature"]},
            {"id": "acme/image-gen", "architecture": {"output_modalities": ["image"]}}
        ]});
        let m = parse_models(&openrouter);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].id, "anthropic/claude-x");
        assert_eq!(m[0].tools, Some(false));
        assert_eq!(m[1].tools, Some(true));
        assert_eq!(m[1].context, Some(1047576));

        let openai = json!({"object": "list", "data": [
            {"id": "gpt-4.1", "object": "model", "owned_by": "openai"},
            {"id": "Gpt-3.5", "object": "model"}
        ]});
        let m = parse_models(&openai);
        assert_eq!(
            m.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["Gpt-3.5", "gpt-4.1"]
        );
        assert_eq!(m[0].tools, None);

        let typed =
            json!({"data": [{"id": "e5", "type": "embedding"}, {"id": "llama", "type": "chat"}]});
        assert_eq!(parse_models(&typed).len(), 1);
    }
}

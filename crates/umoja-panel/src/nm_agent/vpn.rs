//! VPN secrets come from the VPN plugin's own auth dialog
//! (`nm-openvpn-auth-dialog` and friends), run as gnome-shell's
//! VPNRequestHandler runs it: the connection goes in on stdin, secrets
//! come out on stdout. "External UI" plugins only describe what they
//! need and we ask in our own card; older ones show their own window.

use super::content::{Conn, Content, Secret, Validate};
use gio::prelude::*;
use gtk4::glib;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

const VPN_UI_GROUP: &str = "VPN Plugin UI";

pub enum Event {
    Secret(String, String),
    /// An external-UI plugin wants these asked in our dialog.
    Ask(Content),
    /// The helper finished: true when it gave its answer.
    Done(bool),
    Failed,
}

struct Plugin {
    auth_dialog: PathBuf,
    supports_hints: bool,
    external_ui: bool,
}

fn find_plugin(service: &str) -> Option<Plugin> {
    for dir in ["/etc/NetworkManager/VPN", "/usr/lib/NetworkManager/VPN"] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("name") {
                continue;
            }
            let kf = glib::KeyFile::new();
            if kf.load_from_file(&path, glib::KeyFileFlags::NONE).is_err() {
                continue;
            }
            let matches = kf
                .string("VPN Connector", "service")
                .is_ok_and(|s| s == service)
                || kf
                    .string_list("VPN Connector", "aliases")
                    .is_ok_and(|a| a.iter().any(|s| s == service));
            if !matches {
                continue;
            }
            let Ok(dialog) = kf.string("GNOME", "auth-dialog") else {
                continue;
            };
            let mut auth_dialog = PathBuf::from(dialog.as_str());
            if auth_dialog.is_relative() {
                auth_dialog = Path::new("/usr/libexec").join(auth_dialog);
            }
            let truthy = |k: &str| {
                kf.string("GNOME", k).is_ok_and(|v| {
                    matches!(
                        v.trim().to_lowercase().as_str(),
                        "true" | "yes" | "on" | "1"
                    )
                })
            };
            return Some(Plugin {
                auth_dialog,
                supports_hints: truthy("supports-hints"),
                external_ui: truthy("supports-external-ui-mode"),
            });
        }
    }
    None
}

pub struct Helper {
    process: gio::Subprocess,
    external_ui: bool,
    finished: Cell<bool>,
}

impl Helper {
    pub fn start(
        conn: &Conn,
        hints: &[String],
        flags: u32,
        known: &HashMap<String, String>,
        on_event: impl Fn(Event) + 'static,
    ) -> Option<Rc<Self>> {
        let service = conn.str("vpn", "service-type")?;
        let Some(plugin) = find_plugin(&service) else {
            tracing::warn!(service, "no VPN auth dialog for this service");
            return None;
        };
        let mut argv: Vec<std::ffi::OsString> = vec![
            plugin.auth_dialog.clone().into(),
            "-u".into(),
            conn.uuid().into(),
            "-n".into(),
            conn.id().into(),
            "-s".into(),
            service.clone().into(),
        ];
        if plugin.external_ui {
            argv.push("--external-ui-mode".into());
        }
        if flags & super::ALLOW_INTERACTION != 0 {
            argv.push("-i".into());
        }
        if flags & super::REQUEST_NEW != 0 {
            argv.push("-r".into());
        }
        if plugin.supports_hints {
            for h in hints {
                argv.push("-t".into());
                argv.push(h.into());
            }
        }
        let args: Vec<&std::ffi::OsStr> = argv.iter().map(|a| a.as_os_str()).collect();
        let process = gio::Subprocess::newv(
            &args,
            gio::SubprocessFlags::STDIN_PIPE
                | gio::SubprocessFlags::STDOUT_PIPE
                | gio::SubprocessFlags::STDERR_SILENCE,
        )
        .inspect_err(|e| tracing::warn!(error = %e, "could not start the VPN auth dialog"))
        .ok()?;

        // The connection, in the helpers' line protocol.
        let mut input = String::new();
        for (k, v) in conn.dict("vpn", "data") {
            input.push_str(&format!("DATA_KEY={k}\nDATA_VAL={v}\n\n"));
        }
        let mut secrets: HashMap<String, String> =
            conn.dict("vpn", "secrets").into_iter().collect();
        secrets.extend(known.clone());
        for (k, v) in &secrets {
            input.push_str(&format!("SECRET_KEY={k}\nSECRET_VAL={v}\n\n"));
        }
        input.push_str("DONE\n\n");
        let helper = Rc::new(Self {
            process,
            external_ui: plugin.external_ui,
            finished: Cell::new(false),
        });
        if helper.write(&input).is_err() {
            helper.process.force_exit();
            return None;
        }

        let on_event = Rc::new(on_event);
        let h = helper.clone();
        glib::spawn_future_local(async move { h.run(on_event).await });
        Some(helper)
    }

    fn write(&self, text: &str) -> Result<(), glib::Error> {
        let Some(stdin) = self.process.stdin_pipe() else {
            return Ok(());
        };
        stdin
            .write_all(text.as_bytes(), gio::Cancellable::NONE)
            .map(|_| ())
    }

    async fn run(self: Rc<Self>, on_event: Rc<dyn Fn(Event)>) {
        let Some(stdout) = self.process.stdout_pipe() else {
            on_event(Event::Failed);
            return;
        };
        let lines = gio::DataInputStream::new(&stdout);
        if self.external_ui {
            // A key file describing what to ask, then EOF.
            let mut out = Vec::new();
            loop {
                match lines.read_bytes_future(4096, glib::Priority::DEFAULT).await {
                    Ok(b) if !b.is_empty() => out.extend_from_slice(&b),
                    _ => break,
                }
            }
            let _ = self.process.wait_future().await;
            if self.finished.replace(true) {
                return;
            }
            match parse_external(&out) {
                Ok((content, given)) => {
                    for (k, v) in given {
                        on_event(Event::Secret(k, v));
                    }
                    match content {
                        Some(c) => on_event(Event::Ask(c)),
                        None => on_event(Event::Done(true)),
                    }
                }
                Err(()) => on_event(Event::Failed),
            }
            return;
        }
        // Old style: "key\nvalue\n" pairs, then an empty pair.
        let previous: RefCell<Option<String>> = RefCell::new(None);
        loop {
            let line = match lines.read_line_utf8_future(glib::Priority::DEFAULT).await {
                Ok(Some(l)) => l.to_string(),
                _ => break,
            };
            let prev = previous.borrow_mut().take();
            match prev {
                Some(p) if line.is_empty() && p.is_empty() => {
                    let _ = self.write("QUIT\n\n");
                }
                Some(key) => on_event(Event::Secret(key, line)),
                None => *previous.borrow_mut() = Some(line),
            }
        }
        let ok = self.process.wait_future().await.is_ok()
            && self.process.has_exited()
            && self.process.exit_status() == 0;
        if !self.finished.replace(true) {
            on_event(Event::Done(ok));
        }
    }

    /// The request was withdrawn or answered: tell the helper to go.
    pub fn cancel(&self) {
        if self.finished.replace(true) {
            return;
        }
        if !self.external_ui {
            let _ = self.write("QUIT\n\n");
        }
        if let Some(stdin) = self.process.stdin_pipe() {
            let _ = stdin.close(gio::Cancellable::NONE);
        }
    }
}

/// gnome-shell's _showNewStyleDialog(): Version=2 key file; each other
/// group is a secret, asked when ShouldAsk, else given as is.
fn parse_external(data: &[u8]) -> Result<(Option<Content>, Vec<(String, String)>), ()> {
    if data.is_empty() {
        return Ok((None, vec![])); // everything was stored already
    }
    let kf = glib::KeyFile::new();
    kf.load_from_bytes(&glib::Bytes::from(data), glib::KeyFileFlags::NONE)
        .map_err(|_| ())?;
    if kf.integer(VPN_UI_GROUP, "Version").map_err(|_| ())? != 2 {
        return Err(());
    }
    let mut content = Content {
        title: kf
            .string(VPN_UI_GROUP, "Title")
            .map(|s| s.to_string())
            .unwrap_or_default(),
        message: kf
            .string(VPN_UI_GROUP, "Description")
            .ok()
            .map(|s| s.to_string()),
        ..Default::default()
    };
    let mut given = Vec::new();
    for group in kf.groups().iter() {
        let group = group.as_str();
        if group == VPN_UI_GROUP {
            continue;
        }
        let value = kf
            .string(group, "Value")
            .map(|s| s.to_string())
            .unwrap_or_default();
        if kf.boolean(group, "ShouldAsk").unwrap_or(false) {
            content.secrets.push(Secret {
                label: kf
                    .string(group, "Label")
                    .map(|s| s.to_string())
                    .unwrap_or_else(|_| group.into()),
                key: Some(group.into()),
                value,
                password: kf.boolean(group, "IsSecret").unwrap_or(false),
                validate: Validate::NonEmpty,
            });
        } else if !value.is_empty() {
            given.push((group.to_string(), value));
        }
    }
    Ok(((!content.secrets.is_empty()).then_some(content), given))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_ui_keyfile() {
        let data = b"[VPN Plugin UI]\nVersion=2\nTitle=Authenticate VPN\nDescription=Work\n\n[password]\nValue=\nLabel=Password:\nIsSecret=true\nShouldAsk=true\n\n[cert-pass]\nValue=stored\nLabel=Cert\nIsSecret=true\nShouldAsk=false\n";
        let (content, given) = parse_external(data).unwrap();
        let c = content.unwrap();
        assert_eq!(c.title, "Authenticate VPN");
        assert_eq!(c.secrets.len(), 1);
        assert_eq!(c.secrets[0].key.as_deref(), Some("password"));
        assert!(c.secrets[0].password);
        assert_eq!(given, vec![("cert-pass".to_string(), "stored".to_string())]);
        assert!(parse_external(b"").unwrap().0.is_none());
    }
}

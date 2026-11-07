//! Myna dictation.
//!
//! Connects to the myna-desktop daemon's socket (`/tmp/myna.sock`) and reads
//! newline-delimited JSON status messages:
//!
//! ```json
//! {"state": "listening"}            // or "idle"
//! {"partial": "how do i set"}       // live transcription
//! {"final": "how do i set up vpn"}  // finished utterance
//! ```
//!
//! A bare text line is treated as a final transcript. Toggling sends
//! `toggle\n` on the socket, or falls back to calling `Toggle` on the
//! `org.canonical.Myna` D-Bus interface.
//!
//! While the overlay is open, transcripts go into the search box. Otherwise
//! myna-desktop types them into the focused app itself (via the
//! compositor's virtual-keyboard / input-method protocols).

use crate::lemonade::RUNTIME;
use serde::Deserialize;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use umoja_common::dbus;

#[derive(Debug, Clone, PartialEq)]
pub enum MynaEvent {
    Listening(bool),
    Partial(String),
    Final(String),
    Available(bool),
}

#[derive(Deserialize)]
struct Wire {
    state: Option<String>,
    partial: Option<String>,
    #[serde(rename = "final")]
    final_: Option<String>,
}

pub struct Myna {
    pub events: async_channel::Receiver<MynaEvent>,
    toggle: tokio::sync::mpsc::UnboundedSender<()>,
}

impl Myna {
    /// Connect (and keep reconnecting) in the background.
    pub fn start() -> Self {
        let (ev_tx, ev_rx) = async_channel::unbounded();
        let (tg_tx, mut tg_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        RUNTIME.spawn(async move {
            loop {
                match UnixStream::connect(dbus::MYNA_SOCKET_PATH).await {
                    Ok(stream) => {
                        let _ = ev_tx.send(MynaEvent::Available(true)).await;
                        let (read, mut write) = stream.into_split();
                        let mut lines = BufReader::new(read).lines();
                        loop {
                            tokio::select! {
                                line = lines.next_line() => match line {
                                    Ok(Some(l)) => if let Some(ev) = parse(&l)
                                        && ev_tx.send(ev).await.is_err() { return; },
                                    _ => break,
                                },
                                Some(()) = tg_rx.recv() => {
                                    if write.write_all(b"toggle\n").await.is_err() { break; }
                                }
                            }
                        }
                        let _ = ev_tx.send(MynaEvent::Available(false)).await;
                    }
                    Err(_) => {
                        // Daemon not running: honour toggles over D-Bus instead.
                        while let Ok(()) = tg_rx.try_recv() {
                            glib::MainContext::default().invoke(toggle_over_dbus);
                        }
                        if ev_tx.is_closed() {
                            return;
                        }
                        tokio::time::sleep(Duration::from_secs(3)).await;
                    }
                }
            }
        });
        Self {
            events: ev_rx,
            toggle: tg_tx,
        }
    }

    pub fn toggle(&self) {
        let _ = self.toggle.send(());
    }
}

fn toggle_over_dbus() {
    gio::bus_get(gio::BusType::Session, gio::Cancellable::NONE, |conn| {
        let Ok(conn) = conn else { return };
        conn.call(
            Some(dbus::MYNA_BUS_NAME),
            dbus::MYNA_OBJECT_PATH,
            dbus::MYNA_BUS_NAME,
            "Toggle",
            None,
            None,
            gio::DBusCallFlags::NONE,
            1000,
            gio::Cancellable::NONE,
            |r| {
                if let Err(e) = r {
                    tracing::warn!(error = %e, "myna is not available");
                }
            },
        );
    });
}

fn parse(line: &str) -> Option<MynaEvent> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    match serde_json::from_str::<Wire>(line) {
        Ok(w) => {
            if let Some(f) = w.final_ {
                Some(MynaEvent::Final(f))
            } else if let Some(p) = w.partial {
                Some(MynaEvent::Partial(p))
            } else {
                w.state.map(|s| MynaEvent::Listening(s == "listening"))
            }
        }
        Err(_) => Some(MynaEvent::Final(line.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format() {
        assert_eq!(
            parse(r#"{"state":"listening"}"#),
            Some(MynaEvent::Listening(true))
        );
        assert_eq!(
            parse(r#"{"partial":"how do"}"#),
            Some(MynaEvent::Partial("how do".into()))
        );
        assert_eq!(
            parse(r#"{"final":"how do i"}"#),
            Some(MynaEvent::Final("how do i".into()))
        );
        assert_eq!(
            parse("plain words"),
            Some(MynaEvent::Final("plain words".into()))
        );
        assert_eq!(parse("  "), None);
    }
}

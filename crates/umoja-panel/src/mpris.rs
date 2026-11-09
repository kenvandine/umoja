//! Playback keys over MPRIS, like gnome-settings-daemon's media keys:
//! the key goes to the player that is playing, else one that is paused,
//! else any player on the bus.

use gio::prelude::*;
use gtk4::glib;

const PREFIX: &str = "org.mpris.MediaPlayer2.";
const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";

async fn session() -> Option<gio::DBusConnection> {
    gio::bus_get_future(gio::BusType::Session).await.ok()
}

/// (bus name, PlaybackStatus) for every player.
async fn players(conn: &gio::DBusConnection) -> Vec<(String, String)> {
    let Ok(names) = conn
        .call_future(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "ListNames",
            None,
            Some(glib::VariantTy::new("(as)").unwrap()),
            gio::DBusCallFlags::NONE,
            1000,
        )
        .await
    else {
        return Vec::new();
    };
    let names: Vec<String> = names
        .get::<(Vec<String>,)>()
        .map(|(n,)| n)
        .unwrap_or_default()
        .into_iter()
        .filter(|n| n.starts_with(PREFIX))
        .collect();
    let mut out = Vec::new();
    for name in names {
        let status = conn
            .call_future(
                Some(&name),
                PATH,
                "org.freedesktop.DBus.Properties",
                "Get",
                Some(&(PLAYER, "PlaybackStatus").to_variant()),
                Some(glib::VariantTy::new("(v)").unwrap()),
                gio::DBusCallFlags::NONE,
                500,
            )
            .await
            .ok()
            .and_then(|v| v.child_value(0).as_variant())
            .and_then(|v| v.str().map(String::from))
            .unwrap_or_default();
        out.push((name, status));
    }
    out
}

/// Which player a key goes to.
pub fn pick(players: &[(String, String)]) -> Option<&str> {
    ["Playing", "Paused"]
        .iter()
        .find_map(|s| players.iter().find(|(_, st)| st == s))
        .or(players.first())
        .map(|(n, _)| n.as_str())
}

/// Send a Player method (PlayPause, Pause, Stop, Previous, Next).
pub fn send(method: &'static str) {
    glib::spawn_future_local(async move {
        let Some(conn) = session().await else { return };
        let players = players(&conn).await;
        let Some(name) = pick(&players) else {
            tracing::debug!(method, "no media player");
            return;
        };
        if let Err(e) = conn
            .call_future(Some(name), PATH, PLAYER, method, None, None, gio::DBusCallFlags::NONE, 1000)
            .await
        {
            tracing::debug!(player = name, method, error = %e, "MPRIS call failed");
        }
    });
}

/// Calls `f` with whether any player is playing.
pub fn any_playing(f: impl FnOnce(bool) + 'static) {
    glib::spawn_future_local(async move {
        let playing = match session().await {
            Some(conn) => players(&conn).await.iter().any(|(_, s)| s == "Playing"),
            None => false,
        };
        f(playing);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_playing_then_paused() {
        let p = |v: &[(&str, &str)]| -> Vec<(String, String)> {
            v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
        };
        let list = p(&[("a", "Stopped"), ("b", "Paused"), ("c", "Playing")]);
        assert_eq!(pick(&list), Some("c"));
        let list = p(&[("a", "Stopped"), ("b", "Paused")]);
        assert_eq!(pick(&list), Some("b"));
        let list = p(&[("a", "Stopped")]);
        assert_eq!(pick(&list), Some("a"));
        assert_eq!(pick(&[]), None);
    }
}

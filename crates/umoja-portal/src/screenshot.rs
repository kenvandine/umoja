//! `org.freedesktop.impl.portal.Screenshot`.
//!
//! umoja-mir exposes `zwlr_screencopy_manager_v1`, so captures are taken
//! with `grim` straight from the compositor's buffers and saved under
//! `~/Pictures/Screenshots`, like GNOME.
//! TODO(phase 4): interactive area selection (`slurp`) and a preview dialog.

use crate::{RESPONSE_OTHER, RESPONSE_SUCCESS};
use std::collections::HashMap;
use zbus::interface;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};

pub struct Screenshot;

fn screenshot_path() -> std::path::PathBuf {
    let dir = glib::user_special_dir(glib::UserDirectory::Pictures)
        .unwrap_or_else(|| glib::home_dir().join("Pictures"))
        .join("Screenshots");
    let stamp = glib::DateTime::now_local()
        .and_then(|d| d.format("%Y-%m-%d %H-%M-%S"))
        .map(|s| s.to_string())
        .unwrap_or_else(|_| "screenshot".into());
    dir.join(format!("Screenshot From {stamp}.png"))
}

#[interface(name = "org.freedesktop.impl.portal.Screenshot")]
impl Screenshot {
    async fn screenshot(
        &self,
        _handle: ObjectPath<'_>,
        app_id: &str,
        _parent_window: &str,
        _options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let path = screenshot_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        tracing::info!(app_id, path = %path.display(), "screenshot");
        let status = tokio::process::Command::new("grim")
            .arg(&path)
            .status()
            .await;
        let mut results = HashMap::new();
        match status {
            Ok(s) if s.success() => {
                let uri = glib::filename_to_uri(&path, None)
                    .map(|u| u.to_string())
                    .unwrap_or_default();
                if let Ok(v) = OwnedValue::try_from(Value::from(uri)) {
                    results.insert("uri".to_string(), v);
                }
                (RESPONSE_SUCCESS, results)
            }
            other => {
                tracing::warn!(?other, "grim failed");
                (RESPONSE_OTHER, results)
            }
        }
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }
}

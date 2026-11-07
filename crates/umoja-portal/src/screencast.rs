//! `org.freedesktop.impl.portal.ScreenCast`.
//!
//! TODO(phase 4): create PipeWire streams fed from Mir's screencopy buffers.
//! Until then every request is answered with "other error" so apps fail
//! cleanly instead of hanging.

use crate::RESPONSE_OTHER;
use std::collections::HashMap;
use zbus::interface;
use zbus::zvariant::{ObjectPath, OwnedValue};

pub struct ScreenCast;

type Results = (u32, HashMap<String, OwnedValue>);

#[interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCast {
    async fn create_session(
        &self,
        _handle: ObjectPath<'_>,
        _session_handle: ObjectPath<'_>,
        _app_id: &str,
        _options: HashMap<String, OwnedValue>,
    ) -> Results {
        (RESPONSE_OTHER, HashMap::new())
    }

    async fn select_sources(
        &self,
        _handle: ObjectPath<'_>,
        _session_handle: ObjectPath<'_>,
        _app_id: &str,
        _options: HashMap<String, OwnedValue>,
    ) -> Results {
        (RESPONSE_OTHER, HashMap::new())
    }

    async fn start(
        &self,
        _handle: ObjectPath<'_>,
        _session_handle: ObjectPath<'_>,
        _app_id: &str,
        _parent_window: &str,
        _options: HashMap<String, OwnedValue>,
    ) -> Results {
        (RESPONSE_OTHER, HashMap::new())
    }

    #[zbus(property)]
    fn available_source_types(&self) -> u32 {
        1 // MONITOR
    }

    #[zbus(property)]
    fn available_cursor_modes(&self) -> u32 {
        1 // HIDDEN
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        4
    }
}

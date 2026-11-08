//! The Lemonade assistant: a tool-calling loop ([`agent`]) over a registry
//! of tools ([`tools`]) that other modules contribute to.

pub mod agent;
pub mod desktop;
pub mod notifications;
pub mod settings;
pub mod tools;

use std::cell::Cell;

thread_local! {
    static HIDE_WHEN_DONE: Cell<bool> = const { Cell::new(false) };
}

/// Ask for the overlay to close once the current answer is finished, so
/// the user sees what a tool did (an app opened, a window tiled). Hiding
/// straight away would cancel the rest of the answer.
pub fn hide_when_done() {
    HIDE_WHEN_DONE.with(|h| h.set(true));
}

/// Read and clear the flag set by [`hide_when_done`].
pub fn take_hide_request() -> bool {
    HIDE_WHEN_DONE.with(|h| h.replace(false))
}

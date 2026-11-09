//! The assistant: a tool-calling loop ([`agent`]) over the desktop tools
//! served by umoja-mcp ([`mcp`]) and the overlay's own ([`tools`]).

pub mod agent;
pub mod capture;
pub mod diagnostics;
pub mod mcp;
pub mod screen;
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

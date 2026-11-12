//! OS backends: capture the local mouse and keyboard, inject remote input.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc::UnboundedSender;

use crate::layout::{Rect, Side};
use crate::protocol::Button;
use crate::session::Input;

#[cfg(all(unix, not(target_os = "macos")))]
mod wayland;
#[cfg(all(unix, not(target_os = "macos")))]
mod x11;

/// Linux and other Unix desktops: Wayland when the session is Wayland (the
/// X11 backend would only reach XWayland windows there), else X11.
/// `INTERDESK_BACKEND=x11` or `=wayland` forces one.
#[cfg(all(unix, not(target_os = "macos")))]
mod native_impl {
    use super::*;

    pub fn start(events: UnboundedSender<Input>) -> Result<Arc<dyn Platform>> {
        let wayland = match std::env::var("INTERDESK_BACKEND").as_deref() {
            Ok("x11") => false,
            Ok("wayland") => true,
            _ => super::wayland::is_session(),
        };
        if wayland {
            super::wayland::start(events)
        } else {
            super::x11::start(events)
        }
    }
}

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as native_impl;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as native_impl;

/// Everything interdesk needs from the operating system.
pub trait Platform: Send + Sync {
    /// Bounding box of all monitors, in the coordinates used by `warp` and `move_to`.
    fn screen(&self) -> Rect;

    /// `true`: swallow local mouse and keyboard input and report it as
    /// [`Input::Delta`], buttons, wheel and keys. `false`: leave input alone
    /// and report the cursor position as [`Input::Pos`].
    ///
    /// Returns `false` when the input could not be captured (another
    /// application holds it): nothing is captured then, and the cursor must
    /// stay on this screen.
    fn set_grab(&self, grab: bool) -> bool;

    /// Moves the local cursor (source side).
    fn warp(&self, x: i32, y: i32);

    /// The edges of this screen that lead to a neighbour, given once before
    /// any input. Backends that must ask the system to watch the edges
    /// (Wayland) need it; the others watch the cursor everywhere.
    fn set_edges(&self, _sides: &[Side]) {}

    // Injection (target side). `x`, `y` are relative to `screen()`'s origin.
    fn move_to(&self, x: i32, y: i32);
    fn button(&self, button: Button, down: bool);
    fn wheel(&self, dx: i32, dy: i32);
    fn key(&self, hid: u16, down: bool);

    /// `true` when the OS repeats held keys by itself (X11 does, even for
    /// injected keys), so repeated key-downs from the source are dropped.
    fn repeats_keys(&self) -> bool {
        false
    }
}

/// Starts the backend for this OS. Input events are sent to `events`.
pub fn native(events: UnboundedSender<Input>) -> Result<Arc<dyn Platform>> {
    native_impl::start(events)
}

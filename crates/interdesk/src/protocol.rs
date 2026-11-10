//! Messages on an `interdesk/input` channel.
//!
//! The device that owns the physical mouse and keyboard (the *source*) opens
//! the channel to a neighbour (the *target*). The target first sends
//! `Screen`, then the source drives it; the target sends `Screen` again
//! whenever its desktop changes size. Positions are absolute, in the
//! target's own pixels, relative to the top-left corner of its desktop.
//! Keys are USB HID usage codes, so both machines must use the same keyboard
//! layout setting.

use serde::{Deserialize, Serialize};

/// Service name registered by interdesk with the crossdrop daemon.
pub const SERVICE: &str = "interdesk/input";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Msg {
    /// Target → source: size of the target's desktop. First message, and
    /// again whenever it changes.
    Screen {
        width: u32,
        height: u32,
    },
    /// The cursor arrives on the target at this position.
    Enter {
        x: i32,
        y: i32,
    },
    Move {
        x: i32,
        y: i32,
    },
    Button {
        button: Button,
        down: bool,
    },
    /// In 1/120 of a wheel notch (Windows' unit); positive = up / right.
    Wheel {
        dx: i32,
        dy: i32,
    },
    Key {
        hid: u16,
        down: bool,
    },
    /// The cursor went back to the source: release everything.
    Leave,
}

/// One wheel notch in [`Msg::Wheel`] units.
pub const WHEEL_NOTCH: i32 = 120;

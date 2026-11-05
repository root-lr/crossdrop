//! interdesk: one mouse and keyboard for all the computers on your desk.
//!
//! An extension of [crossdrop](https://github.com/root-lr/crossdrop): it uses
//! only the public `crossdrop-sdk`, so pairing, encryption and permissions
//! come from the crossdrop daemon.

pub mod keymap;
pub mod layout;
pub mod platform;
pub mod protocol;
pub mod runtime;
pub mod session;
pub mod target;

use std::path::PathBuf;

/// Location of the layout file in the crossdrop data directory.
pub fn layout_path(paths: &crossdrop_sdk::Paths) -> PathBuf {
    paths.home.join("interdesk.toml")
}

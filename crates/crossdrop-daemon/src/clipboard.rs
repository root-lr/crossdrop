//! Access to the system clipboard.
//!
//! The OS clipboard APIs are blocking and, on X11, the process that last set
//! the clipboard must keep serving it. So one dedicated thread owns the
//! clipboard handle for the daemon's whole life.

use std::sync::mpsc;
use std::thread;

use tokio::sync::oneshot;
use tracing::debug;

enum Cmd {
    Get(oneshot::Sender<Option<String>>),
    Set(String),
}

/// Handle to the clipboard thread.
#[derive(Clone)]
pub struct SystemClipboard {
    tx: mpsc::Sender<Cmd>,
}

impl SystemClipboard {
    /// Starts the clipboard thread. `None` when the system has no clipboard
    /// (no display on Linux, for instance).
    pub fn start() -> Option<Self> {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let (ready_tx, ready_rx) = mpsc::channel::<bool>();
        thread::Builder::new()
            .name("crossdrop-clipboard".into())
            .spawn(move || {
                let mut clipboard = match arboard::Clipboard::new() {
                    Ok(c) => {
                        let _ = ready_tx.send(true);
                        c
                    }
                    Err(e) => {
                        debug!("system clipboard unavailable: {e}");
                        let _ = ready_tx.send(false);
                        return;
                    }
                };
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        Cmd::Get(reply) => {
                            let text = if is_sensitive() {
                                debug!("clipboard marked sensitive by its owner; not shared");
                                None
                            } else {
                                clipboard.get_text().ok()
                            };
                            let _ = reply.send(text);
                        }
                        Cmd::Set(text) => {
                            if let Err(e) = clipboard.set_text(text) {
                                debug!("cannot set clipboard: {e}");
                            }
                        }
                    }
                }
            })
            .ok()?;
        ready_rx.recv().ok().filter(|ok| *ok).map(|_| Self { tx })
    }

    pub async fn get(&self) -> Option<String> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Cmd::Get(reply)).ok()?;
        rx.await.ok().flatten()
    }

    pub fn set(&self, text: String) {
        let _ = self.tx.send(Cmd::Set(text));
    }
}

/// Pasteboard types (macOS) that password managers and similar apps add to
/// mean "do not record or share this": 1Password, Bitwarden, KeePassXC…
/// See <http://nspasteboard.org>.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const MAC_SENSITIVE_TYPES: &[&str] =
    &["org.nspasteboard.ConcealedType", "org.nspasteboard.TransientType", "com.agilebits.onepassword"];

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn has_sensitive_type<S: AsRef<str>>(types: impl IntoIterator<Item = S>) -> bool {
    types.into_iter().any(|t| MAC_SENSITIVE_TYPES.contains(&t.as_ref()))
}

/// Whether the current clipboard content was marked as not to be shared.
#[cfg(target_os = "macos")]
fn is_sensitive() -> bool {
    let Some(types) = objc2_app_kit::NSPasteboard::generalPasteboard().types() else { return false };
    has_sensitive_type(types.iter().map(|t| t.to_string()))
}

/// Windows: the formats clipboard managers and password managers set to
/// keep a copy out of history and sync tools.
#[cfg(windows)]
fn is_sensitive() -> bool {
    ["ExcludeClipboardContentFromMonitorProcessing", "Clipboard Viewer Ignore"].iter().any(|name| {
        clipboard_win::register_format(name).is_some_and(|format| clipboard_win::is_format_avail(format.get()))
    })
}

/// Not detected yet on X11 and Wayland.
#[cfg(not(any(target_os = "macos", windows)))]
fn is_sensitive() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concealed_and_transient_types_are_sensitive() {
        assert!(has_sensitive_type(["public.utf8-plain-text", "org.nspasteboard.ConcealedType"]));
        assert!(has_sensitive_type(["org.nspasteboard.TransientType"]));
        assert!(!has_sensitive_type(["public.utf8-plain-text", "public.html"]));
    }

    /// Uses the real pasteboard, so it is opt-in:
    /// `cargo test -p crossdrop-daemon -- --ignored real_pasteboard`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn real_pasteboard() {
        use arboard::SetExtApple;
        let mut cb = arboard::Clipboard::new().unwrap();
        let saved = cb.get_text().ok();
        cb.set().exclude_from_history().text("hunter2").unwrap();
        let concealed = is_sensitive();
        cb.set_text("plain").unwrap();
        let plain = is_sensitive();
        if let Some(text) = saved {
            cb.set_text(text).unwrap();
        }
        assert!(concealed && !plain, "concealed: {concealed}, plain: {plain}");
    }
}

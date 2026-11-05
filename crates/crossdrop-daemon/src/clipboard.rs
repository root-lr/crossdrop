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
                            let _ = reply.send(clipboard.get_text().ok());
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

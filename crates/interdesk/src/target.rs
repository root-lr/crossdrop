//! Target side: applies what a source sends.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use crossdrop_proto::{read_frame, write_frame, FrameError};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::info;

use crate::platform::Platform;
use crate::protocol::{Button, Msg};

/// Keys and buttons currently held down by a source.
#[derive(Default)]
struct Held {
    keys: HashSet<u16>,
    buttons: HashSet<Button>,
}

impl Held {
    /// Releases everything, so no key stays stuck when the source leaves or
    /// the connection drops.
    fn release(&mut self, platform: &dyn Platform) {
        for key in self.keys.drain() {
            platform.key(key, false);
        }
        for button in self.buttons.drain() {
            platform.button(button, false);
        }
    }
}

/// Serves one source until it disconnects.
pub async fn serve<S>(mut stream: S, platform: Arc<dyn Platform>, from: &str) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let screen = platform.screen();
    write_frame(&mut stream, &Msg::Screen { width: screen.w as u32, height: screen.h as u32 }).await?;
    info!("{from} connected as a neighbour");
    let mut held = Held::default();
    let result = loop {
        let msg: Msg = match read_frame(&mut stream).await {
            Ok(m) => m,
            Err(FrameError::Closed) => break Ok(()),
            Err(e) => break Err(e.into()),
        };
        match msg {
            Msg::Enter { x, y } => {
                info!("now controlled by {from}");
                platform.move_to(x, y);
            }
            Msg::Move { x, y } => platform.move_to(x, y),
            Msg::Button { button, down } => {
                if down {
                    held.buttons.insert(button);
                } else {
                    held.buttons.remove(&button);
                }
                platform.button(button, down);
            }
            Msg::Wheel { dx, dy } => platform.wheel(dx, dy),
            Msg::Key { hid, down } => {
                // A key-down for a key already down is an auto-repeat.
                let repeat = if down {
                    !held.keys.insert(hid)
                } else {
                    held.keys.remove(&hid);
                    false
                };
                if !(repeat && platform.repeats_keys()) {
                    platform.key(hid, down);
                }
            }
            Msg::Leave => {
                held.release(platform.as_ref());
                info!("control returned to {from}");
            }
            Msg::Screen { .. } => {}
        }
    };
    held.release(platform.as_ref());
    info!("{from} disconnected");
    result
}

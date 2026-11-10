//! Target side: applies what a source sends.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

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

/// How often the target checks whether its desktop changed size (a new
/// resolution, a monitor plugged in or out).
pub const SCREEN_CHECK: Duration = Duration::from_secs(2);

fn screen_msg(platform: &dyn Platform) -> Msg {
    let screen = platform.screen();
    Msg::Screen { width: screen.w as u32, height: screen.h as u32 }
}

/// Serves one source until it disconnects.
pub async fn serve<S>(stream: S, platform: Arc<dyn Platform>, from: &str) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut rd, mut wr) = tokio::io::split(stream);
    let mut sent = screen_msg(platform.as_ref());
    write_frame(&mut wr, &sent).await?;
    info!("{from} connected as a neighbour");
    // Tell the source when the desktop changes size, so that it keeps
    // mapping the shared edge onto the whole screen.
    let watch_screen = async {
        loop {
            tokio::time::sleep(SCREEN_CHECK).await;
            let now = screen_msg(platform.as_ref());
            if now != sent {
                info!("screen is now {now:?}; telling {from}");
                if write_frame(&mut wr, &now).await.is_err() {
                    return;
                }
                sent = now;
            }
        }
    };
    let apply = apply_input(&mut rd, platform.as_ref(), from);
    // The reader decides when the session ends; the watcher only writes.
    let result = tokio::select! {
        r = apply => r,
        () = watch_screen => Ok(()),
    };
    info!("{from} disconnected");
    result
}

/// Applies what the source sends until it disconnects, then releases every
/// key and button it still held.
async fn apply_input<R: AsyncRead + Unpin>(rd: &mut R, platform: &dyn Platform, from: &str) -> Result<()> {
    let mut held = Held::default();
    let result = loop {
        let msg: Msg = match read_frame(rd).await {
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
                held.release(platform);
                info!("control returned to {from}");
            }
            Msg::Screen { .. } => {}
        }
    };
    held.release(platform);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Rect;
    use std::sync::Mutex;

    /// A desktop whose size the test changes; input is ignored.
    struct Desk(Mutex<Rect>);

    impl Platform for Desk {
        fn screen(&self) -> Rect {
            *self.0.lock().unwrap()
        }
        fn set_grab(&self, _: bool) -> bool {
            true
        }
        fn warp(&self, _: i32, _: i32) {}
        fn move_to(&self, _: i32, _: i32) {}
        fn button(&self, _: Button, _: bool) {}
        fn wheel(&self, _: i32, _: i32) {}
        fn key(&self, _: u16, _: bool) {}
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_screen_size_is_sent_to_the_source() {
        let desk = Arc::new(Desk(Mutex::new(Rect { x: 0, y: 0, w: 1920, h: 1080 })));
        let (mut source, target_end) = tokio::io::duplex(4096);
        let platform: Arc<dyn Platform> = desk.clone();
        tokio::spawn(async move { serve(target_end, platform, "alpha").await });

        assert_eq!(read_frame::<_, Msg>(&mut source).await.unwrap(), Msg::Screen { width: 1920, height: 1080 });
        *desk.0.lock().unwrap() = Rect { x: 0, y: 0, w: 2560, h: 1440 };
        assert_eq!(read_frame::<_, Msg>(&mut source).await.unwrap(), Msg::Screen { width: 2560, height: 1440 });
    }
}

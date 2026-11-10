//! Target side: applies what a source sends.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use crossdrop_proto::{read_frame, write_frame, FrameError};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
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

/// Which source drives this computer: one at a time, shared by every
/// session of a target.
#[derive(Default)]
pub struct Control(Mutex<Option<u64>>);

impl Control {
    /// `session` takes control unless another one has it.
    fn take(&self, session: u64) -> bool {
        let mut owner = self.0.lock().unwrap();
        match *owner {
            Some(other) if other != session => false,
            _ => {
                *owner = Some(session);
                true
            }
        }
    }

    fn release(&self, session: u64) {
        let mut owner = self.0.lock().unwrap();
        if *owner == Some(session) {
            *owner = None;
        }
    }
}

/// Serves one source until it disconnects.
pub async fn serve<S>(stream: S, platform: Arc<dyn Platform>, control: Arc<Control>, from: &str) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    static SESSIONS: AtomicU64 = AtomicU64::new(0);
    let session = SESSIONS.fetch_add(1, Ordering::Relaxed);
    let (mut rd, mut wr) = tokio::io::split(stream);
    let mut sent = screen_msg(platform.as_ref());
    write_frame(&mut wr, &sent).await?;
    info!("{from} connected as a neighbour");
    let (replies, mut to_send) = unbounded_channel();
    // Writes the replies to the source, and tells it when the desktop
    // changes size, so that it keeps mapping the shared edge onto it.
    let writer = async {
        loop {
            let msg = tokio::select! {
                Some(msg) = to_send.recv() => msg,
                () = tokio::time::sleep(SCREEN_CHECK) => {
                    let now = screen_msg(platform.as_ref());
                    if now == sent {
                        continue;
                    }
                    info!("screen is now {now:?}; telling {from}");
                    sent = now.clone();
                    now
                }
            };
            if write_frame(&mut wr, &msg).await.is_err() {
                return;
            }
        }
    };
    let apply = apply_input(&mut rd, platform.as_ref(), &control, session, &replies, from);
    // The reader decides when the session ends; the writer only writes.
    let result = tokio::select! {
        r = apply => r,
        () = writer => Ok(()),
    };
    control.release(session);
    info!("{from} disconnected");
    result
}

/// Applies what the source sends until it disconnects, then releases every
/// key and button it still held.
/// Input from a source that does not have control is dropped, and its
/// `Enter` answered with `Busy`.
async fn apply_input<R: AsyncRead + Unpin>(
    rd: &mut R,
    platform: &dyn Platform,
    control: &Control,
    session: u64,
    replies: &UnboundedSender<Msg>,
    from: &str,
) -> Result<()> {
    let mut held = Held::default();
    let mut driving = false;
    let result = loop {
        let msg: Msg = match read_frame(rd).await {
            Ok(m) => m,
            Err(FrameError::Closed) => break Ok(()),
            Err(e) => break Err(e.into()),
        };
        if let Msg::Enter { .. } = msg {
            driving = control.take(session);
            if !driving {
                info!("{from} wants control, but another computer has it");
                let _ = replies.send(Msg::Busy);
                continue;
            }
        }
        if !driving {
            continue;
        }
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
                driving = false;
                control.release(session);
                info!("control returned to {from}");
            }
            Msg::Screen { .. } | Msg::Busy => {}
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
        tokio::spawn(async move { serve(target_end, platform, Arc::default(), "alpha").await });

        assert_eq!(read_frame::<_, Msg>(&mut source).await.unwrap(), Msg::Screen { width: 1920, height: 1080 });
        *desk.0.lock().unwrap() = Rect { x: 0, y: 0, w: 2560, h: 1440 };
        assert_eq!(read_frame::<_, Msg>(&mut source).await.unwrap(), Msg::Screen { width: 2560, height: 1440 });
    }

    /// Records where the cursor was moved.
    struct Recorder(Mutex<Vec<(i32, i32)>>);

    impl Platform for Recorder {
        fn screen(&self) -> Rect {
            Rect { x: 0, y: 0, w: 1920, h: 1080 }
        }
        fn set_grab(&self, _: bool) -> bool {
            true
        }
        fn warp(&self, _: i32, _: i32) {}
        fn move_to(&self, x: i32, y: i32) {
            self.0.lock().unwrap().push((x, y));
        }
        fn button(&self, _: Button, _: bool) {}
        fn wheel(&self, _: i32, _: i32) {}
        fn key(&self, _: u16, _: bool) {}
    }

    #[tokio::test]
    async fn one_source_drives_at_a_time() {
        let recorder = Arc::new(Recorder(Mutex::new(vec![])));
        let control = Arc::new(Control::default());
        let source = |name: &'static str| {
            let (end, target_end) = tokio::io::duplex(4096);
            let platform: Arc<dyn Platform> = recorder.clone();
            let control = control.clone();
            tokio::spawn(async move { serve(target_end, platform, control, name).await });
            end
        };
        let (mut a, mut b) = (source("alpha"), source("bravo"));
        for s in [&mut a, &mut b] {
            assert!(matches!(read_frame::<_, Msg>(s).await.unwrap(), Msg::Screen { .. }));
        }

        write_frame(&mut a, &Msg::Enter { x: 1, y: 1 }).await.unwrap();
        write_frame(&mut a, &Msg::Move { x: 2, y: 2 }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        write_frame(&mut b, &Msg::Enter { x: 9, y: 9 }).await.unwrap();
        assert_eq!(read_frame::<_, Msg>(&mut b).await.unwrap(), Msg::Busy);
        write_frame(&mut b, &Msg::Move { x: 8, y: 8 }).await.unwrap();
        write_frame(&mut a, &Msg::Leave).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        write_frame(&mut b, &Msg::Enter { x: 7, y: 7 }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(*recorder.0.lock().unwrap(), [(1, 1), (2, 2), (7, 7)]);
    }
}

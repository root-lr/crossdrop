//! X11 backend (Linux, BSD). Pure Rust through x11rb.
//!
//! - Not grabbed: the pointer position is polled every 8 ms.
//! - Grabbed: pointer and keyboard are grabbed on the root window; the
//!   pointer is kept at the screen centre and every motion event is turned
//!   into a delta.
//! - Injection: XTEST fake input.
//!
//! Wayland sessions use the Wayland backend instead: XWayland alone cannot
//! capture or inject global input.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{self, ConnectionExt as _, EventMask, GrabMode, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::{CURRENT_TIME, NONE};

use super::Platform;
use crate::keymap;
use crate::layout::Rect;
use crate::protocol::{Button, WHEEL_NOTCH};
use crate::session::Input;

struct X11 {
    conn: RustConnection,
    root: Window,
    /// Last known size of the root window (it changes with RandR).
    screen: Mutex<Rect>,
    grabbed: AtomicBool,
    /// Wheel remainder in 1/120 notches (x, y), for smooth-scrolling sources.
    wheel: Mutex<(i32, i32)>,
}

pub fn start(events: UnboundedSender<Input>) -> Result<Arc<dyn Platform>> {
    if std::env::var_os("DISPLAY").is_none() {
        bail!("no X11 display (DISPLAY is not set)");
    }
    let (conn, screen_num) = x11rb::connect(None).context("cannot connect to the X server")?;
    if conn.extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME)?.is_none() {
        bail!("the X server has no XTEST extension, required to move the pointer");
    }
    let s = &conn.setup().roots[screen_num];
    let root = s.root;
    let screen = Rect { x: 0, y: 0, w: s.width_in_pixels as i32, h: s.height_in_pixels as i32 };
    let x = Arc::new(X11 {
        conn,
        root,
        screen: Mutex::new(screen),
        grabbed: AtomicBool::new(false),
        wheel: Mutex::new((0, 0)),
    });

    let poller = x.clone();
    let tx = events.clone();
    std::thread::Builder::new().name("interdesk-x11-poll".into()).spawn(move || {
        let mut last = (i32::MIN, i32::MIN, false);
        loop {
            std::thread::sleep(Duration::from_millis(8));
            if poller.grabbed.load(Ordering::Relaxed) {
                last = (i32::MIN, i32::MIN, false);
                continue;
            }
            let Ok(cookie) = poller.conn.query_pointer(poller.root) else { break };
            let Ok(p) = cookie.reply() else { break };
            // Button1..3 bits of the pointer mask.
            let pressed = u16::from(p.mask) & 0x0700 != 0;
            let pos = (p.root_x as i32, p.root_y as i32, pressed);
            if pos != last {
                last = pos;
                if tx.send(Input::Pos { x: pos.0, y: pos.1, pressed }).is_err() {
                    break;
                }
            }
        }
    })?;

    let reader = x.clone();
    std::thread::Builder::new().name("interdesk-x11-events".into()).spawn(move || {
        while let Ok(event) = reader.conn.wait_for_event() {
            if let Some(input) = reader.translate(event) {
                if events.send(input).is_err() {
                    break;
                }
            }
        }
        debug!("X11 event thread stopped");
    })?;
    Ok(x)
}

fn button_from_x(detail: u8) -> Option<Button> {
    Some(match detail {
        1 => Button::Left,
        2 => Button::Middle,
        3 => Button::Right,
        8 => Button::Back,
        9 => Button::Forward,
        _ => return None,
    })
}

fn button_to_x(button: Button) -> u8 {
    match button {
        Button::Left => 1,
        Button::Middle => 2,
        Button::Right => 3,
        Button::Back => 8,
        Button::Forward => 9,
    }
}

impl X11 {
    fn center(&self) -> (i32, i32) {
        self.screen.lock().unwrap().center()
    }

    fn translate(&self, event: Event) -> Option<Input> {
        if !self.grabbed.load(Ordering::Relaxed) {
            return None;
        }
        match event {
            Event::MotionNotify(ev) => {
                let (cx, cy) = self.center();
                let (dx, dy) = (ev.root_x as i32 - cx, ev.root_y as i32 - cy);
                if dx == 0 && dy == 0 {
                    return None; // our own warp
                }
                self.warp(cx, cy);
                Some(Input::Delta { dx, dy })
            }
            Event::ButtonPress(ev) | Event::ButtonRelease(ev) => {
                let down = ev.response_type & 0x7f == xproto::BUTTON_PRESS_EVENT;
                match ev.detail {
                    4..=7 if down => {
                        let (dx, dy) = match ev.detail {
                            4 => (0, WHEEL_NOTCH),
                            5 => (0, -WHEEL_NOTCH),
                            6 => (-WHEEL_NOTCH, 0),
                            _ => (WHEEL_NOTCH, 0),
                        };
                        Some(Input::Wheel { dx, dy })
                    }
                    4..=7 => None,
                    d => button_from_x(d).map(|button| Input::Button { button, down }),
                }
            }
            Event::KeyPress(ev) | Event::KeyRelease(ev) => {
                let down = ev.response_type & 0x7f == xproto::KEY_PRESS_EVENT;
                let hid = keymap::hid_from_evdev((ev.detail as u16).checked_sub(8)?)?;
                Some(Input::Key { hid, down })
            }
            _ => None,
        }
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) {
        if let Err(e) = self.conn.xtest_fake_input(kind, detail, CURRENT_TIME, self.root, x, y, 0) {
            warn!("XTEST: {e}");
        }
        let _ = self.conn.flush();
    }

    fn ungrab(&self) {
        let _ = self.conn.ungrab_pointer(CURRENT_TIME);
        let _ = self.conn.ungrab_keyboard(CURRENT_TIME);
        let _ = self.conn.flush();
    }

    fn clicks(&self, button: u8, n: i32) {
        for _ in 0..n {
            self.fake(xproto::BUTTON_PRESS_EVENT, button, 0, 0);
            self.fake(xproto::BUTTON_RELEASE_EVENT, button, 0, 0);
        }
    }
}

impl Platform for X11 {
    fn screen(&self) -> Rect {
        // The size given at connection time goes stale when the resolution
        // or the monitors change: ask the server.
        let size = self.conn.get_geometry(self.root).ok().and_then(|c| c.reply().ok());
        let mut screen = self.screen.lock().unwrap();
        if let Some(g) = size {
            *screen = Rect { x: 0, y: 0, w: g.width as i32, h: g.height as i32 };
        }
        *screen
    }

    fn set_grab(&self, grab: bool) -> bool {
        if grab {
            let mask = EventMask::POINTER_MOTION | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE;
            let pointer = self
                .conn
                .grab_pointer(false, self.root, mask, GrabMode::ASYNC, GrabMode::ASYNC, NONE, NONE, CURRENT_TIME)
                .ok()
                .and_then(|c| c.reply().ok());
            let keyboard = self
                .conn
                .grab_keyboard(false, self.root, CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)
                .ok()
                .and_then(|c| c.reply().ok());
            if pointer.map(|r| r.status) != Some(xproto::GrabStatus::SUCCESS)
                || keyboard.map(|r| r.status) != Some(xproto::GrabStatus::SUCCESS)
            {
                // Half a grab would send keys to both screens: drop it all.
                warn!("could not grab the pointer or keyboard (another application holds it?)");
                self.ungrab();
                return false;
            }
            let (cx, cy) = self.center();
            self.grabbed.store(true, Ordering::Relaxed);
            self.warp(cx, cy);
        } else {
            self.grabbed.store(false, Ordering::Relaxed);
            self.ungrab();
        }
        true
    }

    fn warp(&self, x: i32, y: i32) {
        let _ = self.conn.warp_pointer(NONE, self.root, 0, 0, 0, 0, x as i16, y as i16);
        let _ = self.conn.flush();
    }

    fn move_to(&self, x: i32, y: i32) {
        self.fake(xproto::MOTION_NOTIFY_EVENT, 0, x as i16, y as i16);
    }

    fn button(&self, button: Button, down: bool) {
        let kind = if down { xproto::BUTTON_PRESS_EVENT } else { xproto::BUTTON_RELEASE_EVENT };
        self.fake(kind, button_to_x(button), 0, 0);
    }

    fn wheel(&self, dx: i32, dy: i32) {
        let (nx, ny) = {
            let mut acc = self.wheel.lock().unwrap();
            acc.0 += dx;
            acc.1 += dy;
            let n = (acc.0 / WHEEL_NOTCH, acc.1 / WHEEL_NOTCH);
            acc.0 -= n.0 * WHEEL_NOTCH;
            acc.1 -= n.1 * WHEEL_NOTCH;
            n
        };
        self.clicks(if ny > 0 { 4 } else { 5 }, ny.abs());
        self.clicks(if nx > 0 { 7 } else { 6 }, nx.abs());
    }

    fn key(&self, hid: u16, down: bool) {
        let Some(code) = keymap::evdev_from_hid(hid) else { return };
        let kind = if down { xproto::KEY_PRESS_EVENT } else { xproto::KEY_RELEASE_EVENT };
        self.fake(kind, (code + 8) as u8, 0, 0);
    }

    fn repeats_keys(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    async fn expect(rx: &mut UnboundedReceiver<Input>, want: Input) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(Some(got)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                if got == want {
                    return;
                }
            }
        }
        panic!("never received {want:?}");
    }

    /// Needs an X server: `Xvfb :99 & DISPLAY=:99 cargo test -p interdesk -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn real_x_server() {
        let (tx, mut rx) = unbounded_channel();
        let p = start(tx).unwrap();
        let screen = p.screen();
        assert!(screen.w > 0 && screen.h > 0);

        // Injection moves the real pointer, and the poller reports it.
        p.move_to(100, 200);
        expect(&mut rx, Input::Pos { x: 100, y: 200, pressed: false }).await;

        // Grabbed: motion becomes deltas, keys are captured.
        p.set_grab(true);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (cx, cy) = screen.center();
        p.move_to(cx + 10, cy - 5);
        expect(&mut rx, Input::Delta { dx: 10, dy: -5 }).await;
        p.key(0x04, true);
        expect(&mut rx, Input::Key { hid: 0x04, down: true }).await;
        p.key(0x04, false);
        expect(&mut rx, Input::Key { hid: 0x04, down: false }).await;
        p.button(Button::Left, true);
        expect(&mut rx, Input::Button { button: Button::Left, down: true }).await;
        p.button(Button::Left, false);
        p.wheel(0, -WHEEL_NOTCH);
        expect(&mut rx, Input::Wheel { dx: 0, dy: -WHEEL_NOTCH }).await;

        // Released: positions again.
        p.set_grab(false);
        p.warp(300, 400);
        expect(&mut rx, Input::Pos { x: 300, y: 400, pressed: false }).await;
    }
}

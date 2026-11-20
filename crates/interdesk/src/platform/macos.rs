//! macOS backend.
//!
//! - Capture: a Quartz event tap on its own run-loop thread. While grabbed,
//!   the cursor is detached from the mouse (`CGAssociateMouseAndMouseCursorPosition`)
//!   and hidden; motion is read from the events' delta fields, and buttons,
//!   wheel and keys are swallowed.
//! - Injection: `CGEvent`s posted at the HID level, tagged so that our own
//!   tap ignores them.
//! - Needs the Accessibility permission (System Settings → Privacy &
//!   Security → Accessibility) for the terminal or binary running interdesk.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use core_foundation::base::TCFType;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    CGMouseButton, CallbackResult, EventField, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use tokio::sync::mpsc::UnboundedSender;
use tracing::warn;

use super::Platform;
use crate::keymap;
use crate::layout::Rect;
use crate::protocol::{Button, WHEEL_NOTCH};
use crate::session::Input;

/// Marks events we post, so the tap lets them through untouched.
const MAGIC: i64 = 0x1D_E5C0;

static GRABBED: AtomicBool = AtomicBool::new(false);
/// Raw CFMachPortRef of the tap, to re-enable it if macOS disables it.
static TAP: AtomicUsize = AtomicUsize::new(0);

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapEnable(tap: *const c_void, enable: bool);
}

struct Mac {
    state: Mutex<Injected>,
}

/// Target-side state needed to build realistic events.
#[derive(Default)]
struct Injected {
    pos: (f64, f64),
    buttons: [bool; 3],
    flags: u64,
    /// Modifier keys held, each side apart (`Modifier::side` bits).
    sides: u64,
    last_click: Option<(usize, Instant, i64)>,
    wheel: (i32, i32),
}

fn displays_bounds() -> Rect {
    let ids = CGDisplay::active_displays().unwrap_or_default();
    let rects: Vec<_> = if ids.is_empty() {
        vec![CGDisplay::main().bounds()]
    } else {
        ids.into_iter().map(|id| CGDisplay::new(id).bounds()).collect()
    };
    let x0 = rects.iter().map(|r| r.origin.x).fold(f64::MAX, f64::min);
    let y0 = rects.iter().map(|r| r.origin.y).fold(f64::MAX, f64::min);
    let x1 = rects.iter().map(|r| r.origin.x + r.size.width).fold(f64::MIN, f64::max);
    let y1 = rects.iter().map(|r| r.origin.y + r.size.height).fold(f64::MIN, f64::max);
    Rect { x: x0 as i32, y: y0 as i32, w: (x1 - x0).max(1.0) as i32, h: (y1 - y0).max(1.0) as i32 }
}

/// Modifier key code → flag bit.
/// A modifier key: the flag every app sees, which is the same for the left
/// and right keys, and the bit of this key alone (`NX_DEVICE…KEYMASK`, in the
/// low bits of the same flags) next to the other side's.
#[derive(Clone, Copy)]
struct Modifier {
    flag: u64,
    side: u64,
    other_side: u64,
}

/// Caps Lock: its flag is the lock's state, not whether the key is held.
const CAPS_LOCK: u16 = 0x39;

fn modifier(keycode: u16) -> Option<Modifier> {
    use CGEventFlags as F;
    let m = |flag: F, side, other_side| Modifier { flag: flag.bits(), side, other_side };
    Some(match keycode {
        0x38 => m(F::CGEventFlagShift, 0x02, 0x04),
        0x3C => m(F::CGEventFlagShift, 0x04, 0x02),
        0x3B => m(F::CGEventFlagControl, 0x01, 0x2000),
        0x3E => m(F::CGEventFlagControl, 0x2000, 0x01),
        0x3A => m(F::CGEventFlagAlternate, 0x20, 0x40),
        0x3D => m(F::CGEventFlagAlternate, 0x40, 0x20),
        0x37 => m(F::CGEventFlagCommand, 0x08, 0x10),
        0x36 => m(F::CGEventFlagCommand, 0x10, 0x08),
        CAPS_LOCK => m(F::CGEventFlagAlphaShift, 0, 0),
        _ => return None,
    })
}

/// Whether the modifier key `m` is down after a FlagsChanged event with
/// these flags. The side bits tell left from right: with left Shift held,
/// releasing right Shift leaves the Shift flag set. Events that carry no
/// side bits (some tools post such) fall back on the shared flag.
fn modifier_down(m: Modifier, flags: u64) -> bool {
    if flags & (m.side | m.other_side) != 0 || flags & m.flag == 0 {
        flags & m.side != 0
    } else {
        flags & m.flag != 0
    }
}

fn translate(kind: CGEventType, event: &CGEvent, grabbed: bool) -> Option<Input> {
    use CGEventType::*;
    match kind {
        MouseMoved | LeftMouseDragged | RightMouseDragged | OtherMouseDragged => {
            if grabbed {
                let dx = event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X) as i32;
                let dy = event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y) as i32;
                (dx != 0 || dy != 0).then_some(Input::Delta { dx, dy })
            } else {
                let p = event.location();
                let pressed = !matches!(kind, MouseMoved);
                Some(Input::Pos { x: p.x as i32, y: p.y as i32, pressed })
            }
        }
        _ if !grabbed => None,
        LeftMouseDown | LeftMouseUp => {
            Some(Input::Button { button: Button::Left, down: matches!(kind, LeftMouseDown) })
        }
        RightMouseDown | RightMouseUp => {
            Some(Input::Button { button: Button::Right, down: matches!(kind, RightMouseDown) })
        }
        OtherMouseDown | OtherMouseUp => {
            let button = match event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER) {
                3 => Button::Back,
                4 => Button::Forward,
                _ => Button::Middle,
            };
            Some(Input::Button { button, down: matches!(kind, OtherMouseDown) })
        }
        ScrollWheel => {
            let (dx, dy) = scroll_units(event);
            (dx != 0 || dy != 0).then_some(Input::Wheel { dx, dy })
        }
        KeyDown | KeyUp => {
            let code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            keymap::hid_from_mac(code).map(|hid| Input::Key { hid, down: matches!(kind, KeyDown) })
        }
        FlagsChanged => {
            let code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            let m = modifier(code)?;
            // Each Caps Lock event is one press, which turns the lock on
            // or off: sent as a press (its release follows, see the tap).
            let down = code == CAPS_LOCK || modifier_down(m, event.get_flags().bits());
            keymap::hid_from_mac(code).map(|hid| Input::Key { hid, down })
        }
        _ => None,
    }
}

/// Lets this process hide the cursor although it is not the frontmost
/// application. Without it, macOS shows the cursor again at the next mouse
/// move: while driving a neighbour, the Mac's cursor reappeared, frozen at
/// the edge. Private but long-stable window-server call, used by Barrier and
/// Synergy for the same reason.
fn allow_hiding_cursor_in_background() {
    use core_foundation::boolean::CFBoolean;
    use core_foundation::string::CFString;
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn _CGSDefaultConnection() -> i32;
        fn CGSSetConnectionProperty(cid: i32, target: i32, key: *const c_void, value: *const c_void) -> i32;
    }
    let key = CFString::from_static_string("SetsCursorInBackground");
    // SAFETY: plain window-server calls on our own connection, with valid CF objects.
    let err = unsafe {
        let cid = _CGSDefaultConnection();
        CGSSetConnectionProperty(
            cid,
            cid,
            key.as_concrete_TypeRef().cast(),
            CFBoolean::true_value().as_concrete_TypeRef().cast(),
        )
    };
    if err != 0 {
        warn!("cannot keep the cursor hidden while driving a neighbour (error {err})");
    }
}

pub fn start(events: UnboundedSender<Input>) -> Result<Arc<dyn Platform>> {
    allow_hiding_cursor_in_background();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<bool>();
    std::thread::Builder::new().name("interdesk-tap".into()).spawn(move || {
        use CGEventType::*;
        let interest = vec![
            MouseMoved,
            LeftMouseDragged,
            RightMouseDragged,
            OtherMouseDragged,
            LeftMouseDown,
            LeftMouseUp,
            RightMouseDown,
            RightMouseUp,
            OtherMouseDown,
            OtherMouseUp,
            ScrollWheel,
            KeyDown,
            KeyUp,
            FlagsChanged,
        ];
        let tap = CGEventTap::new(
            CGEventTapLocation::Session,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            interest,
            move |_proxy, kind, event| {
                if matches!(kind, TapDisabledByTimeout | TapDisabledByUserInput) {
                    let port = TAP.load(Ordering::Relaxed);
                    if port != 0 {
                        unsafe { CGEventTapEnable(port as *const c_void, true) };
                    }
                    return CallbackResult::Keep;
                }
                if event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA) == MAGIC {
                    return CallbackResult::Keep;
                }
                let grabbed = GRABBED.load(Ordering::Relaxed);
                if let Some(input) = translate(kind, event, grabbed) {
                    let _ = events.send(input);
                    if matches!(kind, FlagsChanged)
                        && event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16 == CAPS_LOCK
                    {
                        let _ = events.send(Input::Key { hid: keymap::hid::CAPS_LOCK, down: false });
                    }
                }
                if grabbed && !matches!(kind, MouseMoved | LeftMouseDragged | RightMouseDragged | OtherMouseDragged) {
                    CallbackResult::Drop
                } else {
                    CallbackResult::Keep
                }
            },
        );
        let Ok(tap) = tap else {
            let _ = ready_tx.send(false);
            return;
        };
        let Ok(source) = tap.mach_port().create_runloop_source(0) else {
            let _ = ready_tx.send(false);
            return;
        };
        TAP.store(tap.mach_port().as_concrete_TypeRef() as usize, Ordering::Relaxed);
        CFRunLoop::get_current().add_source(&source, unsafe { kCFRunLoopCommonModes });
        tap.enable();
        let _ = ready_tx.send(true);
        CFRunLoop::run_current();
    })?;
    if !ready_rx.recv().unwrap_or(false) {
        bail!(
            "cannot watch the mouse and keyboard. Allow this program in System Settings → \
             Privacy & Security → Accessibility, then start it again"
        );
    }
    Ok(Arc::new(Mac { state: Mutex::new(Injected::default()) }))
}

/// Points of trackpad scrolling sent as one wheel notch: macOS scrolls
/// about ten points per line.
const POINTS_PER_NOTCH: i64 = 10;

/// A scroll event in 1/120 of a notch. A trackpad (or a smooth-scrolling
/// mouse) sends many small "continuous" events whose line count is rounded
/// up to at least one line: counted as notches, a gentle swipe scrolled the
/// neighbour by dozens of notches. Their point deltas give the real amount.
fn scroll_units(event: &CGEvent) -> (i32, i32) {
    let field = |f| event.get_integer_value_field(f);
    if field(EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS) != 0 {
        let units = |points: i64| (points * i64::from(WHEEL_NOTCH) / POINTS_PER_NOTCH) as i32;
        (
            units(field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2)),
            units(field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1)),
        )
    } else {
        let units = |lines: i64| (lines as i32).saturating_mul(WHEEL_NOTCH);
        (
            units(field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2)),
            units(field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1)),
        )
    }
}

fn source() -> Option<CGEventSource> {
    CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()
}

fn post(event: CGEvent) {
    event.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, MAGIC);
    event.post(CGEventTapLocation::HID);
}

impl Platform for Mac {
    fn screen(&self) -> Rect {
        displays_bounds()
    }

    fn set_grab(&self, grab: bool) -> bool {
        GRABBED.store(grab, Ordering::Relaxed);
        if CGDisplay::associate_mouse_and_mouse_cursor_position(!grab).is_err() {
            warn!("cannot detach the cursor from the mouse");
        }
        let display = CGDisplay::main();
        let _ = if grab { display.hide_cursor() } else { display.show_cursor() };
        true
    }

    fn warp(&self, x: i32, y: i32) {
        let _ = CGDisplay::warp_mouse_cursor_position(CGPoint::new(x as f64, y as f64));
    }

    fn move_to(&self, x: i32, y: i32) {
        let origin = displays_bounds();
        let p = CGPoint::new((origin.x + x) as f64, (origin.y + y) as f64);
        let mut st = self.state.lock().unwrap();
        st.pos = (p.x, p.y);
        let (kind, button) = if st.buttons[0] {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if st.buttons[1] {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else if st.buttons[2] {
            (CGEventType::OtherMouseDragged, CGMouseButton::Center)
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        };
        if let Some(e) = source().and_then(|s| CGEvent::new_mouse_event(s, kind, p, button).ok()) {
            post(e);
        }
    }

    fn button(&self, button: Button, down: bool) {
        let mut st = self.state.lock().unwrap();
        let p = CGPoint::new(st.pos.0, st.pos.1);
        let (kind, cg, index, number) = match (button, down) {
            (Button::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left, 0, 0),
            (Button::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left, 0, 0),
            (Button::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right, 1, 1),
            (Button::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right, 1, 1),
            (b, d) => {
                let number = match b {
                    Button::Back => 3,
                    Button::Forward => 4,
                    _ => 2,
                };
                let kind = if d { CGEventType::OtherMouseDown } else { CGEventType::OtherMouseUp };
                (kind, CGMouseButton::Center, 2, number)
            }
        };
        st.buttons[index] = down;
        // Double clicks need a click count: same button again within 400 ms.
        let clicks = match (down, st.last_click) {
            (true, Some((b, t, n))) if b == index && t.elapsed() < Duration::from_millis(400) => n + 1,
            (true, _) => 1,
            (false, Some((_, _, n))) => n,
            (false, None) => 1,
        };
        if down {
            st.last_click = Some((index, Instant::now(), clicks));
        }
        if let Some(e) = source().and_then(|s| CGEvent::new_mouse_event(s, kind, p, cg).ok()) {
            e.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, clicks);
            e.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, number);
            post(e);
        }
    }

    fn wheel(&self, dx: i32, dy: i32) {
        let (lx, ly) = {
            let mut st = self.state.lock().unwrap();
            st.wheel.0 += dx;
            st.wheel.1 += dy;
            let n = (st.wheel.0 / WHEEL_NOTCH, st.wheel.1 / WHEEL_NOTCH);
            st.wheel.0 -= n.0 * WHEEL_NOTCH;
            st.wheel.1 -= n.1 * WHEEL_NOTCH;
            n
        };
        if lx == 0 && ly == 0 {
            return;
        }
        if let Some(e) = source().and_then(|s| CGEvent::new_scroll_event(s, ScrollEventUnit::LINE, 2, ly, lx, 0).ok()) {
            post(e);
        }
    }

    fn key(&self, hid: u16, down: bool) {
        let Some(code) = keymap::mac_from_hid(hid) else { return };
        let mut st = self.state.lock().unwrap();
        let modifier = modifier(code);
        if let Some(m) = modifier {
            if m.side == 0 {
                // Caps Lock.
                st.flags = if down { st.flags | m.flag } else { st.flags & !m.flag };
            } else {
                st.sides = if down { st.sides | m.side } else { st.sides & !m.side };
                // Held while either side is.
                let held = st.sides & (m.side | m.other_side) != 0;
                st.flags = if held { st.flags | m.flag } else { st.flags & !m.flag };
            }
        }
        let Some(e) = source().and_then(|s| CGEvent::new_keyboard_event(s, code, down).ok()) else { return };
        if modifier.is_some() {
            e.set_type(CGEventType::FlagsChanged);
        }
        e.set_flags(CGEventFlags::from_bits_truncate(st.flags));
        post(e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_and_right_modifiers_are_told_apart() {
        let shift = CGEventFlags::CGEventFlagShift.bits();
        let (left, right) = (modifier(0x38).unwrap(), modifier(0x3C).unwrap());
        // Left Shift held, right Shift pressed then released.
        assert!(modifier_down(left, shift | 0x02));
        assert!(modifier_down(right, shift | 0x02 | 0x04));
        assert!(!modifier_down(right, shift | 0x02), "right Shift is up although Shift still is");
        assert!(!modifier_down(left, 0));
        // An event without side bits: the shared flag decides.
        assert!(modifier_down(right, shift));
    }
    use core_graphics::event::CGScrollEventUnit;

    fn scroll(unit: CGScrollEventUnit, dy: i32, dx: i32) -> Option<Input> {
        let source = CGEventSource::new(CGEventSourceStateID::Private).unwrap();
        let event = CGEvent::new_scroll_event(source, unit, 2, dy, dx, 0).unwrap();
        translate(CGEventType::ScrollWheel, &event, true)
    }

    #[test]
    fn trackpad_scrolling_keeps_small_movements() {
        // 3 points down: macOS also reports it as a whole line, which used to
        // be sent as a whole notch (120).
        assert_eq!(scroll(ScrollEventUnit::PIXEL, -3, 0), Some(Input::Wheel { dx: 0, dy: -36 }));
        assert_eq!(scroll(ScrollEventUnit::PIXEL, 0, 20), Some(Input::Wheel { dx: 240, dy: 0 }));
        // A wheel notch is still a whole notch.
        assert_eq!(scroll(ScrollEventUnit::LINE, 1, 0), Some(Input::Wheel { dx: 0, dy: WHEEL_NOTCH }));
    }
}

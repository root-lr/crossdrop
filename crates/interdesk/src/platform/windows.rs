//! Windows backend.
//!
//! - Capture: low-level mouse and keyboard hooks on a dedicated thread with
//!   its own message loop. While grabbed, the hooks swallow events, so the
//!   cursor stays at the screen centre and each move reports its offset.
//! - Injection: `SendInput`, absolute coordinates on the virtual desktop,
//!   keys by scan code (layout independent).
//! - Events we inject are flagged by Windows and ignored by our own hooks.

use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

use anyhow::{bail, Result};
use tokio::sync::mpsc::UnboundedSender;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, SetCursorPos, SetWindowsHookExW,
    TranslateMessage, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, LLKHF_UP, LLMHF_INJECTED, MSG,
    MSLLHOOKSTRUCT, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

use super::Platform;
use crate::keymap;
use crate::layout::Rect;
use crate::protocol::Button;
use crate::session::Input;

// Hook procedures are plain functions: their state lives in statics.
static EVENTS: OnceLock<UnboundedSender<Input>> = OnceLock::new();
static GRABBED: AtomicBool = AtomicBool::new(false);
static CENTER_X: AtomicI32 = AtomicI32::new(0);
static CENTER_Y: AtomicI32 = AtomicI32::new(0);
/// Bits of the physical buttons currently held (left, right, middle).
static BUTTONS: AtomicU8 = AtomicU8::new(0);

struct Win;

fn virtual_screen() -> Rect {
    unsafe {
        Rect {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            w: GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
            h: GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
        }
    }
}

fn emit(input: Input) {
    if let Some(tx) = EVENTS.get() {
        let _ = tx.send(input);
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let info = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
        if info.flags & LLMHF_INJECTED == 0 {
            let grabbed = GRABBED.load(Ordering::Relaxed);
            let (x, y) = (info.pt.x, info.pt.y);
            let high = (info.mouseData >> 16) as u16;
            match wparam as u32 {
                WM_LBUTTONDOWN => BUTTONS.fetch_or(1, Ordering::Relaxed),
                WM_LBUTTONUP => BUTTONS.fetch_and(!1, Ordering::Relaxed),
                WM_RBUTTONDOWN => BUTTONS.fetch_or(2, Ordering::Relaxed),
                WM_RBUTTONUP => BUTTONS.fetch_and(!2, Ordering::Relaxed),
                WM_MBUTTONDOWN => BUTTONS.fetch_or(4, Ordering::Relaxed),
                WM_MBUTTONUP => BUTTONS.fetch_and(!4, Ordering::Relaxed),
                _ => 0,
            };
            let pressed = !grabbed && BUTTONS.load(Ordering::Relaxed) != 0;
            let input = match (wparam as u32, grabbed) {
                (WM_MOUSEMOVE, false) => Some(Input::Pos { x, y, pressed }),
                (WM_MOUSEMOVE, true) => {
                    let (dx, dy) = (x - CENTER_X.load(Ordering::Relaxed), y - CENTER_Y.load(Ordering::Relaxed));
                    (dx != 0 || dy != 0).then_some(Input::Delta { dx, dy })
                }
                (_, false) => None,
                (WM_LBUTTONDOWN, true) => Some(Input::Button { button: Button::Left, down: true }),
                (WM_LBUTTONUP, true) => Some(Input::Button { button: Button::Left, down: false }),
                (WM_RBUTTONDOWN, true) => Some(Input::Button { button: Button::Right, down: true }),
                (WM_RBUTTONUP, true) => Some(Input::Button { button: Button::Right, down: false }),
                (WM_MBUTTONDOWN, true) => Some(Input::Button { button: Button::Middle, down: true }),
                (WM_MBUTTONUP, true) => Some(Input::Button { button: Button::Middle, down: false }),
                (WM_XBUTTONDOWN | WM_XBUTTONUP, true) => {
                    let button = if high == 1 { Button::Back } else { Button::Forward };
                    Some(Input::Button { button, down: wparam as u32 == WM_XBUTTONDOWN })
                }
                (WM_MOUSEWHEEL, true) => Some(Input::Wheel { dx: 0, dy: high as i16 as i32 }),
                (WM_MOUSEHWHEEL, true) => Some(Input::Wheel { dx: high as i16 as i32, dy: 0 }),
                _ => None,
            };
            if let Some(input) = input {
                emit(input);
            }
            if grabbed {
                return 1;
            }
        }
    }
    unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && GRABBED.load(Ordering::Relaxed) {
        let info = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        if info.flags & LLKHF_INJECTED == 0 {
            let ext = if info.flags & LLKHF_EXTENDED != 0 { 0xE000 } else { 0 };
            let scan = (info.scanCode as u16 & 0xFF) | ext;
            if let Some(hid) = keymap::hid_from_windows(scan) {
                emit(Input::Key { hid, down: info.flags & LLKHF_UP == 0 });
            }
            return 1;
        }
    }
    unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) }
}

pub fn start(events: UnboundedSender<Input>) -> Result<Arc<dyn Platform>> {
    if EVENTS.set(events).is_err() {
        bail!("the Windows backend can only be started once");
    }
    unsafe {
        // Physical pixels everywhere, whatever the scaling of each monitor.
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::Builder::new().name("interdesk-hooks".into()).spawn(move || unsafe {
        let module = GetModuleHandleW(std::ptr::null());
        let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0);
        let keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), module, 0);
        if mouse.is_null() || keyboard.is_null() {
            let _ = ready_tx.send(Err("cannot install input hooks".into()));
            return;
        }
        let _ = ready_tx.send(Ok(()));
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    })?;
    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => bail!(e),
        Err(_) => bail!("the input hook thread stopped"),
    }
    // Report where the cursor is right away.
    let mut p = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut p) } != 0 {
        emit(Input::Pos { x: p.x, y: p.y, pressed: false });
    }
    Ok(Arc::new(Win))
}

fn send(inputs: &[INPUT]) {
    unsafe {
        SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32);
    }
}

fn mouse(dx: i32, dy: i32, data: i32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

impl Platform for Win {
    fn screen(&self) -> Rect {
        virtual_screen()
    }

    fn set_grab(&self, grab: bool) -> bool {
        if grab {
            let (cx, cy) = virtual_screen().center();
            CENTER_X.store(cx, Ordering::Relaxed);
            CENTER_Y.store(cy, Ordering::Relaxed);
            unsafe { SetCursorPos(cx, cy) };
            GRABBED.store(true, Ordering::Relaxed);
        } else {
            GRABBED.store(false, Ordering::Relaxed);
        }
        true
    }

    fn warp(&self, x: i32, y: i32) {
        unsafe { SetCursorPos(x, y) };
    }

    fn move_to(&self, x: i32, y: i32) {
        let s = virtual_screen();
        // Normalised 0..=65535 across the virtual desktop.
        let nx = (x.clamp(0, s.w - 1) as i64 * 65535 / (s.w - 1).max(1) as i64) as i32;
        let ny = (y.clamp(0, s.h - 1) as i64 * 65535 / (s.h - 1).max(1) as i64) as i32;
        send(&[mouse(nx, ny, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)]);
    }

    fn button(&self, button: Button, down: bool) {
        let (flags, data) = match (button, down) {
            (Button::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (Button::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (Button::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (Button::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (Button::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (Button::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (Button::Back, true) => (MOUSEEVENTF_XDOWN, 1),
            (Button::Back, false) => (MOUSEEVENTF_XUP, 1),
            (Button::Forward, true) => (MOUSEEVENTF_XDOWN, 2),
            (Button::Forward, false) => (MOUSEEVENTF_XUP, 2),
        };
        send(&[mouse(0, 0, data, flags)]);
    }

    fn wheel(&self, dx: i32, dy: i32) {
        if dy != 0 {
            send(&[mouse(0, 0, dy, MOUSEEVENTF_WHEEL)]);
        }
        if dx != 0 {
            send(&[mouse(0, 0, dx, MOUSEEVENTF_HWHEEL)]);
        }
    }

    fn key(&self, hid: u16, down: bool) {
        let Some(scan) = keymap::windows_from_hid(hid) else { return };
        let mut flags = KEYEVENTF_SCANCODE;
        if scan & 0xE000 == 0xE000 {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT { wVk: 0, wScan: scan & 0xFF, dwFlags: flags, time: 0, dwExtraInfo: 0 },
            },
        };
        send(&[input]);
    }
}

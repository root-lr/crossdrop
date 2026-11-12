//! Linux, Wayland: being controlled (target side).
//!
//! Wayland lets no program read or inject global input by itself. interdesk
//! asks the desktop through the RemoteDesktop portal (xdg-desktop-portal,
//! GNOME 45+ or KDE Plasma 6.1+), which shows a permission dialog the first
//! time, then talks libei (the `ei` protocol) over the socket the portal
//! returns: absolute pointer, buttons, scroll and keyboard devices, with the
//! layout of the monitors.
//!
//! The permission is remembered with the portal's restore token, kept in the
//! crossdrop data directory, so the dialog does not come back each start.
//!
//! Controlling other computers from a Wayland session (capturing this
//! keyboard and mouse) needs the InputCapture portal and is not supported
//! yet: `set_grab` refuses, so the cursor never leaves this screen.

use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use ashpd::desktop::remote_desktop::{ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions};
use ashpd::desktop::PersistMode;
use futures_util::StreamExt;
use reis::ei;
use reis::event::{Connection, Device, DeviceCapability, EiEvent};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

use super::Platform;
use crate::keymap;
use crate::layout::Rect;
use crate::protocol::{Button, WHEEL_NOTCH};
use crate::session::Input;

/// Whether this session is Wayland (and not plain X11).
pub fn is_session() -> bool {
    std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland")
        || (std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var_os("DISPLAY").is_none())
}

struct Devices {
    connection: Connection,
    /// Devices the compositor has resumed, which accept events now.
    active: Vec<Device>,
    /// Bounding box of the absolute pointer's regions (the monitors).
    screen: Rect,
    sequence: u32,
}

impl Devices {
    /// An active device with `cap`; the absolute pointer first, since
    /// buttons and scroll usually live on it.
    fn with(&self, cap: DeviceCapability) -> Option<&Device> {
        let mut matching = self.active.iter().filter(|d| d.has_capability(cap));
        let first = matching.clone().next();
        matching.find(|d| d.has_capability(DeviceCapability::PointerAbsolute)).or(first)
    }

    /// Ends one batch of events on `device` and sends it.
    fn frame(&self, device: &Device) {
        device.device().frame(self.connection.serial(), now_us());
        if let Err(e) = self.connection.flush() {
            debug!("ei flush: {e}");
        }
    }
}

struct Wayland {
    devices: Arc<Mutex<Devices>>,
}

pub fn start(_events: UnboundedSender<Input>) -> Result<Arc<dyn Platform>> {
    // The portal, the ei handshake and the device events are asynchronous,
    // and reis's event stream cannot move between threads: they all run on
    // one thread with its own runtime, which reports when the devices are
    // ready (or why not).
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new().name("interdesk-wayland".into()).spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(r) => r,
            Err(e) => {
                let _ = ready_tx.send(Err(e.into()));
                return;
            }
        };
        runtime.block_on(drive(ready_tx));
    })?;
    let devices = ready_rx.recv().context("the Wayland backend stopped")??;
    info!("Wayland: this computer can be controlled; controlling others from Wayland is not supported yet");
    Ok(Arc::new(Wayland { devices }))
}

type Ready = std::sync::mpsc::Sender<Result<Arc<Mutex<Devices>>>>;

/// Connects, reports through `ready`, then follows the devices until the
/// desktop closes the connection.
async fn drive(ready: Ready) {
    let (devices, mut events, keep) = match connect().await {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = ready.send(Ok(devices.clone()));
    // The portal session ends when `keep` is dropped.
    let _keep = keep;
    while let Some(event) = events.next().await {
        match event {
            Ok(event) => handle(&devices, event),
            Err(e) => {
                warn!("ei: {e}");
                break;
            }
        }
    }
    warn!("Wayland: the desktop closed the input connection; restart interdesk to be controlled again");
    devices.lock().unwrap().active.clear();
}

fn token_path() -> Option<std::path::PathBuf> {
    crossdrop_sdk::Paths::resolve(None).ok().map(|p| p.home.join("wayland-remote-desktop.token"))
}

type Session = (RemoteDesktop, ashpd::desktop::Session<RemoteDesktop>);

async fn connect() -> Result<(Arc<Mutex<Devices>>, reis::tokio::EiConvertEventStream, Session)> {
    let portal = RemoteDesktop::new()
        .await
        .context("no RemoteDesktop portal (xdg-desktop-portal with GNOME 45+ or KDE Plasma 6.1+ is needed)")?;
    let session = portal.create_session(Default::default()).await.context("RemoteDesktop portal: create session")?;
    let token = token_path().and_then(|p| std::fs::read_to_string(p).ok());
    let options = SelectDevicesOptions::default()
        .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
        .set_persist_mode(PersistMode::ExplicitlyRevoked)
        .set_restore_token(token.as_deref().map(str::trim));
    portal.select_devices(&session, options).await.context("RemoteDesktop portal: select devices")?;
    let selected = portal
        .start(&session, None, Default::default())
        .await
        .context("RemoteDesktop portal: start")?
        .response()
        .map_err(|e| anyhow!("permission to control this computer was not given ({e})"))?;
    if let (Some(token), Some(path)) = (selected.restore_token(), token_path()) {
        if let Err(e) = crossdrop_sdk::write_atomic(&path, token.as_bytes()) {
            warn!("cannot keep the Wayland permission for next time: {e}");
        }
    }

    let fd = portal
        .connect_to_eis(&session, ConnectToEISOptions::default())
        .await
        .context("RemoteDesktop portal: ConnectToEIS")?;
    let context = ei::Context::new(UnixStream::from(fd))?;
    let (connection, mut events) = context
        .handshake_tokio("interdesk", ei::handshake::ContextType::Sender)
        .await
        .map_err(|e| anyhow!("ei handshake: {e}"))?;
    let devices = Arc::new(Mutex::new(Devices {
        connection,
        active: vec![],
        screen: Rect { x: 0, y: 0, w: 0, h: 0 },
        sequence: 0,
    }));

    // Wait for an absolute pointer to be usable: its regions give the screen.
    let ready = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = events.next().await {
            handle(&devices, event.map_err(|e| anyhow!("ei: {e}"))?);
            let d = devices.lock().unwrap();
            if d.screen.w > 0 && d.with(DeviceCapability::Keyboard).is_some() {
                return Ok(());
            }
        }
        bail!("the desktop closed the input connection")
    })
    .await;
    match ready {
        Ok(r) => r?,
        Err(_) => bail!("the desktop offered no absolute pointer and keyboard within 10 s"),
    }

    Ok((devices, events, (portal, session)))
}

fn handle(devices: &Mutex<Devices>, event: EiEvent) {
    let mut d = devices.lock().unwrap();
    match event {
        EiEvent::SeatAdded(e) => {
            e.seat.bind_capabilities(
                DeviceCapability::PointerAbsolute
                    | DeviceCapability::Button
                    | DeviceCapability::Scroll
                    | DeviceCapability::Keyboard,
            );
            let _ = d.connection.flush();
        }
        EiEvent::DeviceResumed(e) => {
            let sequence = d.sequence;
            d.sequence += 1;
            e.device.device().start_emulating(e.serial, sequence);
            let _ = d.connection.flush();
            if e.device.has_capability(DeviceCapability::PointerAbsolute) {
                d.screen = bounding_box(&e.device);
                debug!(screen = ?d.screen, "absolute pointer ready");
            }
            d.active.retain(|x| x.device() != e.device.device());
            d.active.push(e.device);
        }
        EiEvent::DevicePaused(e) => d.active.retain(|x| x.device() != e.device.device()),
        EiEvent::DeviceRemoved(e) => d.active.retain(|x| x.device() != e.device.device()),
        EiEvent::Disconnected(_) => d.active.clear(),
        _ => {}
    }
}

/// The union of the device's regions, in logical pixels.
fn bounding_box(device: &Device) -> Rect {
    let regions = device.regions();
    if regions.is_empty() {
        return Rect { x: 0, y: 0, w: 0, h: 0 };
    }
    let x0 = regions.iter().map(|r| r.x).min().unwrap_or(0);
    let y0 = regions.iter().map(|r| r.y).min().unwrap_or(0);
    let x1 = regions.iter().map(|r| r.x + r.width).max().unwrap_or(0);
    let y1 = regions.iter().map(|r| r.y + r.height).max().unwrap_or(0);
    Rect { x: x0 as i32, y: y0 as i32, w: (x1 - x0) as i32, h: (y1 - y0) as i32 }
}

/// CLOCK_MONOTONIC in microseconds, the time base of ei frames.
fn now_us() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid, writable timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}

/// Linux input event codes for mouse buttons.
fn button_code(button: Button) -> u32 {
    match button {
        Button::Left => 0x110,
        Button::Right => 0x111,
        Button::Middle => 0x112,
        Button::Back => 0x113,
        Button::Forward => 0x114,
    }
}

impl Platform for Wayland {
    fn screen(&self) -> Rect {
        self.devices.lock().unwrap().screen
    }

    fn set_grab(&self, _grab: bool) -> bool {
        // Capturing this computer's input is not possible here yet.
        false
    }

    fn warp(&self, _x: i32, _y: i32) {}

    fn move_to(&self, x: i32, y: i32) {
        let d = self.devices.lock().unwrap();
        let Some(dev) = d.with(DeviceCapability::PointerAbsolute) else { return };
        let Some(abs) = dev.interface::<ei::PointerAbsolute>() else { return };
        abs.motion_absolute((d.screen.x + x) as f32, (d.screen.y + y) as f32);
        d.frame(dev);
    }

    fn button(&self, button: Button, down: bool) {
        let d = self.devices.lock().unwrap();
        let Some(dev) = d.with(DeviceCapability::Button) else { return };
        let Some(b) = dev.interface::<ei::Button>() else { return };
        let state = if down { ei::button::ButtonState::Press } else { ei::button::ButtonState::Released };
        b.button(button_code(button), state);
        d.frame(dev);
    }

    fn wheel(&self, dx: i32, dy: i32) {
        let d = self.devices.lock().unwrap();
        let Some(dev) = d.with(DeviceCapability::Scroll) else { return };
        let Some(s) = dev.interface::<ei::Scroll>() else { return };
        // Both use 1/120 of a notch; ei's y grows downwards, ours upwards.
        debug_assert_eq!(WHEEL_NOTCH, 120);
        s.scroll_discrete(dx, -dy);
        d.frame(dev);
    }

    fn key(&self, hid: u16, down: bool) {
        let Some(code) = keymap::evdev_from_hid(hid) else { return };
        let d = self.devices.lock().unwrap();
        let Some(dev) = d.with(DeviceCapability::Keyboard) else { return };
        let Some(k) = dev.interface::<ei::Keyboard>() else { return };
        let state = if down { ei::keyboard::KeyState::Press } else { ei::keyboard::KeyState::Released };
        k.key(u32::from(code), state);
        d.frame(dev);
    }

    fn repeats_keys(&self) -> bool {
        // Wayland clients repeat held keys themselves.
        true
    }
}

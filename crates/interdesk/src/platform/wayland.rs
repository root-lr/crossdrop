//! Linux, Wayland.
//!
//! **Being controlled** (target side):
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
//! **Controlling others** (source side): the InputCapture portal puts
//! pointer barriers on the edges that lead to a neighbour (see
//! `set_edges`). Pushing the cursor against one activates the capture: the
//! session hears the position, decides to cross, and libei (as a receiver)
//! then delivers the relative motion, buttons, scroll and keys, which this
//! desktop no longer sees. Coming back releases the capture at the point the
//! session chose. Without neighbours, no barrier and no dialog.

use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use ashpd::desktop::remote_desktop::{ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions};
use ashpd::desktop::PersistMode;
use futures_util::StreamExt;
use reis::ei;
use reis::event::{Connection, Device, DeviceCapability, EiEvent};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{debug, info, warn};

use super::Platform;
use crate::keymap;
use crate::layout::{Rect, Side};
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
    /// Requests to the capture task.
    capture: UnboundedSender<Capture>,
    /// The compositor is capturing input for us right now.
    capturing: Arc<AtomicBool>,
}

/// What `Platform` asks of the capture task.
enum Capture {
    Edges(Vec<Side>),
    Grab(bool),
    Warp(i32, i32),
}

pub fn start(events: UnboundedSender<Input>) -> Result<Arc<dyn Platform>> {
    // The portal, the ei handshake and the device events are asynchronous,
    // and reis's event stream cannot move between threads: they all run on
    // one thread with its own runtime, which reports when the devices are
    // ready (or why not).
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (capture_tx, capture_rx) = unbounded_channel();
    let capturing = Arc::new(AtomicBool::new(false));
    let flag = capturing.clone();
    std::thread::Builder::new().name("interdesk-wayland".into()).spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(r) => r,
            Err(e) => {
                let _ = ready_tx.send(Err(e.into()));
                return;
            }
        };
        runtime.block_on(async {
            let capture = async {
                if let Err(e) = capture(events, capture_rx, flag).await {
                    warn!("Wayland: cannot control other computers from here: {e:#}");
                }
            };
            tokio::join!(drive(ready_tx), capture);
        });
    })?;
    let devices = ready_rx.recv().context("the Wayland backend stopped")??;
    info!("Wayland: this computer can be controlled");
    Ok(Arc::new(Wayland { devices, capture: capture_tx, capturing }))
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

    fn set_grab(&self, grab: bool) -> bool {
        // Only the compositor starts a capture (at a barrier): grabbing can
        // only keep the one it started.
        let ok = !grab || self.capturing.load(Ordering::Relaxed);
        let _ = self.capture.send(Capture::Grab(grab && ok));
        ok
    }

    fn warp(&self, x: i32, y: i32) {
        // Wayland moves the cursor only when the capture is released.
        let _ = self.capture.send(Capture::Warp(x, y));
    }

    fn set_edges(&self, sides: &[Side]) {
        let _ = self.capture.send(Capture::Edges(sides.to_vec()));
    }

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

/// How long the session may take to decide whether to cross once the cursor
/// hits a barrier; without a decision, the capture is released.
const DECIDE: Duration = Duration::from_millis(150);

/// After the session lets go, how long to wait for the point to put the
/// cursor back at (`warp`), before releasing without one.
const RELEASE_WAIT: Duration = Duration::from_millis(50);

/// Logical pixels of continuous scrolling sent as one wheel notch.
const PIXELS_PER_NOTCH: f32 = 10.0;

/// Input capture for the edges that lead to a neighbour.
async fn capture(
    events: UnboundedSender<Input>,
    mut cmds: UnboundedReceiver<Capture>,
    capturing: Arc<AtomicBool>,
) -> Result<()> {
    use ashpd::desktop::input_capture::{Capabilities, CreateSessionOptions, InputCapture, ReleaseOptions};

    // Nothing to do (and no dialog) until there is an edge to watch.
    let sides = loop {
        match cmds.recv().await {
            Some(Capture::Edges(sides)) if !sides.is_empty() => break sides,
            Some(_) => continue,
            None => return Ok(()),
        }
    };
    let portal = InputCapture::new()
        .await
        .context("no InputCapture portal (xdg-desktop-portal with GNOME 45+ or KDE Plasma 6.1+ is needed)")?;
    let (session, _caps) = portal
        .create_session(
            None,
            CreateSessionOptions::default().set_capabilities(Capabilities::Keyboard | Capabilities::Pointer),
        )
        .await
        .context("InputCapture portal: create session")?;
    let fd = portal.connect_to_eis(&session, Default::default()).await.context("InputCapture portal: ConnectToEIS")?;
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(true)?;
    let context = ei::Context::new(stream)?;
    let (connection, mut ei_events) = context
        .handshake_tokio("interdesk", ei::handshake::ContextType::Receiver)
        .await
        .map_err(|e| anyhow!("ei handshake: {e}"))?;

    let zones = portal.zones(&session, Default::default()).await?.response().context("InputCapture portal: zones")?;
    let regions: Vec<Rect> = zones
        .regions()
        .iter()
        .map(|r| Rect { x: r.x_offset(), y: r.y_offset(), w: r.width() as i32, h: r.height() as i32 })
        .collect();
    let barriers = barriers(&regions, &sides);
    if barriers.is_empty() {
        bail!("no monitor edge for {sides:?}");
    }
    let placed = portal
        .set_pointer_barriers(&session, &barriers, zones.zone_set(), Default::default())
        .await?
        .response()
        .context("InputCapture portal: set pointer barriers")?;
    if !placed.failed_barriers().is_empty() {
        warn!("Wayland: {} of {} edge barriers were refused", placed.failed_barriers().len(), barriers.len());
    }
    let mut activated = portal.receive_activated().await?;
    let mut deactivated = portal.receive_deactivated().await?;
    let mut zones_changed = portal.receive_zones_changed().await?;
    portal.enable(&session, Default::default()).await.context("InputCapture portal: enable")?;
    info!("Wayland: watching the {} edge(s) that lead to a neighbour", sides.len());

    // The current activation: its id and where the cursor hit the barrier.
    let mut active: Option<(u32, (f32, f32))> = None;
    let mut grabbed = false;
    // When to release if nothing else happens first, and where to.
    let mut release_at: Option<(tokio::time::Instant, Option<(f64, f64)>)> = None;
    let mut motion = (0f32, 0f32);
    let mut scroll = (0f32, 0f32);
    loop {
        let deadline = release_at.map(|(t, _)| t);
        tokio::select! {
            Some(a) = activated.next() => {
                let (Some(id), Some(pos)) = (a.activation_id(), a.cursor_position()) else { continue };
                debug!(id, ?pos, "capture activated at a barrier");
                active = Some((id, pos));
                capturing.store(true, Ordering::Relaxed);
                grabbed = false;
                motion = (0.0, 0.0);
                // Tell the session where the cursor is, at the edge; it calls
                // `set_grab(true)` if it crosses.
                let (x, y) = on_edge(&regions, pos);
                let _ = events.send(Input::Pos { x, y, pressed: false });
                release_at = Some((tokio::time::Instant::now() + DECIDE, Some(inside(&regions, pos))));
            }
            Some(d) = deactivated.next() => {
                debug!(id = ?d.activation_id(), "capture deactivated");
                active = None;
                grabbed = false;
                release_at = None;
                capturing.store(false, Ordering::Relaxed);
            }
            Some(_) = zones_changed.next() => {
                warn!("Wayland: the monitors changed; restart interdesk so that the edges follow");
            }
            Some(cmd) = cmds.recv() => match cmd {
                Capture::Grab(true) => {
                    grabbed = active.is_some();
                    release_at = None;
                }
                Capture::Grab(false) => {
                    if active.is_some() {
                        grabbed = false;
                        // The session sends where the cursor goes next.
                        release_at = Some((tokio::time::Instant::now() + RELEASE_WAIT, None));
                    }
                }
                Capture::Warp(x, y) => {
                    if active.is_some() && !grabbed {
                        release_at = Some((tokio::time::Instant::now(), Some((f64::from(x), f64::from(y)))));
                    }
                }
                Capture::Edges(_) => warn!("Wayland: the layout changed; restart interdesk so that the edges follow"),
            },
            () = sleep_until(deadline) => {
                let Some((_, pos)) = release_at.take() else { continue };
                if let Some((id, _)) = active.take() {
                    capturing.store(false, Ordering::Relaxed);
                    grabbed = false;
                    let options = ReleaseOptions::default().set_activation_id(id).set_cursor_position(pos);
                    if let Err(e) = portal.release(&session, options).await {
                        warn!("Wayland: cannot release the capture: {e}");
                    }
                    // Back inside: lets the session open that edge again.
                    if let Some((x, y)) = pos {
                        let _ = events.send(Input::Pos { x: x as i32, y: y as i32, pressed: false });
                    }
                }
            }
            Some(event) = ei_events.next() => {
                let event = event.map_err(|e| anyhow!("ei: {e}"))?;
                if let EiEvent::SeatAdded(e) = &event {
                    e.seat.bind_capabilities(
                        DeviceCapability::Pointer
                            | DeviceCapability::PointerAbsolute
                            | DeviceCapability::Button
                            | DeviceCapability::Scroll
                            | DeviceCapability::Keyboard,
                    );
                    let _ = connection.flush();
                    continue;
                }
                if !grabbed {
                    continue;
                }
                for input in captured(event, &mut motion, &mut scroll) {
                    let _ = events.send(input);
                }
            }
            else => break,
        }
    }
    Ok(())
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// Turns a captured event into interdesk input. Fractions of a pixel or of
/// a notch are kept for the next event.
fn captured(event: EiEvent, motion: &mut (f32, f32), scroll: &mut (f32, f32)) -> Vec<Input> {
    match event {
        EiEvent::PointerMotion(m) => {
            motion.0 += m.dx;
            motion.1 += m.dy;
            let (dx, dy) = (motion.0.trunc(), motion.1.trunc());
            *motion = (motion.0 - dx, motion.1 - dy);
            if dx == 0.0 && dy == 0.0 {
                return vec![];
            }
            vec![Input::Delta { dx: dx as i32, dy: dy as i32 }]
        }
        EiEvent::Button(b) => match button_from_code(b.button) {
            Some(button) => vec![Input::Button { button, down: b.state == ei::button::ButtonState::Press }],
            None => vec![],
        },
        // Ours: 1/120 notch, up and right positive; ei: down positive.
        EiEvent::ScrollDiscrete(s) => vec![Input::Wheel { dx: s.discrete_dx, dy: -s.discrete_dy }],
        EiEvent::ScrollDelta(s) => {
            let unit = WHEEL_NOTCH as f32 / PIXELS_PER_NOTCH;
            scroll.0 += s.dx * unit;
            scroll.1 += s.dy * unit;
            let (dx, dy) = (scroll.0.trunc(), scroll.1.trunc());
            *scroll = (scroll.0 - dx, scroll.1 - dy);
            if dx == 0.0 && dy == 0.0 {
                return vec![];
            }
            vec![Input::Wheel { dx: dx as i32, dy: -dy as i32 }]
        }
        EiEvent::KeyboardKey(k) => match u16::try_from(k.key).ok().and_then(keymap::hid_from_evdev) {
            Some(hid) => vec![Input::Key { hid, down: k.state == ei::keyboard::KeyState::Press }],
            None => vec![],
        },
        _ => vec![],
    }
}

fn button_from_code(code: u32) -> Option<Button> {
    [Button::Left, Button::Right, Button::Middle, Button::Back, Button::Forward]
        .into_iter()
        .find(|b| button_code(*b) == code)
}

/// The union of `regions`.
fn union(regions: &[Rect]) -> Rect {
    let x0 = regions.iter().map(|r| r.x).min().unwrap_or(0);
    let y0 = regions.iter().map(|r| r.y).min().unwrap_or(0);
    let x1 = regions.iter().map(|r| r.x + r.w).max().unwrap_or(0);
    let y1 = regions.iter().map(|r| r.y + r.h).max().unwrap_or(0);
    Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
}

/// One barrier per monitor edge that lies on the outer `sides` of the
/// desktop: the portal refuses barriers between two monitors.
fn barriers(regions: &[Rect], sides: &[Side]) -> Vec<ashpd::desktop::input_capture::Barrier> {
    use ashpd::desktop::input_capture::{Barrier, BarrierID, BarrierPosition};
    let all = union(regions);
    let mut out = vec![];
    for r in regions {
        for side in sides {
            let outer = match side {
                Side::Left => r.x == all.x,
                Side::Right => r.x + r.w == all.x + all.w,
                Side::Top => r.y == all.y,
                Side::Bottom => r.y + r.h == all.y + all.h,
            };
            if !outer {
                continue;
            }
            // Inclusive ends, on the outer side of the monitor's pixels.
            let (x1, y1, x2, y2) = match side {
                Side::Left => (r.x, r.y, r.x, r.y + r.h - 1),
                Side::Right => (r.x + r.w, r.y, r.x + r.w, r.y + r.h - 1),
                Side::Top => (r.x, r.y, r.x + r.w - 1, r.y),
                Side::Bottom => (r.x, r.y + r.h, r.x + r.w - 1, r.y + r.h),
            };
            let Some(id) = BarrierID::new(out.len() as u32 + 1) else { continue };
            out.push(Barrier::new(id, BarrierPosition::new(x1, y1, x2, y2)));
        }
    }
    out
}

/// Where the session should see the cursor: exactly on the edge it hit.
fn on_edge(regions: &[Rect], (x, y): (f32, f32)) -> (i32, i32) {
    let all = union(regions);
    ((x.round() as i32).clamp(all.x, all.right()), (y.round() as i32).clamp(all.y, all.bottom()))
}

/// A point two pixels inside the desktop from where the cursor hit the
/// barrier, to put it back when the session does not cross.
fn inside(regions: &[Rect], pos: (f32, f32)) -> (f64, f64) {
    let all = union(regions);
    let (x, y) = on_edge(regions, pos);
    let x = if x >= all.right() {
        x - 2
    } else if x <= all.x {
        x + 2
    } else {
        x
    };
    let y = if y >= all.bottom() {
        y - 2
    } else if y <= all.y {
        y + 2
    } else {
        y
    };
    (f64::from(x), f64::from(y))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAPTOP: Rect = Rect { x: 0, y: 0, w: 1366, h: 768 };
    const MONITOR: Rect = Rect { x: 1366, y: -312, w: 1920, h: 1080 };

    #[test]
    fn barriers_only_on_the_outer_edges_asked_for() {
        let b = barriers(&[LAPTOP], &[Side::Left]);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].position().as_tuple(), (0, 0, 0, 767));
        // Two monitors side by side: the right edge is the monitor's, never
        // the laptop's (that one touches the monitor).
        let b = barriers(&[LAPTOP, MONITOR], &[Side::Right, Side::Top]);
        let positions: Vec<_> = b.iter().map(|b| b.position().as_tuple()).collect();
        assert_eq!(positions, [(3286, -312, 3286, 767), (1366, -312, 3285, -312)]);
    }

    #[test]
    fn the_session_sees_the_cursor_on_the_edge() {
        // A barrier on the right is at x = 1366, one past the last pixel.
        assert_eq!(on_edge(&[LAPTOP], (1366.0, 300.4)), (1365, 300));
        assert_eq!(inside(&[LAPTOP], (1366.0, 300.0)), (1363.0, 300.0));
        assert_eq!(inside(&[LAPTOP], (0.0, 0.0)), (2.0, 2.0));
    }
}

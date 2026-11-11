//! crossdrop desktop app.
//!
//! A window over the crossdrop daemon (started in-process when none is
//! running), plus interdesk running in-process and thin "drop zone" windows
//! on the screen edges that face a neighbour: drag a file to the edge, drop
//! it, and it lands on that computer's desktop.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crossdrop_proto::ipc::{DeviceInfo, OfferInfo, PairRequestInfo};
use crossdrop_sdk::files::{send_files_with, Declined, SendEvent, SendOptions};
use crossdrop_sdk::{Client, Paths};
use interdesk::layout::{Layout, Side};
use interdesk::platform::Platform;
use interdesk::session::Input;
use serde::Serialize;
use tauri::{
    AppHandle, DragDropEvent, Emitter, Manager, PhysicalPosition, PhysicalSize, State, WebviewUrl,
    WebviewWindowBuilder, WindowEvent,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

/// Width of a drop zone at rest, and once a drag hovers it (physical pixels).
const ZONE_THIN: u32 = 4;
const ZONE_OPEN: u32 = 260;

struct Sharing {
    stop: tokio::sync::oneshot::Sender<()>,
}

struct App {
    paths: Paths,
    daemon: Mutex<Option<crossdrop_daemon::Handle>>,
    /// The OS input backend can only be started once per process; its events
    /// are forwarded to whichever interdesk session is running.
    platform: Mutex<Option<Arc<dyn Platform>>>,
    forward: Arc<Mutex<Option<UnboundedSender<Input>>>>,
    sharing: Mutex<Option<Sharing>>,
    transfers: AtomicU64,
    /// Device behind each drop zone, by side.
    /// (side, device to send to, name to show).
    zones: Mutex<Vec<(Side, String, String)>>,
}

type Res<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    format!("{e:#}")
}

async fn client(app: &App) -> Res<Client> {
    Client::connect(app.paths.clone()).await.map_err(err)
}

#[derive(Serialize)]
struct Me {
    id: String,
    name: String,
    port: u16,
}

#[derive(Serialize)]
struct Neighbor {
    device: String,
    side: Side,
    id: Option<String>,
}

#[derive(Serialize)]
struct Overview {
    me: Me,
    devices: Vec<DeviceInfo>,
    pairs: Vec<PairRequestInfo>,
    offers: Vec<OfferInfo>,
    layout: Vec<Neighbor>,
    /// interdesk.toml could not be read; the desk shows no neighbour.
    layout_error: Option<String>,
    /// The key that brings the cursor back, as people write it.
    return_key: String,
    sharing: bool,
    embedded: bool,
}

/// Everything the window shows, polled once a second.
#[tauri::command]
async fn overview(app: State<'_, App>) -> Res<Overview> {
    let mut c = client(&app).await?;
    let (id, name, port, _) = c.status().await.map_err(err)?;
    // A broken interdesk.toml must not hide everything else.
    let (layout, layout_error) = match Layout::load(&interdesk::layout_path(&app.paths)) {
        Ok(layout) => (layout, None),
        Err(e) => (Layout::default(), Some(err(e))),
    };
    let return_key = layout.return_hotkey().unwrap_or_default().to_string();
    Ok(Overview {
        me: Me { id, name, port },
        return_key,
        devices: c.devices().await.map_err(err)?,
        pairs: c.pair_requests().await.map_err(err)?,
        offers: c.offers().await.map_err(err)?,
        layout: layout.neighbors.into_iter().map(|n| Neighbor { device: n.device, side: n.side, id: n.id }).collect(),
        layout_error,
        sharing: app.sharing.lock().unwrap().is_some(),
        embedded: app.daemon.lock().unwrap().is_some(),
    })
}

#[derive(Serialize)]
struct Challenge {
    pair_id: String,
    code: String,
    peer_id: String,
    peer_name: String,
}

#[tauri::command]
async fn pair_start(app: State<'_, App>, target: String) -> Res<Challenge> {
    let (pair_id, code, peer_id, peer_name) = client(&app).await?.pair_start(&target).await.map_err(err)?;
    Ok(Challenge { pair_id, code, peer_id, peer_name })
}

/// Returns whether the other side accepted.
#[tauri::command]
async fn pair_confirm(app: State<'_, App>, pair_id: String, accept: bool) -> Res<bool> {
    let (paired, _, _) = client(&app).await?.pair_confirm(&pair_id, accept).await.map_err(err)?;
    Ok(paired)
}

#[tauri::command]
async fn pair_answer(app: State<'_, App>, pair_id: String, accept: bool) -> Res<()> {
    client(&app).await?.pair_answer(&pair_id, accept).await.map_err(err)
}

#[tauri::command]
async fn offer_answer(app: State<'_, App>, offer_id: String, accept: bool) -> Res<()> {
    client(&app).await?.offer_answer(&offer_id, accept).await.map_err(err)
}

#[tauri::command]
async fn set_permission(app: State<'_, App>, device: String, service: String, allow: bool) -> Res<()> {
    client(&app).await?.set_permission(&device, &service, allow).await.map_err(err)
}

#[tauri::command]
async fn set_auto_accept(app: State<'_, App>, device: String, enabled: bool) -> Res<()> {
    client(&app).await?.set_auto_accept(&device, enabled).await.map_err(err)
}

#[tauri::command]
async fn set_clipboard_send(app: State<'_, App>, device: String, enabled: bool) -> Res<()> {
    client(&app).await?.set_clipboard_send(&device, enabled).await.map_err(err)
}

#[tauri::command]
async fn unpair(handle: AppHandle, app: State<'_, App>, device: String) -> Res<()> {
    let mut c = client(&app).await?;
    // The layout knows devices by name; `device` may be an id.
    let name = c
        .devices()
        .await
        .map_err(err)?
        .into_iter()
        .find(|d| d.id == device || d.name.eq_ignore_ascii_case(&device))
        .map_or_else(|| device.clone(), |d| d.name);
    c.unpair(&device).await.map_err(err)?;
    // A forgotten device leaves the desk too.
    let file = interdesk::layout_path(&app.paths);
    let mut layout = Layout::load(&file).map_err(err)?;
    if layout.remove(&name) {
        layout.save(&file).map_err(err)?;
        after_layout_change(&handle, &app).await?;
    }
    Ok(())
}

#[derive(Serialize, Clone)]
struct Transfer {
    id: u64,
    device: String,
    files: usize,
    done: u64,
    total: u64,
    /// waiting | sending | done | declined | failed
    state: &'static str,
    message: String,
    desktop: bool,
}

/// Sends files in the background; progress arrives as `transfer` events.
/// `device` is an id or a name; `label` is what the transfer list shows.
#[tauri::command]
async fn send_files(
    handle: AppHandle,
    app: State<'_, App>,
    device: String,
    paths: Vec<String>,
    desktop: bool,
    label: Option<String>,
) -> Res<u64> {
    let id = app.transfers.fetch_add(1, Ordering::Relaxed) + 1;
    let paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
    let base = app.paths.clone();
    tauri::async_runtime::spawn(transfer(handle, base, id, device, label, paths, desktop));
    Ok(id)
}

async fn transfer(
    handle: AppHandle,
    base: Paths,
    id: u64,
    device: String,
    label: Option<String>,
    paths: Vec<PathBuf>,
    desktop: bool,
) {
    let shown = label.unwrap_or_else(|| device.clone());
    let mut t = Transfer { id, device: shown, files: 0, done: 0, total: 0, state: "waiting", message: String::new(), desktop };
    let _ = handle.emit("transfer", t.clone());
    let result = async {
        let mut stream = Client::connect(base).await?.open_channel(&device, "crossdrop/files").await?;
        let emitter = handle.clone();
        let mut last = std::time::Instant::now();
        let mut current = t.clone();
        let mut on_event = move |e: SendEvent<'_>| {
            match e {
                SendEvent::Waiting { files, bytes } => {
                    current.files = files;
                    current.total = bytes;
                }
                SendEvent::Accepted { resumed } => {
                    current.state = "sending";
                    current.done = resumed;
                }
                SendEvent::Progress { done, total, .. } => {
                    current.done = done;
                    current.total = total;
                    if done != total && last.elapsed().as_millis() < 120 {
                        return;
                    }
                    last = std::time::Instant::now();
                }
            }
            let _ = emitter.emit("transfer", current.clone());
        };
        let opts = SendOptions { to_desktop: desktop };
        send_files_with(&mut stream, &paths, opts, &mut on_event).await
    }
    .await;
    match result {
        Ok(report) => {
            t.state = "done";
            t.files = report.received as usize;
            t.done = report.bytes_sent + report.resumed;
            t.total = t.done;
        }
        Err(e) if e.downcast_ref::<Declined>().is_some() => t.state = "declined",
        Err(e) => {
            t.state = "failed";
            t.message = err(e);
        }
    }
    let _ = handle.emit("transfer", t);
}

/// Puts `device` on `side` of this screen, or clears the side.
#[tauri::command]
async fn layout_set(handle: AppHandle, app: State<'_, App>, side: Side, device: Option<String>) -> Res<()> {
    let file = interdesk::layout_path(&app.paths);
    let mut layout = Layout::load(&file).map_err(err)?;
    match device {
        Some(d) => {
            // Keep the id too, so a renamed device stays on the desk.
            let devices = client(&app).await?.devices().await.map_err(err)?;
            match devices.iter().find(|x| x.paired && (x.id == d || x.name.eq_ignore_ascii_case(&d))) {
                Some(x) => layout.place(&x.name, &x.id, side),
                None => layout.set(&d, side),
            }
        }
        None => layout.neighbors.retain(|n| n.side != side),
    }
    layout.save(&file).map_err(err)?;
    after_layout_change(&handle, &app).await
}

async fn after_layout_change(handle: &AppHandle, app: &App) -> Res<()> {
    refresh_zones(handle, app)?;
    if app.sharing.lock().unwrap().is_some() {
        stop_sharing(app);
        start_sharing(app).await?;
    }
    Ok(())
}

/// Turns interdesk (shared mouse and keyboard) on or off.
#[tauri::command]
async fn sharing_set(app: State<'_, App>, on: bool) -> Res<()> {
    if on {
        start_sharing(&app).await
    } else {
        stop_sharing(&app);
        Ok(())
    }
}

fn platform(app: &App) -> Res<Arc<dyn Platform>> {
    let mut slot = app.platform.lock().unwrap();
    if let Some(p) = slot.as_ref() {
        return Ok(p.clone());
    }
    let (tx, mut rx) = unbounded_channel::<Input>();
    let p = interdesk::platform::native(tx).map_err(err)?;
    let forward = app.forward.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(input) = rx.recv().await {
            if let Some(tx) = forward.lock().unwrap().as_ref() {
                let _ = tx.send(input);
            }
        }
    });
    *slot = Some(p.clone());
    Ok(p)
}

async fn start_sharing(app: &App) -> Res<()> {
    if app.sharing.lock().unwrap().is_some() {
        return Ok(());
    }
    let platform = platform(app)?;
    let layout = Layout::load(&interdesk::layout_path(&app.paths)).map_err(err)?;
    let (tx, rx) = unbounded_channel();
    *app.forward.lock().unwrap() = Some(tx);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let paths = app.paths.clone();
    tauri::async_runtime::spawn(async move {
        let shutdown = async {
            let _ = stopped.await;
        };
        if let Err(e) = interdesk::runtime::run(paths, layout, platform, rx, shutdown).await {
            tracing::warn!("interdesk stopped: {e:#}");
        }
    });
    *app.sharing.lock().unwrap() = Some(Sharing { stop });
    Ok(())
}

fn stop_sharing(app: &App) {
    *app.forward.lock().unwrap() = None;
    if let Some(s) = app.sharing.lock().unwrap().take() {
        let _ = s.stop.send(());
    }
}

/// Bounding box of all monitors, physical pixels.
fn desktop_bounds(handle: &AppHandle) -> Option<(i32, i32, i32, i32)> {
    let monitors = handle.available_monitors().ok()?;
    let mut it = monitors.iter();
    let first = it.next()?;
    let (mut x0, mut y0) = (first.position().x, first.position().y);
    let (mut x1, mut y1) = (x0 + first.size().width as i32, y0 + first.size().height as i32);
    for m in it {
        x0 = x0.min(m.position().x);
        y0 = y0.min(m.position().y);
        x1 = x1.max(m.position().x + m.size().width as i32);
        y1 = y1.max(m.position().y + m.size().height as i32);
    }
    Some((x0, y0, x1, y1))
}

/// Position and size of the drop zone on `side`.
fn zone_rect(handle: &AppHandle, side: Side, open: bool) -> Option<(PhysicalPosition<i32>, PhysicalSize<u32>)> {
    let (x0, y0, x1, y1) = desktop_bounds(handle)?;
    let t = if open { ZONE_OPEN } else { ZONE_THIN };
    let (w, h) = ((x1 - x0) as u32, (y1 - y0) as u32);
    // The open panel only covers the middle of a long edge.
    let span = |len: u32| if open { (len / 3).max(200) } else { len };
    Some(match side {
        Side::Right => (PhysicalPosition::new(x1 - t as i32, y0 + ((h - span(h)) / 2) as i32), PhysicalSize::new(t, span(h))),
        Side::Left => (PhysicalPosition::new(x0, y0 + ((h - span(h)) / 2) as i32), PhysicalSize::new(t, span(h))),
        Side::Top => (PhysicalPosition::new(x0 + ((w - span(w)) / 2) as i32, y0), PhysicalSize::new(span(w), t)),
        Side::Bottom => (PhysicalPosition::new(x0 + ((w - span(w)) / 2) as i32, y1 - t as i32), PhysicalSize::new(span(w), t)),
    })
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Left => "left",
        Side::Right => "right",
        Side::Top => "top",
        Side::Bottom => "bottom",
    }
}

/// One thin window per neighbour, on the matching screen edge.
fn refresh_zones(handle: &AppHandle, app: &App) -> Res<()> {
    for (label, window) in handle.webview_windows() {
        if label.starts_with("dz-") {
            let _ = window.close();
        }
    }
    let layout = Layout::load(&interdesk::layout_path(&app.paths)).map_err(err)?;
    let mut zones = app.zones.lock().unwrap();
    zones.clear();
    for n in layout.neighbors {
        let Some((pos, size)) = zone_rect(handle, n.side, false) else { continue };
        let init = format!(
            "window.__DROPZONE = {};",
            serde_json::json!({ "device": n.device, "side": side_name(n.side) })
        );
        let label = format!("dz-{}", side_name(n.side));
        let window = WebviewWindowBuilder::new(handle, &label, WebviewUrl::App("dropzone.html".into()))
            .title(format!("Drop to {}", n.device))
            .decorations(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .resizable(false)
            .focused(false)
            .shadow(false)
            .initialization_script(&init)
            .build()
            .map_err(err)?;
        let _ = window.set_position(pos);
        let _ = window.set_size(size);
        zones.push((n.side, n.target().to_string(), n.device.clone()));
    }
    Ok(())
}

/// Drag & drop on a drop zone: open while hovered, send on drop.
fn on_zone_event(window: &tauri::Window, event: &WindowEvent) {
    let WindowEvent::DragDrop(drag) = event else { return };
    let handle = window.app_handle();
    let app = handle.state::<App>();
    let label = window.label().trim_start_matches("dz-").to_string();
    let Some((side, device, name)) =
        app.zones.lock().unwrap().iter().find(|(s, _, _)| side_name(*s) == label).cloned()
    else {
        return;
    };
    let open = matches!(drag, DragDropEvent::Enter { .. } | DragDropEvent::Over { .. });
    if let Some((pos, size)) = zone_rect(handle, side, open) {
        let _ = window.set_position(pos);
        let _ = window.set_size(size);
    }
    if let DragDropEvent::Drop { paths, .. } = drag {
        let paths: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        let id = app.transfers.fetch_add(1, Ordering::Relaxed) + 1;
        let base = app.paths.clone();
        let handle = handle.clone();
        let paths = paths.into_iter().map(PathBuf::from).collect();
        tauri::async_runtime::spawn(transfer(handle, base, id, device, Some(name), paths, true));
    }
}

/// Logs go to the terminal in development. A released app has no terminal
/// (none at all on Windows), so they go to `app.log` in the data directory,
/// started afresh once it passes a few megabytes.
fn init_logging(paths: &Paths) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let file = if cfg!(debug_assertions) { None } else { open_log(paths).ok() };
    match file {
        Some(file) => {
            tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(Mutex::new(file)).init()
        }
        None => tracing_subscriber::fmt().with_env_filter(filter).init(),
    }
}

fn open_log(paths: &Paths) -> std::io::Result<std::fs::File> {
    const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
    std::fs::create_dir_all(&paths.home)?;
    let path = paths.home.join("app.log");
    let full = std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG_BYTES);
    std::fs::OpenOptions::new().create(true).append(!full).write(true).truncate(full).open(path)
}

fn main() {
    let paths = Paths::resolve(None).expect("no data directory");
    init_logging(&paths);
    tauri::Builder::default()
        // A second launch shows this window instead of starting a second app
        // (and a second capture of the mouse and keyboard). Registered first,
        // as the plugin asks.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        // Off until the user turns it on in the window.
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, None))
        .plugin(tauri_plugin_notification::init())
        .manage(App {
            paths,
            daemon: Mutex::new(None),
            platform: Mutex::new(None),
            forward: Arc::new(Mutex::new(None)),
            sharing: Mutex::new(None),
            transfers: AtomicU64::new(0),
            zones: Mutex::new(vec![]),
        })
        .setup(|app| {
            let handle = app.handle().clone();
            tauri::async_runtime::block_on(async {
                let state = handle.state::<App>();
                // Use the daemon already running, or start one inside the app.
                if Client::connect(state.paths.clone()).await.is_err() {
                    match crossdrop_daemon::start(crossdrop_daemon::Options::default()).await {
                        Ok(h) => *state.daemon.lock().unwrap() = Some(h),
                        Err(e) => tracing::error!("cannot start the crossdrop daemon: {e:#}"),
                    }
                }
                if let Err(e) = refresh_zones(&handle, &state) {
                    tracing::warn!("drop zones: {e}");
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label().starts_with("dz-") {
                on_zone_event(window, event);
            } else if window.label() == "main" {
                if let WindowEvent::Destroyed = event {
                    // Closing the main window quits the app (drop zones included).
                    window.app_handle().exit(0);
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            overview,
            pair_start,
            pair_confirm,
            pair_answer,
            offer_answer,
            set_permission,
            set_auto_accept,
            set_clipboard_send,
            unpair,
            send_files,
            layout_set,
            sharing_set,
        ])
        .build(tauri::generate_context!())
        .expect("error while building the app")
        .run(|handle, event| {
            if let tauri::RunEvent::Exit = event {
                let state = handle.state::<App>();
                stop_sharing(&state);
                let daemon = state.daemon.lock().unwrap().take();
                if let Some(h) = daemon {
                    tauri::async_runtime::block_on(h.shutdown_gracefully());
                }
            }
        });
}

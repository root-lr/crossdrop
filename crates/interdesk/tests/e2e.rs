//! Two crossdrop daemons, two interdesk runtimes with fake platforms: the
//! mouse leaves alpha's right edge and drives bravo.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossdrop_daemon::{start, Handle, Options};
use crossdrop_sdk::{Client, Paths};
use interdesk::layout::{Layout, Rect, Side};
use interdesk::platform::Platform;
use interdesk::protocol::Button;
use interdesk::runtime;
use interdesk::session::Input;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

#[derive(Debug, Clone, PartialEq)]
enum Call {
    Grab(bool),
    Warp(i32, i32),
    Move(i32, i32),
    Button(Button, bool),
    Key(u16, bool),
}

struct Fake {
    screen: Rect,
    calls: Mutex<Vec<Call>>,
}

impl Fake {
    fn new(w: i32, h: i32) -> Arc<Self> {
        Arc::new(Self { screen: Rect { x: 0, y: 0, w, h }, calls: Mutex::default() })
    }
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
    fn push(&self, c: Call) {
        self.calls.lock().unwrap().push(c);
    }
}

impl Platform for Fake {
    fn screen(&self) -> Rect {
        self.screen
    }
    fn set_grab(&self, grab: bool) {
        self.push(Call::Grab(grab));
    }
    fn warp(&self, x: i32, y: i32) {
        self.push(Call::Warp(x, y));
    }
    fn move_to(&self, x: i32, y: i32) {
        self.push(Call::Move(x, y));
    }
    fn button(&self, b: Button, down: bool) {
        self.push(Call::Button(b, down));
    }
    fn wheel(&self, _: i32, _: i32) {}
    fn key(&self, hid: u16, down: bool) {
        self.push(Call::Key(hid, down));
    }
}

async fn daemon(dir: &Path, name: &str) -> Handle {
    start(Options {
        home: Some(dir.join(name)),
        name: Some(name.into()),
        port: Some(0),
        mdns: Some(false),
        download_dir: Some(dir.join(name).join("dl")),
        clipboard_sync: Some(false),
        desktop_dir: None,
    })
    .await
    .unwrap()
}

fn paths(h: &Handle) -> Paths {
    Paths::resolve(Some(h.paths.home.clone())).unwrap()
}

async fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..300 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

/// The responder stores a pairing once the initiator has read its answer,
/// a moment after `pair_confirm` returned: wait for it.
async fn until_paired(c: &mut Client, peer: &Handle) {
    let id = peer.id.to_string();
    for _ in 0..250 {
        if c.devices().await.unwrap().iter().any(|d| d.id == id && d.paired) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the pairing was never stored");
}

fn launch(h: &Handle, layout: Layout, fake: Arc<Fake>) -> (UnboundedSender<Input>, tokio::sync::oneshot::Sender<()>) {
    let (tx, rx) = unbounded_channel();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let p = paths(h);
    tokio::spawn(async move {
        runtime::run(p, layout, fake, rx, async {
            let _ = stop_rx.await;
        })
        .await
        .unwrap();
    });
    (tx, stop_tx)
}

#[tokio::test]
async fn mouse_crosses_to_the_neighbour_and_back() {
    let dir = tempfile::tempdir().unwrap();
    let a = daemon(dir.path(), "alpha").await;
    let b = daemon(dir.path(), "bravo").await;

    // Pair alpha with bravo.
    let mut ca = Client::connect(paths(&a)).await.unwrap();
    let (pair_id, _, _, _) = ca.pair_start(&format!("127.0.0.1:{}", b.port)).await.unwrap();
    let confirm = tokio::spawn(async move { ca.pair_confirm(&pair_id, true).await.unwrap() });
    let mut cb = Client::connect(paths(&b)).await.unwrap();
    let req = loop {
        if let Some(r) = cb.pair_requests().await.unwrap().pop() {
            break r;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    cb.pair_answer(&req.pair_id, true).await.unwrap();
    assert!(confirm.await.unwrap().0);
    until_paired(&mut cb, &a).await;
    // bravo lets alpha drive it.
    cb.set_permission("alpha", "interdesk/input", true).await.unwrap();

    let fa = Fake::new(1920, 1080);
    let fb = Fake::new(2560, 1440);
    let mut layout = Layout::default();
    layout.set("bravo", Side::Right);
    let (_tb, stop_b) = launch(&b, Layout::default(), fb.clone());
    let (ta, stop_a) = launch(&a, layout, fa.clone());

    // Wait until alpha's link to bravo is up: moving to the edge then grabs.
    wait_for("the neighbour link", || {
        ta.send(Input::Pos { x: 1919, y: 540, pressed: false }).unwrap();
        fa.calls().contains(&Call::Grab(true))
    })
    .await;
    wait_for("bravo to receive the cursor", || fb.calls().contains(&Call::Move(0, 720))).await;

    ta.send(Input::Delta { dx: 100, dy: 0 }).unwrap();
    ta.send(Input::Button { button: Button::Left, down: true }).unwrap();
    ta.send(Input::Key { hid: 0x04, down: true }).unwrap();
    wait_for("forwarded input", || {
        let c = fb.calls();
        c.contains(&Call::Move(100, 720))
            && c.contains(&Call::Button(Button::Left, true))
            && c.contains(&Call::Key(0x04, true))
    })
    .await;

    // Back to alpha: bravo releases what was held, alpha ungrabs and warps.
    ta.send(Input::Delta { dx: -101, dy: 0 }).unwrap();
    wait_for("the cursor to come back", || fa.calls().contains(&Call::Warp(1917, 540))).await;
    assert!(fa.calls().contains(&Call::Grab(false)));
    wait_for("bravo to release keys", || {
        let c = fb.calls();
        c.contains(&Call::Key(0x04, false)) && c.contains(&Call::Button(Button::Left, false))
    })
    .await;

    let _ = stop_a.send(());
    let _ = stop_b.send(());
    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn without_permission_the_edge_stays_closed() {
    let dir = tempfile::tempdir().unwrap();
    let a = daemon(dir.path(), "alpha").await;
    let b = daemon(dir.path(), "bravo").await;
    let mut ca = Client::connect(paths(&a)).await.unwrap();
    let (pair_id, _, _, _) = ca.pair_start(&format!("127.0.0.1:{}", b.port)).await.unwrap();
    let confirm = tokio::spawn(async move { ca.pair_confirm(&pair_id, true).await.unwrap() });
    let mut cb = Client::connect(paths(&b)).await.unwrap();
    let req = loop {
        if let Some(r) = cb.pair_requests().await.unwrap().pop() {
            break r;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    cb.pair_answer(&req.pair_id, true).await.unwrap();
    confirm.await.unwrap();
    until_paired(&mut cb, &a).await;

    let fa = Fake::new(1920, 1080);
    let fb = Fake::new(1920, 1080);
    let mut layout = Layout::default();
    layout.set("bravo", Side::Right);
    let (_tb, stop_b) = launch(&b, Layout::default(), fb.clone());
    let (ta, stop_a) = launch(&a, layout, fa.clone());
    for _ in 0..20 {
        ta.send(Input::Pos { x: 1919, y: 540, pressed: false }).unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!fa.calls().contains(&Call::Grab(true)));
    assert!(fb.calls().iter().all(|c| *c == Call::Grab(false)), "{:?}", fb.calls());
    let _ = stop_a.send(());
    let _ = stop_b.send(());
    a.shutdown();
    b.shutdown();
}

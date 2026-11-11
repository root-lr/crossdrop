//! Wires the session, the platform and crossdrop together.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossdrop_proto::{read_frame, write_frame};
use crossdrop_sdk::{Client, Paths};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{debug, info, warn};

use crate::layout::Layout;
use crate::platform::Platform;
use crate::protocol::{Msg, SERVICE};
use crate::session::{Action, Input, Session};
use crate::target;

const RETRY: Duration = Duration::from_secs(3);

enum Link {
    Up {
        idx: usize,
        width: u32,
        height: u32,
        tx: UnboundedSender<Msg>,
    },
    /// The neighbour's desktop changed size.
    Resized {
        idx: usize,
        width: u32,
        height: u32,
    },
    /// The neighbour is driven by another computer right now.
    Busy {
        idx: usize,
    },
    Down {
        idx: usize,
    },
}

/// Runs interdesk until `shutdown` resolves.
pub async fn run(
    paths: Paths,
    layout: Layout,
    platform: Arc<dyn Platform>,
    mut input: UnboundedReceiver<Input>,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let mut tasks = vec![tokio::spawn(serve_targets(paths.clone(), platform.clone()))];

    let (link_tx, mut links) = unbounded_channel();
    for (idx, n) in layout.neighbors.iter().enumerate() {
        info!(device = %n.device, side = %n.side, "neighbour");
        tasks.push(tokio::spawn(keep_linked(paths.clone(), idx, n.target().to_string(), link_tx.clone())));
    }
    drop(link_tx);

    let mut session = Session::new(platform.screen(), layout.neighbors.iter().map(|n| n.side).collect());
    session.set_return_key(layout.return_hotkey()?);
    for (idx, n) in layout.neighbors.iter().enumerate() {
        let (from, to) = n.edge();
        session.set_edge(idx, from, to);
    }
    let mut writers: Vec<Option<UnboundedSender<Msg>>> = vec![None; layout.neighbors.len()];
    platform.set_grab(false);

    tokio::pin!(shutdown);
    // This screen can change too (resolution, monitors).
    let mut screen_check = tokio::time::interval(target::SCREEN_CHECK);
    screen_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let actions = tokio::select! {
            Some(event) = input.recv() => session.on_input(event),
            _ = screen_check.tick() => {
                session.set_screen(platform.screen());
                vec![]
            }
            Some(link) = links.recv() => match link {
                Link::Up { idx, width, height, tx } => {
                    info!(device = %layout.neighbors[idx].device, width, height, "neighbour ready");
                    writers[idx] = Some(tx);
                    session.set_screen(platform.screen());
                    session.connected(idx, width, height);
                    vec![]
                }
                Link::Resized { idx, width, height } => {
                    info!(device = %layout.neighbors[idx].device, width, height, "neighbour resized");
                    session.connected(idx, width, height);
                    vec![]
                }
                Link::Busy { idx } => {
                    info!(device = %layout.neighbors[idx].device, "neighbour is driven by another computer");
                    session.refused(idx)
                }
                Link::Down { idx } => {
                    writers[idx] = None;
                    session.disconnected(idx)
                }
            },
            _ = &mut shutdown => break,
        };
        for action in actions {
            match action {
                Action::Grab(true) if !platform.set_grab(true) => {
                    // Nothing was sent yet: stay here, as if the edge were closed.
                    session.cancel_crossing();
                    break;
                }
                Action::Grab(on) => {
                    platform.set_grab(on);
                }
                Action::Warp(x, y) => platform.warp(x, y),
                Action::Send(idx, msg) => {
                    if let Some(tx) = &writers[idx] {
                        let _ = tx.send(msg);
                    }
                }
            }
        }
    }

    if let Some(idx) = session.remote() {
        if let Some(tx) = &writers[idx] {
            let _ = tx.send(Msg::Leave);
        }
    }
    platform.set_grab(false);
    // Give writers a moment to flush the final Leave.
    tokio::time::sleep(Duration::from_millis(50)).await;
    for t in tasks {
        t.abort();
    }
    Ok(())
}

/// Target role: accept sources for as long as the daemon is there.
async fn serve_targets(paths: Paths, platform: Arc<dyn Platform>) {
    loop {
        match Client::connect(paths.clone()).await {
            Ok(client) => match client.register(SERVICE).await {
                Ok(mut reg) => {
                    info!("ready to be controlled (service {SERVICE})");
                    let control = Arc::new(target::Control::default());
                    while let Ok(Some(incoming)) = reg.next().await {
                        let platform = platform.clone();
                        let control = control.clone();
                        let from = device_name(&paths, &incoming.device).await;
                        tokio::spawn(async move {
                            match incoming.accept().await {
                                Ok(stream) => {
                                    if let Err(e) = target::serve(stream, platform, control, &from).await {
                                        debug!("source {from}: {e:#}");
                                    }
                                }
                                Err(e) => debug!("attach failed: {e:#}"),
                            }
                        });
                    }
                    warn!("lost the crossdrop daemon; retrying");
                }
                Err(e) => warn!("cannot register {SERVICE}: {e:#}"),
            },
            Err(e) => warn!("{e:#}"),
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// Name of a paired device, or its id if unknown.
async fn device_name(paths: &Paths, id: &str) -> String {
    let devices = match Client::connect(paths.clone()).await {
        Ok(mut c) => c.devices().await.unwrap_or_default(),
        Err(_) => vec![],
    };
    devices.into_iter().find(|d| d.id == id).map(|d| d.name).unwrap_or_else(|| id.to_string())
}

/// Source role: keep a channel open to one neighbour, reconnecting as needed.
async fn keep_linked(paths: Paths, idx: usize, device: String, links: UnboundedSender<Link>) {
    let mut last_error = String::new();
    loop {
        match link_once(&paths, idx, &device, &links).await {
            Ok(()) => last_error.clear(),
            Err(e) => {
                let msg = format!("{e:#}");
                if msg != last_error {
                    if msg.contains("permission denied") {
                        warn!("{device} refuses control from this device. On {device}, run: crossdrop allow <this device> input");
                    } else {
                        warn!("{device} unavailable: {msg}");
                    }
                    last_error = msg;
                }
            }
        }
        if links.send(Link::Down { idx }).is_err() {
            return;
        }
        tokio::time::sleep(RETRY).await;
    }
}

async fn link_once(paths: &Paths, idx: usize, device: &str, links: &UnboundedSender<Link>) -> Result<()> {
    let stream = Client::connect(paths.clone()).await?.open_channel(device, SERVICE).await?;
    let (mut rd, mut wr) = tokio::io::split(stream);
    let Msg::Screen { width, height } = read_frame(&mut rd).await? else {
        anyhow::bail!("{device} did not send its screen size");
    };
    let (tx, mut rx) = unbounded_channel::<Msg>();
    links.send(Link::Up { idx, width, height, tx })?;
    let writer = forward(&mut rx, &mut wr);
    let reader = async {
        // The target only reports size changes; anything else ends the link.
        while let Ok(msg) = read_frame::<_, Msg>(&mut rd).await {
            match msg {
                Msg::Screen { width, height } => links.send(Link::Resized { idx, width, height })?,
                Msg::Busy => links.send(Link::Busy { idx })?,
                _ => {}
            }
        }
        anyhow::Ok(())
    };
    tokio::select! {
        r = writer => r,
        r = reader => r,
    }
}

/// Writes the source's messages to the target. When the link falls behind,
/// moves waiting in the queue are merged into the last one: positions are
/// absolute, so the cursor goes straight to where the mouse is instead of
/// replaying its path late. Everything else keeps its order.
async fn forward<W>(rx: &mut UnboundedReceiver<Msg>, wr: &mut W) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut next = None;
    loop {
        let mut msg = match next.take() {
            Some(msg) => msg,
            None => match rx.recv().await {
                Some(msg) => msg,
                None => return Ok(()),
            },
        };
        if matches!(msg, Msg::Move { .. }) {
            while let Ok(queued) = rx.try_recv() {
                if matches!(queued, Msg::Move { .. }) {
                    msg = queued;
                } else {
                    next = Some(queued);
                    break;
                }
            }
        }
        write_frame(wr, &msg).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Button;

    #[tokio::test]
    async fn queued_moves_are_merged_but_order_is_kept() {
        let (tx, mut rx) = unbounded_channel();
        for msg in [
            Msg::Move { x: 1, y: 1 },
            Msg::Move { x: 2, y: 2 },
            Msg::Move { x: 3, y: 3 },
            Msg::Button { button: Button::Left, down: true },
            Msg::Move { x: 4, y: 4 },
            Msg::Move { x: 5, y: 5 },
            Msg::Key { hid: 4, down: true },
        ] {
            tx.send(msg).unwrap();
        }
        drop(tx);
        let (mut a, mut b) = tokio::io::duplex(4096);
        forward(&mut rx, &mut a).await.unwrap();
        drop(a);
        let mut got = Vec::new();
        while let Ok(msg) = read_frame::<_, Msg>(&mut b).await {
            got.push(msg);
        }
        assert_eq!(
            got,
            [
                Msg::Move { x: 3, y: 3 },
                Msg::Button { button: Button::Left, down: true },
                Msg::Move { x: 5, y: 5 },
                Msg::Key { hid: 4, down: true },
            ]
        );
    }
}

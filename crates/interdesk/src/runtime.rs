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
    Up { idx: usize, width: u32, height: u32, tx: UnboundedSender<Msg> },
    Down { idx: usize },
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
        tasks.push(tokio::spawn(keep_linked(paths.clone(), idx, n.device.clone(), link_tx.clone())));
    }
    drop(link_tx);

    let mut session = Session::new(platform.screen(), layout.neighbors.iter().map(|n| n.side).collect());
    let mut writers: Vec<Option<UnboundedSender<Msg>>> = vec![None; layout.neighbors.len()];
    platform.set_grab(false);

    tokio::pin!(shutdown);
    loop {
        let actions = tokio::select! {
            Some(event) = input.recv() => session.on_input(event),
            Some(link) = links.recv() => match link {
                Link::Up { idx, width, height, tx } => {
                    info!(device = %layout.neighbors[idx].device, width, height, "neighbour ready");
                    writers[idx] = Some(tx);
                    session.set_screen(platform.screen());
                    session.connected(idx, width, height);
                    vec![]
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
                Action::Grab(on) => platform.set_grab(on),
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
                    while let Ok(Some(incoming)) = reg.next().await {
                        let platform = platform.clone();
                        let from = device_name(&paths, &incoming.device).await;
                        tokio::spawn(async move {
                            match incoming.accept().await {
                                Ok(stream) => {
                                    if let Err(e) = target::serve(stream, platform, &from).await {
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
    let writer = async {
        while let Some(msg) = rx.recv().await {
            write_frame(&mut wr, &msg).await?;
        }
        anyhow::Ok(())
    };
    let reader = async {
        // The target sends nothing more today; a read result means it is gone.
        let _ = read_frame::<_, Msg>(&mut rd).await;
        anyhow::Ok(())
    };
    tokio::select! {
        r = writer => r,
        r = reader => r,
    }
}

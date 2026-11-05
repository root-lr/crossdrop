//! `crossdrop` command-line interface.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use crossdrop_proto::ipc::{OfferInfo, PairRequestInfo};
use crossdrop_sdk::files::{send_files, Declined, SendEvent};
use crossdrop_sdk::{Client, Paths};

#[derive(Parser)]
#[command(name = "crossdrop", version, about = "Share files and more between your devices, end-to-end encrypted.")]
struct Cli {
    /// Data directory (default: $CROSSDROP_HOME or the OS config directory).
    #[arg(long, global = true, value_name = "DIR")]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon in the foreground.
    Daemon {
        /// Name shown to other devices.
        #[arg(long)]
        name: Option<String>,
        /// UDP port (0 = random).
        #[arg(long)]
        port: Option<u16>,
        /// Do not announce or discover devices with mDNS.
        #[arg(long)]
        no_mdns: bool,
        /// Where received files are stored.
        #[arg(long, value_name = "DIR")]
        download_dir: Option<PathBuf>,
    },
    /// Show this device's id, name and port.
    Status,
    /// List paired and nearby devices.
    #[command(alias = "ls")]
    Devices,
    /// Pair with a device: its name, id prefix, or ip:port.
    Pair {
        target: String,
        /// Skip the code confirmation prompt.
        #[arg(long, short)]
        yes: bool,
    },
    /// List pairing requests and incoming transfers waiting for an answer.
    Requests,
    /// Accept a pairing request or an incoming transfer (the only one, if no id is given).
    Accept {
        id: Option<String>,
        #[arg(long, short)]
        yes: bool,
    },
    /// Reject a pairing request or an incoming transfer.
    Reject { id: Option<String> },
    /// Send files and folders to a paired device. Interrupted transfers resume.
    Send {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Target device: name or id prefix.
        #[arg(long, short)]
        to: String,
    },
    /// Share text with your devices' clipboards.
    #[command(subcommand)]
    Clip(Clip),
    /// Store files from a device without asking (`--off` to ask again).
    AutoAccept {
        device: String,
        #[arg(long)]
        off: bool,
    },
    /// Allow a paired device to use a service (files, clipboard, input, or a full name).
    Allow { device: String, service: String },
    /// Revoke a service permission.
    Deny { device: String, service: String },
    /// Forget a paired device.
    Unpair { device: String },
}

#[derive(Subcommand)]
enum Clip {
    /// Send text (argument or standard input) to one device, or to every clipboard peer.
    Send {
        text: Option<String>,
        #[arg(long, short)]
        to: Option<String>,
    },
    /// Print the last text another device shared.
    Get,
}

fn service_name(s: &str) -> String {
    match s {
        "files" => "crossdrop/files".into(),
        "clipboard" => "crossdrop/clipboard".into(),
        "input" => "interdesk/input".into(),
        other => other.into(),
    }
}

fn confirm(question: &str) -> Result<bool> {
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes" | "o" | "oui"))
}

fn short(id: &str) -> &str {
    &id[..id.len().min(9)]
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Cli::parse()).await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let paths = Paths::resolve(cli.home.clone())?;
    let connect = || Client::connect(paths.clone());

    match cli.command {
        Command::Daemon { name, port, no_mdns, download_dir } => {
            let filter = tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
            tracing_subscriber::fmt().with_env_filter(filter).init();
            crossdrop_daemon::run(crossdrop_daemon::Options {
                home: cli.home,
                name,
                port,
                mdns: no_mdns.then_some(false),
                download_dir,
                clipboard_sync: None,
            })
            .await?;
        }

        Command::Status => {
            let (id, name, port, version) = connect().await?.status().await?;
            println!("name     {name}");
            println!("id       {id}");
            println!("port     {port}/udp");
            println!("version  {version}");
        }

        Command::Devices => {
            let devices = connect().await?.devices().await?;
            if devices.is_empty() {
                println!("No devices yet. Devices on your network appear here; pair one with `crossdrop pair <name>`.");
            }
            for d in devices {
                let state = match (d.paired, d.online) {
                    (true, true) => "paired, online",
                    (true, false) => "paired",
                    (false, _) => "nearby",
                };
                println!("{:<20} {}  {:<15} {}", d.name, short(&d.id), state, d.addrs.join(", "));
                if d.paired {
                    let auto = if d.auto_accept { "  (files accepted automatically)" } else { "" };
                    println!("{:<20} allowed: {}{auto}", "", d.permissions.join(", "));
                }
            }
        }

        Command::Pair { target, yes } => {
            let mut client = connect().await?;
            let (pair_id, code, peer_id, peer_name) = client.pair_start(&target).await?;
            println!("Pairing with {peer_name} ({})", short(&peer_id));
            println!();
            println!("    Code: {code}");
            println!();
            let ok = yes || confirm(&format!("Does {peer_name} show the same code?"))?;
            if !ok {
                client.pair_confirm(&pair_id, false).await?;
                bail!("pairing cancelled");
            }
            println!("Waiting for the other device to accept (`crossdrop accept` there)…");
            let (paired, peer_id, peer_name) = client.pair_confirm(&pair_id, true).await?;
            if paired {
                println!(
                    "Paired with {peer_name} ({}). Send a file with `crossdrop send FILE --to {peer_name}`.",
                    short(&peer_id)
                );
            } else {
                bail!("{peer_name} refused the pairing");
            }
        }

        Command::Requests => {
            let mut client = connect().await?;
            let pairs = client.pair_requests().await?;
            let offers = client.offers().await?;
            if pairs.is_empty() && offers.is_empty() {
                println!("Nothing waiting.");
            }
            for r in pairs {
                println!("{}  pairing   {} ({})  code {}", &r.pair_id[..8], r.peer_name, short(&r.peer_id), r.sas);
            }
            for o in offers {
                println!(
                    "{}  files     {} wants to send {} file(s), {}: {}{}",
                    &o.offer_id[..8],
                    o.peer_name,
                    o.files,
                    human(o.total_bytes),
                    o.preview.join(", "),
                    if (o.files as usize) > o.preview.len() { ", …" } else { "" }
                );
            }
        }

        Command::Accept { id, yes } => match pick_request(&mut connect().await?, id).await? {
            Pending::Pair(request) => {
                let mut client = connect().await?;
                println!("{} ({}) wants to pair.", request.peer_name, short(&request.peer_id));
                println!();
                println!("    Code: {}", request.sas);
                println!();
                let ok = yes || confirm("Is this the code shown on the other device?")?;
                client.pair_answer(&request.pair_id, ok).await?;
                if ok {
                    println!(
                        "Paired with {}. It may send you files; allow more with `crossdrop allow`.",
                        request.peer_name
                    );
                } else {
                    println!("Rejected.");
                }
            }
            Pending::Offer(offer) => {
                connect().await?.offer_answer(&offer.offer_id, true).await?;
                println!("Receiving {} file(s) from {}.", offer.files, offer.peer_name);
            }
        },

        Command::Reject { id } => match pick_request(&mut connect().await?, id).await? {
            Pending::Pair(request) => {
                connect().await?.pair_answer(&request.pair_id, false).await?;
                println!("Rejected pairing with {}.", request.peer_name);
            }
            Pending::Offer(offer) => {
                connect().await?.offer_answer(&offer.offer_id, false).await?;
                println!("Declined the files from {}.", offer.peer_name);
            }
        },

        Command::Send { files, to } => {
            let mut stream = connect().await?.open_channel(&to, "crossdrop/files").await?;
            let started = Instant::now();
            let mut last = Instant::now();
            let mut on_event = |event: SendEvent<'_>| match event {
                SendEvent::Waiting { files, bytes } => {
                    eprintln!("Offering {files} file(s), {} to {to}…", human(bytes));
                }
                SendEvent::Accepted { resumed } => {
                    if resumed > 0 {
                        eprintln!("Accepted. Resuming: {} already there.", human(resumed));
                    }
                }
                SendEvent::Progress { path, done, total } => {
                    if done == total || last.elapsed().as_millis() > 100 {
                        last = Instant::now();
                        let pct = (done * 100).checked_div(total).unwrap_or(100);
                        eprint!("\r\x1b[K{pct:>3}%  {} / {}  {path}", human(done), human(total));
                        if done == total {
                            eprintln!();
                        }
                    }
                }
            };
            let report = match send_files(&mut stream, &files, &mut on_event).await {
                Ok(r) => r,
                Err(e) if e.downcast_ref::<Declined>().is_some() => bail!("{to} declined the files"),
                Err(e) => return Err(e),
            };
            let secs = started.elapsed().as_secs_f64().max(0.001);
            println!(
                "Sent {} file(s), {} in {secs:.1}s ({}/s).",
                report.received,
                human(report.bytes_sent),
                human((report.bytes_sent as f64 / secs) as u64)
            );
        }

        Command::Clip(Clip::Send { text, to }) => {
            let text = match text {
                Some(t) => t,
                None => {
                    let mut buf = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
                    buf
                }
            };
            if text.is_empty() {
                bail!("nothing to send");
            }
            let n = connect().await?.clipboard_send(&text, to.as_deref()).await?;
            println!("Shared with {n} device(s).");
        }

        Command::Clip(Clip::Get) => match connect().await?.clipboard_get().await? {
            Some((text, _from)) => print!("{text}"),
            None => bail!("nothing received yet"),
        },

        Command::AutoAccept { device, off } => {
            connect().await?.set_auto_accept(&device, !off).await?;
            if off {
                println!("Files from {device} will wait for `crossdrop accept`.");
            } else {
                println!("Files from {device} are now accepted automatically.");
            }
        }

        Command::Allow { device, service } => {
            let service = service_name(&service);
            connect().await?.set_permission(&device, &service, true).await?;
            println!("{device} may now use {service}.");
        }

        Command::Deny { device, service } => {
            let service = service_name(&service);
            connect().await?.set_permission(&device, &service, false).await?;
            println!("{device} may no longer use {service}.");
        }

        Command::Unpair { device } => {
            connect().await?.unpair(&device).await?;
            println!("Forgot {device}.");
        }
    }
    Ok(())
}

enum Pending {
    Pair(PairRequestInfo),
    Offer(OfferInfo),
}

/// Finds a pending pairing request or transfer by id prefix, or the only one.
async fn pick_request(client: &mut Client, id: Option<String>) -> Result<Pending> {
    let mut all: Vec<(String, Pending)> = Vec::new();
    for r in client.pair_requests().await? {
        all.push((r.pair_id.clone(), Pending::Pair(r)));
    }
    for o in client.offers().await? {
        all.push((o.offer_id.clone(), Pending::Offer(o)));
    }
    if let Some(id) = id {
        all.retain(|(key, _)| key.starts_with(&id));
    }
    match all.len() {
        0 => bail!("nothing waiting with that id; see `crossdrop requests`"),
        1 => Ok(all.remove(0).1),
        _ => bail!("several requests are waiting; pass an id from `crossdrop requests`"),
    }
}

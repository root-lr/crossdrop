//! `crossdrop` command-line interface.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use crossdrop_sdk::{files::send_files, Client, Paths};

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
    /// List pairing requests waiting for an answer.
    Requests,
    /// Accept a pairing request (the only one, if no id is given).
    Accept {
        pair_id: Option<String>,
        #[arg(long, short)]
        yes: bool,
    },
    /// Reject a pairing request.
    Reject { pair_id: Option<String> },
    /// Send files to a paired device.
    Send {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Target device: name or id prefix.
        #[arg(long, short)]
        to: String,
    },
    /// Allow a paired device to use a service (files, clipboard, input, or a full name).
    Allow { device: String, service: String },
    /// Revoke a service permission.
    Deny { device: String, service: String },
    /// Forget a paired device.
    Unpair { device: String },
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
                    println!("{:<20} allowed: {}", "", d.permissions.join(", "));
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
            let requests = connect().await?.pair_requests().await?;
            if requests.is_empty() {
                println!("No pending pairing request.");
            }
            for r in requests {
                println!("{}  {} ({})  code {}", r.pair_id, r.peer_name, short(&r.peer_id), r.sas);
            }
        }

        Command::Accept { pair_id, yes } => {
            let mut client = connect().await?;
            let request = pick_request(&mut client, pair_id).await?;
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

        Command::Reject { pair_id } => {
            let mut client = connect().await?;
            let request = pick_request(&mut client, pair_id).await?;
            client.pair_answer(&request.pair_id, false).await?;
            println!("Rejected {}.", request.peer_name);
        }

        Command::Send { files, to } => {
            let mut stream = connect().await?.open_channel(&to, "crossdrop/files").await?;
            let total: u64 = files.iter().filter_map(|f| std::fs::metadata(f).ok()).map(|m| m.len()).sum();
            let started = Instant::now();
            let mut last = Instant::now();
            let mut progress = |name: &str, sent: u64, size: u64| {
                if sent == size || last.elapsed().as_millis() > 100 {
                    last = Instant::now();
                    let pct = (sent * 100).checked_div(size).unwrap_or(100);
                    eprint!("\r\x1b[K{name}  {pct:>3}%  {}", human(sent));
                    if sent == size {
                        eprintln!();
                    }
                }
            };
            let n = send_files(&mut stream, &files, &mut progress).await?;
            let secs = started.elapsed().as_secs_f64().max(0.001);
            println!("Sent {n} file(s), {} in {secs:.1}s ({}/s).", human(total), human((total as f64 / secs) as u64));
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

async fn pick_request(client: &mut Client, pair_id: Option<String>) -> Result<crossdrop_proto::ipc::PairRequestInfo> {
    let mut requests = client.pair_requests().await?;
    match pair_id {
        Some(id) => requests
            .into_iter()
            .find(|r| r.pair_id.starts_with(&id))
            .ok_or_else(|| anyhow::anyhow!("no pairing request {id}")),
        None => match requests.len() {
            0 => bail!("no pending pairing request"),
            1 => Ok(requests.remove(0)),
            _ => bail!("several requests are pending; pass one of the ids from `crossdrop requests`"),
        },
    }
}

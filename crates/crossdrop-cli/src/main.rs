//! `crossdrop` command-line interface.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use crossdrop_proto::ipc::{OfferInfo, PairRequestInfo};
use crossdrop_sdk::files::{send_files, Declined, SendEvent};
use crossdrop_sdk::{Client, Paths};

/// `print!` / `println!` that end the command quietly when standard output
/// is closed early (`crossdrop devices | head -1`), instead of panicking with
/// "failed printing to stdout: Broken pipe". (Restoring the default SIGPIPE
/// would do it too, but would also kill `crossdrop daemon` whenever a client
/// hangs up on its socket.)
macro_rules! out {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        if let Err(e) = write!(std::io::stdout(), $($arg)*) {
            exit_on_closed_stdout(e);
        }
    }};
}

macro_rules! outln {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        if let Err(e) = writeln!(std::io::stdout(), $($arg)*) {
            exit_on_closed_stdout(e);
        }
    }};
}

fn exit_on_closed_stdout(e: std::io::Error) {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        std::process::exit(0);
    }
    eprintln!("error: cannot write to standard output: {e}");
    std::process::exit(1);
}

#[derive(Parser)]
#[command(name = "crossdrop", version, about = "Share files and more between your devices, end-to-end encrypted.")]
struct Cli {
    /// Data directory (default: $CROSSDROP_HOME or the OS config directory).
    #[arg(long, global = true, value_name = "DIR")]
    home: Option<PathBuf>,

    /// Print `status`, `devices` and `requests` as JSON, for scripts.
    #[arg(long, global = true)]
    json: bool,

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
        /// Do not watch or set the system clipboard (`crossdrop clip` still works).
        #[arg(long)]
        no_clipboard: bool,
        /// Where received files are stored.
        #[arg(long, value_name = "DIR")]
        download_dir: Option<PathBuf>,
        /// Where files dropped from a neighbouring screen land (default: the desktop).
        #[arg(long, value_name = "DIR")]
        desktop_dir: Option<PathBuf>,
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
        /// Accept every incoming transfer. Pairing requests are left alone:
        /// each code must be checked.
        #[arg(long, conflicts_with = "id")]
        all: bool,
    },
    /// Reject a pairing request or an incoming transfer.
    Reject {
        id: Option<String>,
        /// Reject every pairing request and incoming transfer.
        #[arg(long, conflicts_with = "id")]
        all: bool,
    },
    /// Send files and folders to a paired device. Interrupted transfers resume.
    Send {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Target device: name or id prefix. Repeat to send to several
        /// devices, one after the other.
        #[arg(long, short, required = true)]
        to: Vec<String>,
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
    /// Print a completion script for your shell.
    ///
    /// bash: `crossdrop completions bash > ~/.local/share/bash-completion/completions/crossdrop`;
    /// zsh: `crossdrop completions zsh > "${fpath[1]}/_crossdrop"`;
    /// fish: `crossdrop completions fish > ~/.config/fish/completions/crossdrop.fish`.
    Completions { shell: clap_complete::Shell },
    /// Check this device's setup and say what to fix: data directory,
    /// daemon, identity, paired devices, and the macOS Accessibility
    /// permission interdesk needs.
    Doctor,
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
    /// Send this device's copies to a device (`--off` to stop), whether or
    /// not it may write to this clipboard. By default copies go to the
    /// devices allowed `clipboard`.
    Share {
        device: String,
        #[arg(long)]
        off: bool,
    },
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

/// `, 12.3 MiB/s, 1:05 left` once enough has been sent to tell.
fn speed(sent: u64, secs: f64, left: u64) -> String {
    if secs < 0.5 || sent == 0 {
        return String::new();
    }
    let rate = sent as f64 / secs;
    let eta = (left as f64 / rate).round() as u64;
    let eta = match eta {
        0 => String::new(),
        s if s < 3600 => format!(", {}:{:02} left", s / 60, s % 60),
        s => format!(", {}:{:02}:{:02} left", s / 3600, s / 60 % 60, s % 60),
    };
    format!(", {}/s{eta}", human(rate as u64))
}

/// A size in binary units (1 KiB = 1024 B), named as such.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
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
        Command::Daemon { name, port, no_mdns, no_clipboard, download_dir, desktop_dir } => {
            let filter = tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
            tracing_subscriber::fmt().with_env_filter(filter).init();
            crossdrop_daemon::run(crossdrop_daemon::Options {
                home: cli.home,
                name,
                port,
                mdns: no_mdns.then_some(false),
                download_dir,
                clipboard_sync: no_clipboard.then_some(false),
                desktop_dir,
            })
            .await?;
        }

        Command::Status => {
            let (id, name, port, version) = connect().await?.status().await?;
            if cli.json {
                print_json(serde_json::json!({ "id": id, "name": name, "port": port, "version": version }))?;
                return Ok(());
            }
            outln!("name     {name}");
            outln!("id       {id}");
            outln!("port     {port}/udp");
            outln!("version  {version}");
            if version != env!("CARGO_PKG_VERSION") {
                eprintln!(
                    "warning: the daemon runs {version} but this command is {}; restart the daemon to use the same version",
                    env!("CARGO_PKG_VERSION")
                );
            }
        }

        Command::Devices => {
            let devices = connect().await?.devices().await?;
            if cli.json {
                return print_json(serde_json::to_value(&devices)?);
            }
            if devices.is_empty() {
                outln!("No devices yet. Devices on your network appear here; pair one with `crossdrop pair <name>`.");
            }
            for d in devices {
                let state = match (d.paired, d.online) {
                    (true, true) => "paired, online",
                    (true, false) => "paired",
                    (false, _) => "nearby",
                };
                outln!("{:<20} {}  {:<15} {}", d.name, short(&d.id), state, d.addrs.join(", "));
                if d.paired {
                    let auto = if d.auto_accept { "  (files accepted automatically)" } else { "" };
                    outln!("{:<20} allowed: {}{auto}", "", d.permissions.join(", "));
                    if d.clipboard_send {
                        outln!("{:<20} gets this device's clipboard copies", "");
                    }
                }
            }
        }

        Command::Pair { target, yes } => {
            let mut client = connect().await?;
            let (pair_id, code, peer_id, peer_name) = client.pair_start(&target).await?;
            outln!("Pairing with {peer_name} ({})", short(&peer_id));
            outln!();
            outln!("    Code: {code}");
            outln!();
            let ok = yes || confirm(&format!("Does {peer_name} show the same code?"))?;
            if !ok {
                client.pair_confirm(&pair_id, false).await?;
                bail!("pairing cancelled");
            }
            outln!("Waiting for the other device to accept (`crossdrop accept` there)…");
            let (paired, peer_id, peer_name) = client.pair_confirm(&pair_id, true).await?;
            if paired {
                outln!(
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
            if cli.json {
                return print_json(serde_json::json!({ "pairing": pairs, "transfers": offers }));
            }
            if pairs.is_empty() && offers.is_empty() {
                outln!("Nothing waiting.");
            }
            for r in pairs {
                outln!("{}  pairing   {} ({})  code {}", &r.pair_id[..8], r.peer_name, short(&r.peer_id), r.sas);
            }
            for o in offers {
                outln!(
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

        Command::Accept { all: true, .. } => {
            let mut client = connect().await?;
            let offers = client.offers().await?;
            if offers.is_empty() {
                bail!("no incoming transfer is waiting");
            }
            for offer in offers {
                client.offer_answer(&offer.offer_id, true).await?;
                outln!("Receiving {} file(s) from {}.", offer.files, offer.peer_name);
            }
            let pairs = client.pair_requests().await?.len();
            if pairs > 0 {
                outln!("{pairs} pairing request(s) still waiting: accept each one after checking its code.");
            }
        }

        Command::Reject { all: true, .. } => {
            let mut client = connect().await?;
            let (pairs, offers) = (client.pair_requests().await?, client.offers().await?);
            if pairs.is_empty() && offers.is_empty() {
                bail!("nothing waiting");
            }
            for request in pairs {
                client.pair_answer(&request.pair_id, false).await?;
                outln!("Rejected pairing with {}.", request.peer_name);
            }
            for offer in offers {
                client.offer_answer(&offer.offer_id, false).await?;
                outln!("Declined the files from {}.", offer.peer_name);
            }
        }

        Command::Accept { id, yes, .. } => match pick_request(&mut connect().await?, id).await? {
            Pending::Pair(request) => {
                let mut client = connect().await?;
                outln!("{} ({}) wants to pair.", request.peer_name, short(&request.peer_id));
                outln!();
                outln!("    Code: {}", request.sas);
                outln!();
                let ok = yes || confirm("Is this the code shown on the other device?")?;
                client.pair_answer(&request.pair_id, ok).await?;
                if ok {
                    outln!(
                        "Paired with {}. It may send you files; allow more with `crossdrop allow`.",
                        request.peer_name
                    );
                } else {
                    outln!("Rejected.");
                }
            }
            Pending::Offer(offer) => {
                connect().await?.offer_answer(&offer.offer_id, true).await?;
                outln!("Receiving {} file(s) from {}.", offer.files, offer.peer_name);
            }
        },

        Command::Reject { id, .. } => match pick_request(&mut connect().await?, id).await? {
            Pending::Pair(request) => {
                connect().await?.pair_answer(&request.pair_id, false).await?;
                outln!("Rejected pairing with {}.", request.peer_name);
            }
            Pending::Offer(offer) => {
                connect().await?.offer_answer(&offer.offer_id, false).await?;
                outln!("Declined the files from {}.", offer.peer_name);
            }
        },

        Command::Send { files, to } => {
            if let [one] = to.as_slice() {
                return send_to(&paths, one, &files).await;
            }
            let mut failed = 0;
            for device in &to {
                if let Err(e) = send_to(&paths, device, &files).await {
                    eprintln!("error: {device}: {e:#}");
                    failed += 1;
                }
            }
            if failed > 0 {
                bail!("could not send to {failed} of {} devices", to.len());
            }
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
            outln!("Shared with {n} device(s).");
        }

        Command::Clip(Clip::Get) => match connect().await?.clipboard_get().await? {
            Some((text, _from)) => out!("{text}"),
            None => bail!("nothing received yet"),
        },

        Command::Clip(Clip::Share { device, off }) => {
            connect().await?.set_clipboard_send(&device, !off).await?;
            if off {
                outln!("Copies made here are no longer sent to {device}.");
            } else {
                outln!("Copies made here are now sent to {device}.");
            }
        }

        Command::AutoAccept { device, off } => {
            connect().await?.set_auto_accept(&device, !off).await?;
            if off {
                outln!("Files from {device} will wait for `crossdrop accept`.");
            } else {
                outln!("Files from {device} are now accepted automatically.");
            }
        }

        Command::Allow { device, service } => {
            let service = service_name(&service);
            connect().await?.set_permission(&device, &service, true).await?;
            outln!("{device} may now use {service}.");
        }

        Command::Deny { device, service } => {
            let service = service_name(&service);
            connect().await?.set_permission(&device, &service, false).await?;
            outln!("{device} may no longer use {service}.");
        }

        Command::Doctor => doctor(&paths).await?,

        Command::Completions { shell } => {
            use clap::CommandFactory;
            let mut script = Vec::new();
            clap_complete::generate(shell, &mut Cli::command(), "crossdrop", &mut script);
            out!("{}", String::from_utf8_lossy(&script));
        }

        Command::Unpair { device } => {
            connect().await?.unpair(&device).await?;
            outln!("Forgot {device}.");
        }
    }
    Ok(())
}

/// One JSON document on standard output.
fn print_json(value: serde_json::Value) -> Result<()> {
    outln!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

/// Sends `files` to one device, with progress on stderr.
async fn send_to(paths: &Paths, to: &str, files: &[PathBuf]) -> Result<()> {
    let mut stream = Client::connect(paths.clone()).await?.open_channel(to, "crossdrop/files").await?;
    let started = Instant::now();
    let mut last = Instant::now();
    // When the bytes started to flow, and how many were already there.
    let mut flowing = (Instant::now(), 0u64);
    let tty = std::io::stderr().is_terminal();
    let mut on_event = |event: SendEvent<'_>| match event {
        SendEvent::Waiting { files, bytes } => {
            eprintln!("Offering {files} file(s), {} to {to}…", human(bytes));
        }
        SendEvent::Accepted { resumed } => {
            flowing = (Instant::now(), resumed);
            if resumed > 0 {
                eprintln!("Accepted. Resuming: {} already there.", human(resumed));
            }
        }
        SendEvent::Progress { path, done, total } => {
            // A terminal gets one line redrawn in place; a log or a
            // pipe gets plain lines, a few seconds apart.
            let every = if tty { 100 } else { 5000 };
            if done == total || last.elapsed().as_millis() > every {
                last = Instant::now();
                let pct = (done * 100).checked_div(total).unwrap_or(100);
                let speed = speed(done - flowing.1, flowing.0.elapsed().as_secs_f64(), total - done);
                let line = format!("{pct:>3}%  {} / {}{speed}  {path}", human(done), human(total));
                if tty {
                    eprint!("\r\x1b[K{line}");
                    if done == total {
                        eprintln!();
                    }
                } else {
                    eprintln!("{line}");
                }
            }
        }
    };
    let report = match send_files(&mut stream, files, &mut on_event).await {
        Ok(r) => r,
        Err(e) if e.downcast_ref::<Declined>().is_some() => bail!("{to} declined the files"),
        Err(e) => return Err(e),
    };
    let secs = started.elapsed().as_secs_f64().max(0.001);
    outln!(
        "Sent {} file(s), {} in {secs:.1}s ({}/s).",
        report.received,
        human(report.bytes_sent),
        human((report.bytes_sent as f64 / secs) as u64)
    );
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
        _ => bail!("several requests are waiting; pass an id from `crossdrop requests`, or use --all"),
    }
}

/// `crossdrop doctor`: one line per check, then a non-zero exit if
/// something is broken.
async fn doctor(paths: &Paths) -> Result<()> {
    let mut problems = 0;
    let mut report = |ok: Option<bool>, what: String| {
        let tag = match ok {
            Some(true) => "ok  ",
            None => "warn",
            Some(false) => {
                problems += 1;
                "FAIL"
            }
        };
        outln!("{tag}  {what}");
    };

    let home = &paths.home;
    report(Some(home.is_dir()), format!("data directory {}", home.display()));
    #[cfg(unix)]
    {
        let max = if cfg!(any(target_os = "linux", target_os = "android")) { 107 } else { 103 };
        let len = paths.ipc_endpoint().as_os_str().len();
        report(Some(len <= max), format!("socket path length {len} bytes (at most {max})"));
    }
    let identity = home.join("identity.crt").is_file() && home.join("identity.key").is_file();
    report(Some(identity), "device identity (identity.crt, identity.key)".into());

    match Client::connect(paths.clone()).await {
        Err(e) => report(Some(false), format!("daemon not reachable: {e:#}")),
        Ok(mut client) => {
            let (id, name, port, version) = client.status().await?;
            report(Some(true), format!("daemon running: {name} ({}), UDP port {port}", short(&id)));
            let same = version == env!("CARGO_PKG_VERSION");
            let what = format!("daemon version {version}, this command {}", env!("CARGO_PKG_VERSION"));
            report(if same { Some(true) } else { None }, what);
            let devices = client.devices().await?;
            let paired: Vec<_> = devices.iter().filter(|d| d.paired).collect();
            if paired.is_empty() {
                report(None, "no paired device yet: `crossdrop pair <name>`".into());
            }
            for d in paired {
                let state = if d.online { "online" } else { "not reachable now" };
                let addrs = if d.addrs.is_empty() { "no known address".to_string() } else { d.addrs.join(", ") };
                report(if d.online { Some(true) } else { None }, format!("paired with {}: {state} ({addrs})", d.name));
            }
            let nearby = devices.iter().filter(|d| !d.paired).count();
            report(Some(true), format!("{nearby} unpaired device(s) seen on the network"));
        }
    }

    #[cfg(target_os = "macos")]
    report(
        if accessibility_allowed() { Some(true) } else { None },
        "Accessibility permission of this terminal (needed by `interdesk run`)".into(),
    );

    if problems > 0 {
        bail!("{problems} problem(s) found");
    }
    Ok(())
}

/// Whether this process (the terminal it runs in) may watch and drive the
/// mouse and keyboard, as interdesk must.
#[cfg(target_os = "macos")]
fn accessibility_allowed() -> bool {
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }
    // SAFETY: no arguments, no preconditions.
    unsafe { AXIsProcessTrusted() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_and_time_left() {
        assert_eq!(speed(10 << 20, 0.1, 1 << 30), "", "too early to tell");
        assert_eq!(speed(10 << 20, 2.0, 30 << 20), ", 5.0 MiB/s, 0:06 left");
        assert_eq!(speed(1 << 20, 1.0, 4000 << 20), ", 1.0 MiB/s, 1:06:40 left");
        assert_eq!(speed(1 << 20, 1.0, 0), ", 1.0 MiB/s");
    }

    #[test]
    fn sizes_use_binary_unit_names() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1536), "1.5 KiB");
        assert_eq!(human(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(human(3 << 30), "3.0 GiB");
    }
}

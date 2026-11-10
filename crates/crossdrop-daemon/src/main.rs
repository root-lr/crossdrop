//! `crossdropd` — the crossdrop daemon. `crossdrop daemon` does the same.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use crossdrop_daemon::Options;

/// The crossdrop daemon: keeps this device's identity and connections, and
/// serves the CLI, the desktop app and extensions. Settings not given here
/// come from config.toml in the data directory.
#[derive(Parser)]
#[command(name = "crossdropd", version)]
struct Args {
    /// Data directory (default: $CROSSDROP_HOME or the OS config directory).
    #[arg(long, value_name = "DIR")]
    home: Option<PathBuf>,
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber_init();
    crossdrop_daemon::run(Options {
        home: args.home,
        name: args.name,
        port: args.port,
        mdns: args.no_mdns.then_some(false),
        download_dir: args.download_dir,
        clipboard_sync: args.no_clipboard.then_some(false),
        desktop_dir: args.desktop_dir,
    })
    .await
}

fn tracing_subscriber_init() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

//! `crossdropd` — the crossdrop daemon. `crossdrop daemon` does the same.

use std::path::PathBuf;

use anyhow::{bail, Result};
use crossdrop_daemon::Options;

const USAGE: &str = "usage: crossdropd [--home DIR] [--name NAME] [--port PORT] [--no-mdns] [--download-dir DIR]";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber_init();
    let mut opts = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| args.next().ok_or_else(|| anyhow::anyhow!("{flag} needs a value"));
        match arg.as_str() {
            "--home" => opts.home = Some(PathBuf::from(value("--home")?)),
            "--name" => opts.name = Some(value("--name")?),
            "--port" => opts.port = Some(value("--port")?.parse()?),
            "--no-mdns" => opts.mdns = Some(false),
            "--download-dir" => opts.download_dir = Some(PathBuf::from(value("--download-dir")?)),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("crossdropd {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            other => bail!("unknown argument {other}\n{USAGE}"),
        }
    }
    crossdrop_daemon::run(opts).await
}

fn tracing_subscriber_init() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

//! `interdesk` command.

use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use crossdrop_proto::ipc::DeviceInfo;
use crossdrop_sdk::{Client, Paths};
use interdesk::layout::{Layout, Side};
use interdesk::{layout_path, platform, runtime};

#[derive(Parser)]
#[command(
    name = "interdesk",
    version,
    about = "One mouse and keyboard for all your computers (a crossdrop extension)."
)]
struct Cli {
    /// crossdrop data directory (default: $CROSSDROP_HOME or the OS config directory).
    #[arg(long, global = true, value_name = "DIR")]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Share this computer's mouse and keyboard and accept control from neighbours.
    Run,
    /// Place a paired device next to this screen: left, right, top or bottom.
    Add { device: String, side: Side },
    /// Remove a device from the layout.
    Remove { device: String },
    /// Show the layout.
    Layout,
}

#[tokio::main]
async fn main() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
    if let Err(e) = run(Cli::parse()).await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let paths = Paths::resolve(cli.home)?;
    let file = layout_path(&paths);
    match cli.command {
        Command::Run => {
            let layout = Layout::load(&file)?;
            // Fail early with a clear message if the daemon is not running.
            let (_, name, _, _) = Client::connect(paths.clone()).await?.status().await?;
            if layout.neighbors.is_empty() {
                println!("No neighbour configured: {name} can be controlled but controls nobody.");
                println!("Add one with `interdesk add <device> right`.");
            }
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let platform = platform::native(tx)?;
            println!("interdesk running on {name}. Ctrl+Alt+Esc brings the cursor back. Ctrl+C to stop.");
            runtime::run(paths, layout, platform, rx, crossdrop_sdk::shutdown_signal()?).await?;
        }
        Command::Add { device, side } => {
            let devices = Client::connect(paths.clone()).await?.devices().await?;
            let found = find_paired(&devices, &device)?;
            let mut layout = Layout::load(&file)?;
            layout.set(&found.name, side);
            layout.save(&file)?;
            println!("{} is now on the {side} of this screen.", found.name);
            println!("On {}, allow this device to control it: crossdrop allow <this device> input", found.name);
        }
        Command::Remove { device } => {
            let mut layout = Layout::load(&file)?;
            if !layout.remove(&device) {
                bail!("{device} is not in the layout");
            }
            layout.save(&file)?;
            println!("Removed {device}.");
        }
        Command::Layout => {
            let layout = Layout::load(&file)?;
            if layout.neighbors.is_empty() {
                println!("No neighbour. Add one with `interdesk add <device> right`.");
            }
            for n in layout.neighbors {
                println!("{:<7} {}", n.side, n.device);
            }
        }
    }
    Ok(())
}

/// The paired device named `query`, or whose id starts with it (at least
/// four characters, dashes optional), as `crossdrop` itself resolves them.
fn find_paired<'a>(devices: &'a [DeviceInfo], query: &str) -> Result<&'a DeviceInfo> {
    let prefix: String = query.chars().filter(|c| *c != '-').collect::<String>().to_uppercase();
    let matches: Vec<&DeviceInfo> = devices
        .iter()
        .filter(|d| d.paired)
        .filter(|d| {
            d.name.eq_ignore_ascii_case(query) || (prefix.len() >= 4 && d.id.replace('-', "").starts_with(&prefix))
        })
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => bail!("{query:?} is not a paired device; pair it first with `crossdrop pair {query}`"),
        _ => bail!("{query:?} matches several paired devices; use more of its id"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(name: &str, id: &str) -> DeviceInfo {
        DeviceInfo {
            id: id.into(),
            name: name.into(),
            addrs: vec![],
            paired: true,
            online: true,
            permissions: vec![],
            auto_accept: false,
            clipboard_send: false,
        }
    }

    #[test]
    fn devices_are_found_by_name_or_a_long_enough_id_prefix() {
        let devices = [device("bravo", "ABCD-EFGH-1111"), device("charlie", "ABCD-XYZW-2222")];
        assert_eq!(find_paired(&devices, "Bravo").unwrap().name, "bravo");
        assert_eq!(find_paired(&devices, "abcd-x").unwrap().name, "charlie");
        assert!(find_paired(&devices, "").is_err(), "an empty name must not pick the first device");
        assert!(find_paired(&devices, "ab").is_err(), "too short to be an id prefix");
        assert!(find_paired(&devices, "ABCD").unwrap_err().to_string().contains("several"));
    }
}

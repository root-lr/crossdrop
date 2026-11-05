//! Minimal crossdrop extension: echoes back whatever a paired device sends.
//!
//! Device B (serves the extension):
//!     cargo run -p crossdrop-sdk --example echo -- serve
//!     crossdrop allow <device A> example/echo
//!
//! Device A (uses it):
//!     cargo run -p crossdrop-sdk --example echo -- send <device B> "hello"

use anyhow::{bail, Result};
use crossdrop_sdk::Client;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SERVICE: &str = "example/echo";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["serve"] => serve().await,
        ["send", device, message] => send(device, message).await,
        _ => bail!("usage: echo serve | echo send <device> <message>"),
    }
}

async fn serve() -> Result<()> {
    let mut registration = Client::connect_default().await?.register(SERVICE).await?;
    println!("serving {SERVICE}; waiting for channels…");
    while let Some(incoming) = registration.next().await? {
        println!("channel from {}", incoming.device);
        let mut stream = incoming.accept().await?;
        tokio::spawn(async move {
            let mut buf = Vec::new();
            if stream.read_to_end(&mut buf).await.is_ok() {
                let _ = stream.write_all(&buf).await;
                let _ = stream.shutdown().await;
            }
        });
    }
    Ok(())
}

async fn send(device: &str, message: &str) -> Result<()> {
    let mut stream = Client::connect_default().await?.open_channel(device, SERVICE).await?;
    stream.write_all(message.as_bytes()).await?;
    stream.shutdown().await?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await?;
    println!("{device} echoed: {reply}");
    Ok(())
}

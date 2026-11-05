//! Sending side of the `crossdrop/files` service.
//!
//! Protocol, over one channel:
//! `File{name,size}` frame, `size` raw bytes, `FileEnd{blake3}` frame — per
//! file — then `End`. The receiver answers `Done{received}` or `Error`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use crossdrop_proto::wire::Files;
use crossdrop_proto::{read_frame, write_frame};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const CHUNK: usize = 64 * 1024;

/// Progress callback: `(file name, bytes sent for this file, file size)`.
pub type Progress<'a> = &'a mut (dyn FnMut(&str, u64, u64) + Send);

/// Sends regular files over an open `crossdrop/files` channel.
/// Returns how many files the receiver stored.
pub async fn send_files<S>(io: &mut S, files: &[PathBuf], progress: Progress<'_>) -> Result<u32>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    for path in files {
        send_one(io, path, progress).await?;
    }
    write_frame(io, &Files::End).await?;
    match read_frame::<_, Files>(io).await? {
        Files::Done { received } => Ok(received),
        Files::Error { message } => bail!("receiver error: {message}"),
        other => bail!("unexpected answer: {other:?}"),
    }
}

async fn send_one<S>(io: &mut S, path: &Path, progress: Progress<'_>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let meta = tokio::fs::metadata(path).await.with_context(|| format!("{}", path.display()))?;
    if !meta.is_file() {
        bail!("{} is not a regular file (folders arrive in v0.2)", path.display());
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .with_context(|| format!("{} has no file name", path.display()))?;
    let size = meta.len();
    write_frame(io, &Files::File { name: name.clone(), size }).await?;

    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK];
    let mut sent = 0u64;
    while sent < size {
        let want = CHUNK.min((size - sent) as usize);
        let n = file.read(&mut buf[..want]).await?;
        if n == 0 {
            bail!("{} shrank while being sent", path.display());
        }
        hasher.update(&buf[..n]);
        io.write_all(&buf[..n]).await?;
        sent += n as u64;
        progress(&name, sent, size);
    }
    write_frame(io, &Files::FileEnd { blake3: hasher.finalize().to_hex().to_string() }).await?;
    io.flush().await?;
    Ok(())
}

//! Receiving side of the `crossdrop/files` service.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use crossdrop_proto::wire::Files;
use crossdrop_proto::{read_frame, write_frame};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{info, warn};

const CHUNK: usize = 64 * 1024;

/// Receives files into `dir` until the sender says `End`.
pub async fn receive<S>(io: &mut S, dir: &Path, from: &str) -> Result<u32>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let result = receive_inner(io, dir, from).await;
    let answer = match &result {
        Ok(n) => Files::Done { received: *n },
        Err(e) => Files::Error { message: e.to_string() },
    };
    // Best effort: the sender may already be gone.
    let _ = write_frame(io, &answer).await;
    let _ = io.shutdown().await;
    result
}

async fn receive_inner<S>(io: &mut S, dir: &Path, from: &str) -> Result<u32>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::fs::create_dir_all(dir).await?;
    let mut received = 0u32;
    loop {
        match read_frame::<_, Files>(io).await? {
            Files::File { name, size } => {
                let path = receive_file(io, dir, &name, size).await?;
                info!(from, file = %path.display(), size, "file received");
                received += 1;
            }
            Files::End => return Ok(received),
            other => bail!("unexpected message: {other:?}"),
        }
    }
}

async fn receive_file<S>(io: &mut S, dir: &Path, name: &str, size: u64) -> Result<PathBuf>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let name = sanitize(name)?;
    let target = unique_path(dir, &name);
    let part = target.with_file_name(format!(".{name}.crossdrop-part"));
    let mut file = tokio::fs::File::create(&part).await?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK];
    let mut left = size;
    let copied: Result<()> = async {
        while left > 0 {
            let want = CHUNK.min(left as usize);
            let n = io.read(&mut buf[..want]).await?;
            if n == 0 {
                bail!("connection closed in the middle of {name}");
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n]).await?;
            left -= n as u64;
        }
        file.flush().await?;
        Ok(())
    }
    .await;
    drop(file);
    if let Err(e) = copied {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(e);
    }

    let expected = match read_frame::<_, Files>(io).await? {
        Files::FileEnd { blake3 } => blake3,
        other => {
            let _ = tokio::fs::remove_file(&part).await;
            bail!("unexpected message after file data: {other:?}");
        }
    };
    let actual = hasher.finalize().to_hex().to_string();
    if actual != expected {
        let _ = tokio::fs::remove_file(&part).await;
        warn!(file = %name, "integrity check failed");
        bail!("integrity check failed for {name}");
    }
    tokio::fs::rename(&part, &target).await?;
    Ok(target)
}

/// Keeps only a plain file name: no directories, no `..`, no control characters.
fn sanitize(name: &str) -> Result<String> {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = last.chars().filter(|c| !c.is_control()).collect();
    let clean = clean.trim().trim_start_matches('.').to_string();
    if clean.is_empty() {
        bail!("invalid file name {name:?}");
    }
    Ok(clean)
}

/// `report.pdf`, then `report (1).pdf`, `report (2).pdf`…
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    (1..).map(|i| dir.join(format!("{stem} ({i}){ext}"))).find(|p| !p.exists()).expect("an unused name exists")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_paths() {
        assert_eq!(sanitize("../../etc/passwd").unwrap(), "passwd");
        assert_eq!(sanitize("C:\\Users\\me\\a.txt").unwrap(), "a.txt");
        assert_eq!(sanitize(".bashrc").unwrap(), "bashrc");
        assert!(sanitize("..").is_err());
        assert!(sanitize("dir/").is_err());
    }

    #[test]
    fn unique_path_does_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        assert_eq!(unique_path(dir.path(), "a.txt"), dir.path().join("a (1).txt"));
        assert_eq!(unique_path(dir.path(), "b.txt"), dir.path().join("b.txt"));
    }
}

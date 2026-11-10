//! Sending side of the `crossdrop/files` service.
//!
//! `Offer` (every file with its relative path) → the receiver answers with
//! `Answer { accepted, offsets }`, `offsets[i]` being what it already holds
//! from an interrupted transfer. Then, per file, the bytes from that offset
//! to the end followed by `FileEnd { blake3 }` (hash of the whole file), and
//! finally `End` → `Done` / `Error`.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{bail, Context, Result};
use crossdrop_proto::wire::{FileEntry, Files};
use crossdrop_proto::{read_frame, write_frame};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const CHUNK: usize = 256 * 1024;

/// What happens during a transfer, for progress display.
#[derive(Debug)]
pub enum SendEvent<'a> {
    /// The offer is out; the receiver may have to accept it by hand.
    Waiting { files: usize, bytes: u64 },
    /// Accepted. `resumed` bytes were already on the other side.
    Accepted { resumed: u64 },
    /// `done` of `total` bytes of the whole transfer, currently in `path`.
    Progress { path: &'a str, done: u64, total: u64 },
}

/// Outcome of [`send_files`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendReport {
    pub received: u32,
    /// Bytes that went over the network.
    pub bytes_sent: u64,
    /// Bytes skipped because the receiver already had them.
    pub resumed: u64,
}

/// Error returned when the receiver declines the transfer.
#[derive(Debug)]
pub struct Declined;

impl std::fmt::Display for Declined {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the other device declined the transfer")
    }
}

impl std::error::Error for Declined {}

/// Lists the files to send: plain files as-is, folders recursively. A
/// symbolic link given here is followed (the user named it); links found
/// inside folders are skipped. Returns entries with their source paths.
pub fn collect(paths: &[PathBuf]) -> Result<Vec<(FileEntry, PathBuf)>> {
    let mut out = Vec::new();
    let mut roots = std::collections::HashSet::new();
    for path in paths {
        let meta = std::fs::metadata(path).with_context(|| format!("{}", path.display()))?;
        let root = root_name(path)?;
        if !roots.insert(root.clone()) {
            bail!("two items are named {root:?}; rename one of them");
        }
        if meta.is_file() {
            out.push((entry(root, &meta), path.clone()));
        } else if meta.is_dir() {
            let before = out.len();
            walk(path, &root, &mut out)?;
            out[before..].sort_by(|a, b| a.0.path.cmp(&b.0.path));
        } else {
            bail!("{} is not a file or a folder", path.display());
        }
    }
    Ok(out)
}

fn root_name(path: &Path) -> Result<String> {
    let name = match path.file_name() {
        Some(n) => n.to_owned(),
        None => std::fs::canonicalize(path)?.file_name().map(|n| n.to_owned()).unwrap_or_default(),
    };
    let name = utf8_name(&name, path)?;
    if name.is_empty() {
        bail!("{} has no name", path.display());
    }
    Ok(name)
}

fn entry(path: String, meta: &std::fs::Metadata) -> FileEntry {
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
    FileEntry { path, size: meta.len(), mtime, executable: is_executable(meta) }
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &std::fs::Metadata) -> bool {
    false
}

/// File names travel as UTF-8. A name in another encoding (old Linux
/// systems, some archives) would be mangled, and two such names could end up
/// identical on the other side: ask for a rename instead.
fn utf8_name(name: &std::ffi::OsStr, path: &Path) -> Result<String> {
    match name.to_str() {
        Some(name) => Ok(name.to_string()),
        None => bail!("{}: the name is not valid UTF-8; rename it to send it", path.display()),
    }
}

fn walk(dir: &Path, rel: &str, out: &mut Vec<(FileEntry, PathBuf)>) -> Result<()> {
    for item in std::fs::read_dir(dir).with_context(|| format!("{}", dir.display()))? {
        let item = item?;
        let meta = std::fs::symlink_metadata(item.path())?;
        let name = utf8_name(&item.file_name(), &item.path())?;
        let child = format!("{rel}/{name}");
        if meta.is_file() {
            out.push((entry(child, &meta), item.path()));
        } else if meta.is_dir() {
            walk(&item.path(), &child, out)?;
        }
    }
    Ok(())
}

/// Options for [`send_files_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SendOptions {
    /// Ask the receiver to put the files on its desktop.
    pub to_desktop: bool,
}

/// Sends files and folders over an open `crossdrop/files` channel.
pub async fn send_files<S>(
    io: &mut S,
    paths: &[PathBuf],
    on_event: &mut (dyn FnMut(SendEvent<'_>) + Send),
) -> Result<SendReport>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    send_files_with(io, paths, SendOptions::default(), on_event).await
}

/// [`send_files`] with options.
pub async fn send_files_with<S>(
    io: &mut S,
    paths: &[PathBuf],
    options: SendOptions,
    on_event: &mut (dyn FnMut(SendEvent<'_>) + Send),
) -> Result<SendReport>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let items = collect(paths)?;
    if items.is_empty() {
        bail!("nothing to send (empty folders only)");
    }
    let total: u64 = items.iter().map(|(e, _)| e.size).sum();
    let entries: Vec<FileEntry> = items.iter().map(|(e, _)| e.clone()).collect();
    write_frame(io, &Files::Offer { entries, desktop: options.to_desktop }).await?;
    on_event(SendEvent::Waiting { files: items.len(), bytes: total });

    let offsets = match read_frame::<_, Files>(io).await? {
        Files::Answer { accepted: false, .. } => return Err(Declined.into()),
        Files::Answer { accepted: true, offsets } if offsets.len() == items.len() => offsets,
        Files::Error { message } => bail!("receiver error: {message}"),
        other => bail!("unexpected answer: {other:?}"),
    };
    let resumed: u64 = items.iter().zip(&offsets).map(|((e, _), o)| (*o).min(e.size)).sum();
    on_event(SendEvent::Accepted { resumed });

    let mut done = resumed;
    let mut buf = vec![0u8; CHUNK];
    for ((entry, source), offset) in items.iter().zip(offsets) {
        let offset = offset.min(entry.size);
        let mut file = tokio::fs::File::open(source).await.with_context(|| format!("{}", source.display()))?;
        let mut hasher = blake3::Hasher::new();
        let mut pos = 0u64;
        while pos < entry.size {
            let want = CHUNK.min((entry.size - pos) as usize);
            let n = file.read(&mut buf[..want]).await?;
            if n == 0 {
                bail!("{} changed while being sent", source.display());
            }
            hasher.update(&buf[..n]);
            let end = pos + n as u64;
            if end > offset {
                let from = offset.saturating_sub(pos) as usize;
                io.write_all(&buf[from..n]).await?;
                done += (n - from) as u64;
                on_event(SendEvent::Progress { path: &entry.path, done, total });
            }
            pos = end;
        }
        write_frame(io, &Files::FileEnd { blake3: hasher.finalize().to_hex().to_string() }).await?;
    }
    write_frame(io, &Files::End).await?;
    io.flush().await?;
    match read_frame::<_, Files>(io).await? {
        Files::Done { received } => Ok(SendReport { received, bytes_sent: total - resumed, resumed }),
        Files::Error { message } => bail!("receiver error: {message}"),
        other => bail!("unexpected answer: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_walks_folders_and_rejects_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("photos");
        std::fs::create_dir_all(photos.join("2026")).unwrap();
        std::fs::write(photos.join("b.jpg"), "b").unwrap();
        std::fs::write(photos.join("2026").join("a.jpg"), "aa").unwrap();
        std::fs::create_dir_all(photos.join("empty")).unwrap();
        std::fs::write(dir.path().join("note.txt"), "n").unwrap();

        let items = collect(&[photos.clone(), dir.path().join("note.txt")]).unwrap();
        let paths: Vec<&str> = items.iter().map(|(e, _)| e.path.as_str()).collect();
        assert_eq!(paths, ["photos/2026/a.jpg", "photos/b.jpg", "note.txt"]);
        assert_eq!(items[0].0.size, 2);

        assert!(collect(&[photos.clone(), photos]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn named_links_are_followed_but_not_links_inside_folders() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        std::fs::write(real.join("sub").join("a.txt"), "a").unwrap();
        std::fs::write(dir.path().join("note.txt"), "note").unwrap();
        symlink(dir.path().join("note.txt"), real.join("inner-link")).unwrap();
        symlink(dir.path().join("note.txt"), dir.path().join("link.txt")).unwrap();
        symlink(&real, dir.path().join("folder-link")).unwrap();

        let items = collect(&[dir.path().join("link.txt"), dir.path().join("folder-link")]).unwrap();
        let paths: Vec<&str> = items.iter().map(|(e, _)| e.path.as_str()).collect();
        assert_eq!(paths, ["link.txt", "folder-link/sub/a.txt"]);
        assert_eq!(items[0].0.size, 4);
    }

    #[cfg(unix)]
    #[test]
    fn names_that_are_not_utf8_are_refused() {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::OsStr::from_bytes(b"caf\xe9.txt"); // Latin-1
        let err = utf8_name(name, Path::new("/x/caf?.txt")).unwrap_err();
        assert!(err.to_string().contains("not valid UTF-8"), "{err}");
        assert_eq!(utf8_name(std::ffi::OsStr::new("café.txt"), Path::new("x")).unwrap(), "café.txt");
    }
}

//! Receiving side of the `crossdrop/files` service.
//!
//! Data first lands in `<download dir>/.crossdrop-partial/`, one `.part` file
//! per incoming file, keyed by sender, path, size and mtime. A transfer that
//! breaks off resumes where it stopped the next time the same files are
//! offered. A small journal per transfer remembers which root folder name was
//! chosen and which files are already complete.

use std::collections::BTreeMap;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use crossdrop_proto::wire::{FileEntry, Files};
use crossdrop_proto::{read_frame, write_frame};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};
use tracing::{info, warn};

const CHUNK: usize = 256 * 1024;
const PARTIAL_DIR: &str = ".crossdrop-partial";
const MAX_ENTRIES: usize = 100_000;
/// Partial files and journals untouched for this long are deleted.
const PARTIAL_TTL: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// A validated offer.
pub struct Offer {
    pub entries: Vec<FileEntry>,
    /// The sender would like the files on the desktop.
    pub desktop: bool,
    /// Sanitized path components of each entry.
    parts: Vec<Vec<String>>,
}

impl Offer {
    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }
}

/// Reads and validates the sender's offer.
pub async fn read_offer<S: AsyncRead + Unpin>(io: &mut S) -> Result<Offer> {
    let (entries, desktop) = match read_frame::<_, Files>(io).await? {
        Files::Offer { entries, desktop } => (entries, desktop),
        other => bail!("expected an offer, got {other:?}"),
    };
    if entries.is_empty() || entries.len() > MAX_ENTRIES {
        bail!("offer with {} files refused", entries.len());
    }
    let parts = entries.iter().map(|e| sanitize_path(&e.path)).collect::<Result<Vec<_>>>()?;
    Ok(Offer { entries, desktop, parts })
}

pub async fn decline<S: AsyncWrite + Unpin>(io: &mut S) -> Result<()> {
    write_frame(io, &Files::Answer { accepted: false, offsets: vec![] }).await?;
    let _ = io.shutdown().await;
    Ok(())
}

#[derive(Serialize, Deserialize, Default)]
struct Journal {
    /// Root name as offered → name used in the download folder.
    roots: BTreeMap<String, String>,
    /// Completed files (offered path → BLAKE3).
    done: BTreeMap<String, String>,
}

struct Transfer {
    dir: PathBuf,
    journal_path: PathBuf,
    journal: Journal,
}

impl Transfer {
    fn save(&self) -> Result<()> {
        crossdrop_sdk::write_atomic(&self.journal_path, &serde_json::to_vec(&self.journal)?)?;
        Ok(())
    }

    fn target(&self, parts: &[String]) -> PathBuf {
        let mut path = self.dir.join(&self.journal.roots[&parts[0]]);
        for p in &parts[1..] {
            path.push(p);
        }
        path
    }
}

fn key(peer: &str, items: &[&FileEntry]) -> String {
    let mut h = blake3::Hasher::new();
    h.update(peer.as_bytes());
    for e in items {
        h.update(format!("\n{}\n{}\n{}", e.path, e.size, e.mtime).as_bytes());
    }
    h.finalize().to_hex()[..32].to_string()
}

/// Path of the partial file for one entry.
pub(crate) fn part_path(dir: &Path, peer: &str, entry: &FileEntry) -> PathBuf {
    dir.join(PARTIAL_DIR).join(format!("{}.part", key(peer, &[entry])))
}

/// Receives an accepted offer into `dir`. Returns how many files were stored.
pub async fn receive<S>(io: &mut S, offer: Offer, dir: &Path, peer: &str, from: &str) -> Result<u32>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let result = receive_inner(io, offer, dir, peer, from).await;
    let answer = match &result {
        Ok(n) => Files::Done { received: *n },
        Err(e) => Files::Error { message: format!("{e:#}") },
    };
    // Best effort: the sender may already be gone.
    let _ = write_frame(io, &answer).await;
    let _ = io.shutdown().await;
    result
}

async fn receive_inner<S>(io: &mut S, offer: Offer, dir: &Path, peer: &str, from: &str) -> Result<u32>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let partial = dir.join(PARTIAL_DIR);
    tokio::fs::create_dir_all(&partial).await.with_context(|| format!("{}", partial.display()))?;
    remove_stale(&partial);
    let all: Vec<&FileEntry> = offer.entries.iter().collect();
    let journal_path = partial.join(format!("{}.json", key(peer, &all)));
    let journal =
        std::fs::read(&journal_path).ok().and_then(|b| serde_json::from_slice::<Journal>(&b).ok()).unwrap_or_default();
    let mut t = Transfer { dir: dir.to_path_buf(), journal_path, journal };

    for parts in &offer.parts {
        if !t.journal.roots.contains_key(&parts[0]) {
            let chosen = unique_name(dir, &parts[0], t.journal.roots.values());
            t.journal.roots.insert(parts[0].clone(), chosen);
        }
    }

    let mut offsets = Vec::with_capacity(offer.entries.len());
    for (entry, parts) in offer.entries.iter().zip(&offer.parts) {
        let complete = t.journal.done.contains_key(&entry.path)
            && std::fs::metadata(t.target(parts)).map(|m| m.len() == entry.size).unwrap_or(false);
        if complete {
            offsets.push(entry.size);
            continue;
        }
        t.journal.done.remove(&entry.path);
        let part = part_path(dir, peer, entry);
        let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        offsets.push(if have <= entry.size { have } else { 0 });
    }
    t.save()?;
    write_frame(io, &Files::Answer { accepted: true, offsets: offsets.clone() }).await?;

    let mut received = 0u32;
    let mut buf = vec![0u8; CHUNK];
    for ((entry, parts), offset) in offer.entries.iter().zip(&offer.parts).zip(offsets) {
        let target = t.target(parts);
        if let Some(known) = t.journal.done.get(&entry.path).cloned() {
            let sent = expect_file_end(io).await?;
            if sent != known {
                bail!("{} changed on the sending side; send it again", entry.path);
            }
            received += 1;
            continue;
        }

        let part = part_path(dir, peer, entry);
        let mut file =
            tokio::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&part).await?;
        file.set_len(offset).await?;
        let mut hasher = blake3::Hasher::new();
        let mut left = offset;
        while left > 0 {
            let n = file.read(&mut buf[..CHUNK.min(left as usize)]).await?;
            if n == 0 {
                bail!("partial file shrank: {}", part.display());
            }
            hasher.update(&buf[..n]);
            left -= n as u64;
        }
        file.seek(SeekFrom::Start(offset)).await?;
        let mut left = entry.size - offset;
        while left > 0 {
            let n = io.read(&mut buf[..CHUNK.min(left as usize)]).await?;
            if n == 0 {
                file.flush().await?;
                bail!("connection lost in {} ({} bytes kept for resume)", entry.path, entry.size - left);
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n]).await?;
            left -= n as u64;
        }
        file.flush().await?;
        drop(file);

        let expected = expect_file_end(io).await?;
        let actual = hasher.finalize().to_hex().to_string();
        if actual != expected {
            let _ = tokio::fs::remove_file(&part).await;
            warn!(file = %entry.path, "integrity check failed");
            bail!("integrity check failed for {}", entry.path);
        }
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let target = if target.exists() { unique_path(&target) } else { target };
        tokio::fs::rename(&part, &target).await?;
        t.journal.done.insert(entry.path.clone(), actual);
        t.save()?;
        info!(from, file = %target.display(), size = entry.size, resumed = offset, "file received");
        received += 1;
    }

    match read_frame::<_, Files>(io).await? {
        Files::End => {}
        other => bail!("unexpected message: {other:?}"),
    }
    let _ = std::fs::remove_file(&t.journal_path);
    Ok(received)
}

/// Deletes abandoned partial files and journals.
fn remove_stale(partial: &Path) {
    let Ok(items) = std::fs::read_dir(partial) else { return };
    for item in items.flatten() {
        let old = item
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > PARTIAL_TTL);
        if old {
            let _ = std::fs::remove_file(item.path());
        }
    }
}

async fn expect_file_end<S: AsyncRead + Unpin>(io: &mut S) -> Result<String> {
    match read_frame::<_, Files>(io).await? {
        Files::FileEnd { blake3 } => Ok(blake3),
        other => bail!("unexpected message after file data: {other:?}"),
    }
}

/// Splits an offered path into safe components: no `..`, no absolute paths,
/// no characters Windows forbids, no control characters.
fn sanitize_path(path: &str) -> Result<Vec<String>> {
    let parts: Vec<String> = path
        .split('/')
        .map(|c| {
            c.chars()
                .filter(|ch| !ch.is_control())
                .map(|ch| if "\\:*?\"<>|".contains(ch) { '_' } else { ch })
                .collect::<String>()
                .trim()
                .trim_end_matches('.')
                .to_string()
        })
        .collect();
    if parts.iter().any(|p| p.is_empty() || p == "..") {
        bail!("invalid path {path:?}");
    }
    if parts[0] == PARTIAL_DIR {
        bail!("reserved name {path:?}");
    }
    Ok(parts)
}

/// A name for `root` that exists neither in `dir` nor among `taken`.
fn unique_name<'a>(dir: &Path, root: &str, taken: impl Iterator<Item = &'a String> + Clone) -> String {
    let free = |n: &str| !dir.join(n).exists() && !taken.clone().any(|t| t == n);
    if free(root) {
        return root.to_string();
    }
    let (stem, ext) = match root.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && !e.contains(' ') => (s.to_string(), format!(".{e}")),
        _ => (root.to_string(), String::new()),
    };
    (1..).map(|i| format!("{stem} ({i}){ext}")).find(|n| free(n)).expect("an unused name exists")
}

fn unique_path(path: &Path) -> PathBuf {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    dir.join(unique_name(dir, &name, std::iter::empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossdrop_sdk::files::{send_files, SendEvent};

    #[test]
    fn sanitize_rejects_escapes() {
        assert_eq!(sanitize_path("photos/2026/a.jpg").unwrap(), ["photos", "2026", "a.jpg"]);
        assert_eq!(sanitize_path("a:b?.txt").unwrap(), ["a_b_.txt"]);
        assert!(sanitize_path("../etc/passwd").is_err());
        assert!(sanitize_path("/etc/passwd").is_err());
        assert!(sanitize_path("a//b").is_err());
        assert!(sanitize_path(".crossdrop-partial/x").is_err());
        assert_eq!(sanitize_path("dir/.gitignore").unwrap(), ["dir", ".gitignore"]);
    }

    #[test]
    fn unique_names() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        std::fs::create_dir(dir.path().join("photos")).unwrap();
        let none: Vec<String> = vec![];
        assert_eq!(unique_name(dir.path(), "a.txt", none.iter()), "a (1).txt");
        assert_eq!(unique_name(dir.path(), "photos", none.iter()), "photos (1)");
        assert_eq!(unique_name(dir.path(), "new", none.iter()), "new");
    }

    /// Runs one transfer through an in-memory pipe.
    async fn transfer(src: &[PathBuf], dl: &Path) -> (crossdrop_sdk::files::SendReport, u32) {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let dl = dl.to_path_buf();
        let receiver = tokio::spawn(async move {
            let offer = read_offer(&mut b).await.unwrap();
            receive(&mut b, offer, &dl, "PEER", "peer").await.unwrap()
        });
        let report = send_files(&mut a, src, &mut |_: SendEvent<'_>| {}).await.unwrap();
        (report, receiver.await.unwrap())
    }

    #[tokio::test]
    async fn folder_transfer_keeps_tree_and_avoids_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("src").join("photos");
        std::fs::create_dir_all(photos.join("2026")).unwrap();
        std::fs::write(photos.join("2026").join("a.jpg"), "aaa").unwrap();
        std::fs::write(photos.join("b.jpg"), "bb").unwrap();
        let dl = dir.path().join("dl");

        let (report, n) = transfer(std::slice::from_ref(&photos), &dl).await;
        assert_eq!((report.received, n), (2, 2));
        assert_eq!(std::fs::read_to_string(dl.join("photos/2026/a.jpg")).unwrap(), "aaa");

        // Same folder again: lands next to the first copy.
        transfer(&[photos], &dl).await;
        assert_eq!(std::fs::read_to_string(dl.join("photos (1)/b.jpg")).unwrap(), "bb");
        // No leftovers.
        assert_eq!(std::fs::read_dir(dl.join(PARTIAL_DIR)).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn interrupted_file_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("big.bin");
        let data: Vec<u8> = (0..1_000_000u32).map(|i| (i % 253) as u8).collect();
        std::fs::write(&src, &data).unwrap();
        let dl = dir.path().join("dl");

        // Simulate an earlier transfer that stopped after 400 000 bytes.
        let entry = crossdrop_sdk::files::collect(std::slice::from_ref(&src)).unwrap().remove(0).0;
        let part = part_path(&dl, "PEER", &entry);
        std::fs::create_dir_all(part.parent().unwrap()).unwrap();
        std::fs::write(&part, &data[..400_000]).unwrap();

        let (report, _) = transfer(&[src], &dl).await;
        assert_eq!(report.resumed, 400_000);
        assert_eq!(report.bytes_sent, 600_000);
        assert_eq!(std::fs::read(dl.join("big.bin")).unwrap(), data);
    }

    #[tokio::test]
    async fn corrupted_partial_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("f.bin");
        std::fs::write(&src, vec![7u8; 10_000]).unwrap();
        let dl = dir.path().join("dl");
        let entry = crossdrop_sdk::files::collect(std::slice::from_ref(&src)).unwrap().remove(0).0;
        let part = part_path(&dl, "PEER", &entry);
        std::fs::create_dir_all(part.parent().unwrap()).unwrap();
        std::fs::write(&part, vec![0u8; 5_000]).unwrap();

        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let dl2 = dl.clone();
        let receiver = tokio::spawn(async move {
            let offer = read_offer(&mut b).await.unwrap();
            receive(&mut b, offer, &dl2, "PEER", "peer").await
        });
        let sent = send_files(&mut a, &[src], &mut |_: SendEvent<'_>| {}).await;
        assert!(sent.is_err());
        assert!(receiver.await.unwrap().is_err());
        assert!(!part.exists(), "a corrupted partial file must be discarded");
        assert!(!dl.join("f.bin").exists());
    }
}

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
use crossdrop_proto::ipc::ReceivingInfo;
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
    /// Sanitized path components of each empty folder to create.
    dirs: Vec<Vec<String>>,
}

impl Offer {
    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }
}

/// Reads and validates the sender's offer.
pub async fn read_offer<S: AsyncRead + Unpin>(io: &mut S) -> Result<Offer> {
    let (entries, desktop, dirs) = match read_frame::<_, Files>(io).await? {
        Files::Offer { entries, desktop, dirs } => (entries, desktop, dirs),
        other => bail!("expected an offer, got {other:?}"),
    };
    let items = entries.len() + dirs.len();
    if items == 0 || items > MAX_ENTRIES {
        bail!("offer with {} files and {} folders refused", entries.len(), dirs.len());
    }
    let mut seen = std::collections::HashSet::new();
    if let Some(dup) = entries.iter().find(|e| !seen.insert(e.path.as_str())) {
        bail!("offer lists {:?} twice", dup.path);
    }
    let dirs = dirs.iter().map(|d| sanitize_path(d)).collect::<Result<Vec<_>>>()?;
    let parts = entries.iter().map(|e| sanitize_path(&e.path)).collect::<Result<Vec<_>>>()?;
    Ok(Offer { entries, desktop, parts, dirs })
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

/// Where an incoming transfer stands, for `crossdrop transfers` and the
/// app, and the switch that cancels it.
pub(crate) struct Progress {
    info: std::sync::Mutex<ReceivingInfo>,
    cancel: tokio::sync::watch::Sender<bool>,
}

impl Default for Progress {
    fn default() -> Self {
        Self::new(ReceivingInfo::default())
    }
}

impl Progress {
    pub fn new(info: ReceivingInfo) -> Self {
        Self { info: std::sync::Mutex::new(info), cancel: tokio::sync::watch::channel(false).0 }
    }

    pub fn get(&self) -> ReceivingInfo {
        self.info.lock().unwrap().clone()
    }

    fn update(&self, f: impl FnOnce(&mut ReceivingInfo)) {
        f(&mut self.info.lock().unwrap());
    }

    /// Stops the transfer at its next read, even if the sender is stalled.
    pub fn cancel(&self) {
        self.cancel.send_replace(true);
    }

    fn cancelled(&self) -> bool {
        *self.cancel.borrow()
    }

    /// Resolves once the transfer is cancelled.
    async fn until_cancelled(&self) {
        let mut rx = self.cancel.subscribe();
        let _ = rx.wait_for(|c| *c).await;
    }
}

/// Error returned when the user stops a transfer on this side.
const CANCELLED: &str = "cancelled on the receiving side";

/// Receives an accepted offer into `dir`. Returns how many files were stored.
pub async fn receive<S>(
    io: &mut S,
    offer: Offer,
    dir: &Path,
    peer: &str,
    from: &str,
    progress: &Progress,
) -> Result<u32>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let entries = offer.entries.clone();
    let result = tokio::select! {
        r = receive_inner(io, offer, dir, peer, from, progress) => r,
        () = progress.until_cancelled() => Err(anyhow::anyhow!(CANCELLED)),
    };
    if progress.cancelled() {
        // Nothing to resume: remove the partial files and the journal.
        let partial = dir.join(PARTIAL_DIR);
        for e in &entries {
            let _ = std::fs::remove_file(part_path(dir, peer, e));
        }
        let all: Vec<&FileEntry> = entries.iter().collect();
        let _ = std::fs::remove_file(partial.join(format!("{}.json", key(peer, &all))));
        info!(from, "transfer cancelled");
    }
    let answer = match &result {
        Ok(n) => Files::Done { received: *n },
        Err(e) => Files::Error { message: format!("{e:#}") },
    };
    // Best effort: the sender may already be gone.
    let _ = write_frame(io, &answer).await;
    let _ = io.shutdown().await;
    result
}

async fn receive_inner<S>(
    io: &mut S,
    offer: Offer,
    dir: &Path,
    peer: &str,
    from: &str,
    progress: &Progress,
) -> Result<u32>
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

    for parts in offer.parts.iter().chain(&offer.dirs) {
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
    // Refuse now rather than fill the disk and fail at the end.
    let needed: u64 = offer.entries.iter().zip(&offsets).map(|(e, o)| e.size - o).sum();
    if let Some(free) = free_space(&partial) {
        check_space(needed, free)?;
    }
    t.save()?;
    write_frame(io, &Files::Answer { accepted: true, offsets: offsets.clone() }).await?;
    progress.update(|p| p.received_bytes = offsets.iter().sum());

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

        progress.update(|p| p.current = entry.path.clone());
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
            progress.update(|p| p.received_bytes += n as u64);
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
        restore_metadata(&target, entry);
        t.journal.done.insert(entry.path.clone(), actual);
        t.save()?;
        info!(from, file = %target.display(), size = entry.size, resumed = offset, "file received");
        received += 1;
    }

    for parts in &offer.dirs {
        let folder = t.target(parts);
        tokio::fs::create_dir_all(&folder).await.with_context(|| format!("{}", folder.display()))?;
    }

    match read_frame::<_, Files>(io).await? {
        Files::End => {}
        other => bail!("unexpected message: {other:?}"),
    }
    let _ = std::fs::remove_file(&t.journal_path);
    Ok(received)
}

/// Gives a received file the sender's modification time and, on Unix, its
/// executable bit. Best effort: a file system that refuses only loses the
/// metadata, not the file.
fn restore_metadata(target: &Path, entry: &FileEntry) {
    #[cfg(unix)]
    if entry.executable {
        use std::os::unix::fs::PermissionsExt;
        // Executable by whoever may read it, as `chmod +x` does.
        let set = std::fs::metadata(target).and_then(|m| {
            let mode = m.permissions().mode();
            std::fs::set_permissions(target, std::fs::Permissions::from_mode(mode | (mode & 0o444) >> 2))
        });
        if let Err(e) = set {
            warn!(file = %target.display(), "cannot make it executable: {e}");
        }
    }
    if entry.mtime == 0 {
        return;
    }
    let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(entry.mtime);
    let set = std::fs::File::options().write(true).open(target).and_then(|f| f.set_modified(mtime));
    if let Err(e) = set {
        warn!(file = %target.display(), "cannot set the modification time: {e}");
    }
}

/// Deletes abandoned partial files under `dir` (a download or desktop
/// folder), so that a transfer never resumed does not keep its data forever.
pub fn remove_stale_partials(dir: &Path) {
    remove_stale(&dir.join(PARTIAL_DIR));
}

/// Room left on the disk once a transfer is stored, for everything else.
const SPARE_BYTES: u64 = 64 * 1024 * 1024;

fn check_space(needed: u64, free: u64) -> Result<()> {
    if needed.saturating_add(SPARE_BYTES) > free {
        let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
        bail!("not enough disk space: {:.1} MiB to receive, {:.1} MiB free", mib(needed), mib(free));
    }
    Ok(())
}

/// Bytes available to this user on the file system holding `dir`, if the
/// system tells.
#[cfg(unix)]
fn free_space(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a valid C string and `stat` a writable statvfs.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)] // the field types differ between systems
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(windows)]
fn free_space(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free = 0u64;
    // SAFETY: `wide` is NUL-terminated; the other outputs may be null.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
    (ok != 0).then_some(free)
}

#[cfg(not(any(unix, windows)))]
fn free_space(_: &Path) -> Option<u64> {
    None
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
/// no characters Windows forbids, no control characters, no names Windows
/// reserves for devices.
fn sanitize_path(path: &str) -> Result<Vec<String>> {
    let parts: Vec<String> = path.split('/').map(|c| sanitize_component(c, cfg!(windows))).collect();
    if parts.iter().any(|p| p.is_empty() || p == "..") {
        bail!("invalid path {path:?}");
    }
    if parts[0] == PARTIAL_DIR {
        bail!("reserved name {path:?}");
    }
    Ok(parts)
}

/// One path component made safe to create. `windows`: also avoid the device
/// names Windows reserves (`CON`, `nul.txt`, `COM1.log`…), which cannot be
/// created as files there, by prefixing them with `_`.
fn sanitize_component(c: &str, windows: bool) -> String {
    let name = c
        .chars()
        .filter(|ch| !ch.is_control())
        .map(|ch| if "\\:*?\"<>|".contains(ch) { '_' } else { ch })
        .collect::<String>()
        .trim()
        .trim_end_matches('.')
        .to_string();
    let name = if windows && is_windows_device_name(&name) { format!("_{name}") } else { name };
    shorten(name)
}

/// Longest component kept, in bytes. File systems allow 255; the margin
/// leaves room for the " (1)" a name gets when it is already taken.
const MAX_COMPONENT_BYTES: usize = 240;

/// Cuts a name too long for the file system, keeping its extension.
fn shorten(name: String) -> String {
    if name.len() <= MAX_COMPONENT_BYTES {
        return name;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 16 => (stem, format!(".{ext}")),
        _ => (name.as_str(), String::new()),
    };
    let mut cut = MAX_COMPONENT_BYTES - ext.len();
    while !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{ext}", stem[..cut].trim_end())
}

/// `CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9` (and the
/// superscript-digit forms), whatever the case and the extension.
fn is_windows_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    match stem.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" => true,
        _ => {
            let (prefix, digit) = stem.split_at(stem.len().min(3));
            matches!(prefix, "COM" | "LPT")
                && matches!(digit, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
        }
    }
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
    fn windows_device_names_are_renamed_there_only() {
        for name in ["CON", "con.txt", "Nul.tar.gz", "COM1.log", "lpt9", "AUX ", "aux.c.orig", "com²"] {
            assert_eq!(sanitize_component(name, true), format!("_{}", name.trim()), "{name}");
        }
        for name in ["CONSOLE", "com10", "COM0", "nullable.txt", "lpt", "my.con"] {
            assert_eq!(sanitize_component(name, true), name, "{name}");
        }
        // Elsewhere these are ordinary names (Linux has drivers/…/aux.c).
        assert_eq!(sanitize_component("aux.c", false), "aux.c");
    }

    #[test]
    fn long_names_are_cut_keeping_the_extension() {
        let long = format!("{}.jpeg", "é".repeat(200));
        let short = sanitize_component(&long, false);
        assert!(short.len() <= MAX_COMPONENT_BYTES, "{} bytes", short.len());
        assert!(short.ends_with("é.jpeg"), "{short}");
        let no_ext = sanitize_component(&"x".repeat(300), false);
        assert_eq!(no_ext.len(), MAX_COMPONENT_BYTES);
        assert_eq!(sanitize_component("photo.jpeg", false), "photo.jpeg");
    }

    #[tokio::test]
    async fn an_offer_with_the_same_path_twice_is_refused() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let entry = FileEntry { path: "photos/a.jpg".into(), size: 1, mtime: 0, executable: false };
        let offer = Files::Offer { entries: vec![entry.clone(), entry], desktop: false, dirs: vec![] };
        write_frame(&mut a, &offer).await.unwrap();
        let err = read_offer(&mut b).await.err().unwrap();
        assert!(err.to_string().contains("twice"), "{err}");
    }

    #[test]
    fn transfers_that_would_fill_the_disk_are_refused() {
        let gib = 1024 * 1024 * 1024;
        assert!(check_space(gib, 2 * gib).is_ok());
        let err = check_space(2 * gib, gib).unwrap_err();
        assert!(err.to_string().contains("not enough disk space"), "{err}");
        // Leaves room for the rest of the system.
        assert!(check_space(gib, gib).is_err());
        let dir = tempfile::tempdir().unwrap();
        assert!(free_space(dir.path()).is_some_and(|free| free > 0));
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
            receive(&mut b, offer, &dl, "PEER", "peer", &Progress::default()).await.unwrap()
        });
        let report = send_files(&mut a, src, &mut |_: SendEvent<'_>| {}).await.unwrap();
        (report, receiver.await.unwrap())
    }

    #[tokio::test]
    async fn progress_counts_every_byte_stored() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.bin");
        std::fs::write(&src, vec![7u8; 300_000]).unwrap();
        let dl = dir.path().join("dl");
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let progress = std::sync::Arc::new(Progress::default());
        let p = progress.clone();
        let receiver = tokio::spawn(async move {
            let offer = read_offer(&mut b).await.unwrap();
            receive(&mut b, offer, &dl, "PEER", "peer", &p).await.unwrap()
        });
        send_files(&mut a, &[src], &mut |_: SendEvent<'_>| {}).await.unwrap();
        receiver.await.unwrap();
        let info = progress.get();
        assert_eq!((info.received_bytes, info.current.as_str()), (300_000, "a.bin"));
    }

    #[tokio::test]
    async fn a_cancelled_transfer_stops_both_sides_and_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("big.bin");
        std::fs::write(&src, vec![1u8; 4 * 1024 * 1024]).unwrap();
        let dl = dir.path().join("dl");
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let progress = std::sync::Arc::new(Progress::default());
        let (p, dl2) = (progress.clone(), dl.clone());
        let receiver = tokio::spawn(async move {
            let offer = read_offer(&mut b).await.unwrap();
            receive(&mut b, offer, &dl2, "PEER", "peer", &p).await
        });
        // Cancel as soon as some bytes are stored.
        let p = progress.clone();
        tokio::spawn(async move {
            while p.get().received_bytes == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            p.cancel();
        });
        let err = send_files(&mut a, &[src], &mut |_: SendEvent<'_>| {}).await.unwrap_err();
        assert!(err.to_string().contains("cancelled on the receiving side"), "{err}");
        assert!(receiver.await.unwrap().is_err());
        assert!(!dl.join("big.bin").exists());
        let left = std::fs::read_dir(dl.join(PARTIAL_DIR)).unwrap().count();
        assert_eq!(left, 0, "partial file and journal removed");
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
            receive(&mut b, offer, &dl2, "PEER", "peer", &Progress::default()).await
        });
        let sent = send_files(&mut a, &[src], &mut |_: SendEvent<'_>| {}).await;
        assert!(sent.is_err());
        assert!(receiver.await.unwrap().is_err());
        assert!(!part.exists(), "a corrupted partial file must be discarded");
        assert!(!dl.join("f.bin").exists());
    }

    #[tokio::test]
    async fn received_files_keep_their_modification_time() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("old.txt");
        std::fs::write(&src, "from 2020").unwrap();
        let then = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        std::fs::File::options().write(true).open(&src).unwrap().set_modified(then).unwrap();
        let dl = dir.path().join("dl");

        transfer(&[src], &dl).await;
        let got = std::fs::metadata(dl.join("old.txt")).unwrap().modified().unwrap();
        assert_eq!(got, then);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn executables_stay_executable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let tools = dir.path().join("tools");
        std::fs::create_dir(&tools).unwrap();
        std::fs::write(tools.join("run.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(tools.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(tools.join("notes.txt"), "plain").unwrap();
        std::fs::set_permissions(tools.join("notes.txt"), std::fs::Permissions::from_mode(0o644)).unwrap();
        let dl = dir.path().join("dl");

        transfer(&[tools], &dl).await;
        let mode = |name: &str| std::fs::metadata(dl.join("tools").join(name)).unwrap().permissions().mode();
        assert_ne!(mode("run.sh") & 0o100, 0, "run.sh lost its executable bit");
        assert_eq!(mode("notes.txt") & 0o111, 0, "notes.txt became executable");
    }

    #[tokio::test]
    async fn empty_folders_are_created() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::create_dir_all(project.join("build").join("cache")).unwrap();
        std::fs::write(project.join("src").join("main.rs"), "fn main() {}").unwrap();
        let blank = dir.path().join("blank");
        std::fs::create_dir(&blank).unwrap();
        let dl = dir.path().join("dl");

        let (report, _) = transfer(&[project, blank], &dl).await;
        assert_eq!(report.received, 1);
        assert!(dl.join("project/src/main.rs").is_file());
        assert!(dl.join("project/build/cache").is_dir());
        assert!(dl.join("blank").is_dir());
    }
}

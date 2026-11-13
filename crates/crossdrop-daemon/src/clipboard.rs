//! Access to the system clipboard: text and images.
//!
//! The OS clipboard APIs are blocking and, on X11, the process that last set
//! the clipboard must keep serving it. So one dedicated thread owns the
//! clipboard handle for the daemon's whole life.
//!
//! Images travel as PNG. Reading one means converting it, which is too
//! costly to repeat at every poll: on macOS and Windows the clipboard is
//! read again only when its change counter moves; on Linux, which has none,
//! an image is looked for at most every [`LINUX_IMAGE_EVERY`] and only when
//! there is no text.

use std::borrow::Cow;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::sync::oneshot;
use tracing::debug;

/// What the clipboard holds, as far as crossdrop shares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Text(String),
    Image(Image),
}

/// A clipboard image, encoded as PNG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub png: Vec<u8>,
}

impl Image {
    /// Encodes RGBA pixels (8 bits per channel, row after row).
    pub fn from_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<Self> {
        if rgba.len() != width as usize * height as usize * 4 {
            bail!("{width}×{height} image with {} bytes of pixels", rgba.len());
        }
        let mut png = Vec::new();
        let mut encoder = png::Encoder::new(&mut png, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        // Clipboard images are mostly screenshots: fast beats small.
        encoder.set_compression(png::Compression::Fast);
        encoder.write_header()?.write_image_data(rgba)?;
        Ok(Self { width, height, png })
    }

    /// Checks and decodes a PNG received from another device into RGBA.
    pub fn to_rgba(&self) -> Result<Vec<u8>> {
        decode_png(&self.png).and_then(|(w, h, rgba)| {
            if (w, h) != (self.width, self.height) {
                bail!("the image is {w}×{h}, not {}×{}", self.width, self.height);
            }
            Ok(rgba)
        })
    }
}

/// PNG → (width, height, RGBA), whatever its color type.
pub fn decode_png(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    // Palette, gray and 16-bit images become 8-bit RGB(A).
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().context("not a PNG image")?;
    let mut buf = vec![0; reader.output_buffer_size().context("image too large")?];
    let frame = reader.next_frame(&mut buf).context("damaged PNG image")?;
    buf.truncate(frame.buffer_size());
    let pixels = (frame.width * frame.height) as usize;
    let rgba = match frame.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => {
            buf.as_chunks::<2>().0.iter().flat_map(|p| [p[0], p[0], p[0], p[1]]).collect()
        }
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => bail!("unexpected indexed PNG after expansion"),
    };
    if rgba.len() != pixels * 4 {
        bail!("damaged PNG image");
    }
    Ok((frame.width, frame.height, rgba))
}

/// On Linux, how often to look for an image when the clipboard has no text.
pub const LINUX_IMAGE_EVERY: Duration = Duration::from_secs(2);

enum Cmd {
    Get(oneshot::Sender<Option<Content>>),
    Set(Content),
}

/// Handle to the clipboard thread.
#[derive(Clone)]
pub struct SystemClipboard {
    tx: mpsc::Sender<Cmd>,
}

impl SystemClipboard {
    /// Starts the clipboard thread. `None` when the system has no clipboard
    /// (no display on Linux, for instance).
    pub fn start() -> Option<Self> {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let (ready_tx, ready_rx) = mpsc::channel::<bool>();
        thread::Builder::new()
            .name("crossdrop-clipboard".into())
            .spawn(move || {
                let mut clipboard = match arboard::Clipboard::new() {
                    Ok(c) => {
                        let _ = ready_tx.send(true);
                        c
                    }
                    Err(e) => {
                        debug!("system clipboard unavailable: {e}");
                        let _ = ready_tx.send(false);
                        return;
                    }
                };
                let mut reader = Reader::default();
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        Cmd::Get(reply) => {
                            let _ = reply.send(reader.read(&mut clipboard));
                        }
                        Cmd::Set(content) => {
                            if let Err(e) = write(&mut clipboard, &content) {
                                debug!("cannot set clipboard: {e:#}");
                            }
                            // Whatever was cached is stale now.
                            reader = Reader::default();
                        }
                    }
                }
            })
            .ok()?;
        ready_rx.recv().ok().filter(|ok| *ok).map(|_| Self { tx })
    }

    pub async fn get(&self) -> Option<Content> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Cmd::Get(reply)).ok()?;
        rx.await.ok().flatten()
    }

    pub fn set(&self, content: Content) {
        let _ = self.tx.send(Cmd::Set(content));
    }
}

/// Reads the clipboard, reusing the last result while the system says it
/// has not changed.
#[derive(Default)]
struct Reader {
    /// Change counter seen with `last`, where the system has one.
    seen: Option<u64>,
    last: Option<Content>,
    /// Linux: when an image was last looked for.
    image_checked: Option<Instant>,
}

impl Reader {
    fn read(&mut self, clipboard: &mut arboard::Clipboard) -> Option<Content> {
        let counter = change_counter();
        if counter.is_some() && counter == self.seen {
            return self.last.clone();
        }
        if is_sensitive() {
            debug!("clipboard marked sensitive by its owner; not shared");
            self.remember(counter, None);
            return None;
        }
        if let Some(text) = clipboard.get_text().ok().filter(|t| !t.is_empty()) {
            self.remember(counter, Some(Content::Text(text)));
            return self.last.clone();
        }
        if counter.is_none() {
            // No counter: an image is looked for now and then only, and the
            // last one is kept meanwhile.
            if self.image_checked.is_some_and(|t| t.elapsed() < LINUX_IMAGE_EVERY) {
                return self.last.clone().filter(|c| matches!(c, Content::Image(_)));
            }
            self.image_checked = Some(Instant::now());
        }
        let image = clipboard.get_image().ok().and_then(|img| {
            Image::from_rgba(img.width as u32, img.height as u32, &img.bytes)
                .map_err(|e| debug!("clipboard image not read: {e:#}"))
                .ok()
        });
        self.remember(counter, image.map(Content::Image));
        self.last.clone()
    }

    fn remember(&mut self, counter: Option<u64>, content: Option<Content>) {
        self.seen = counter;
        self.last = content;
    }
}

fn write(clipboard: &mut arboard::Clipboard, content: &Content) -> Result<()> {
    match content {
        Content::Text(text) => clipboard.set_text(text.clone())?,
        Content::Image(image) => {
            let rgba = image.to_rgba()?;
            clipboard.set_image(arboard::ImageData {
                width: image.width as usize,
                height: image.height as usize,
                bytes: Cow::Owned(rgba),
            })?
        }
    }
    Ok(())
}

/// A number that changes whenever the clipboard does, where the system
/// keeps one.
#[cfg(target_os = "macos")]
fn change_counter() -> Option<u64> {
    Some(objc2_app_kit::NSPasteboard::generalPasteboard().changeCount() as u64)
}

#[cfg(windows)]
fn change_counter() -> Option<u64> {
    clipboard_win::raw::seq_num().map(|n| u64::from(n.get()))
}

#[cfg(not(any(target_os = "macos", windows)))]
fn change_counter() -> Option<u64> {
    None
}

/// Pasteboard types (macOS) that password managers and similar apps add to
/// mean "do not record or share this": 1Password, Bitwarden, KeePassXC…
/// See <http://nspasteboard.org>.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const MAC_SENSITIVE_TYPES: &[&str] =
    &["org.nspasteboard.ConcealedType", "org.nspasteboard.TransientType", "com.agilebits.onepassword"];

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn has_sensitive_type<S: AsRef<str>>(types: impl IntoIterator<Item = S>) -> bool {
    types.into_iter().any(|t| MAC_SENSITIVE_TYPES.contains(&t.as_ref()))
}

/// Whether the current clipboard content was marked as not to be shared.
#[cfg(target_os = "macos")]
fn is_sensitive() -> bool {
    let Some(types) = objc2_app_kit::NSPasteboard::generalPasteboard().types() else { return false };
    has_sensitive_type(types.iter().map(|t| t.to_string()))
}

/// Windows: the formats clipboard managers and password managers set to
/// keep a copy out of history and sync tools.
#[cfg(windows)]
fn is_sensitive() -> bool {
    ["ExcludeClipboardContentFromMonitorProcessing", "Clipboard Viewer Ignore"].iter().any(|name| {
        clipboard_win::register_format(name).is_some_and(|format| clipboard_win::is_format_avail(format.get()))
    })
}

/// Not detected yet on X11 and Wayland.
#[cfg(not(any(target_os = "macos", windows)))]
fn is_sensitive() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_survive_the_trip_through_png() {
        let (w, h) = (3u32, 2u32);
        let rgba: Vec<u8> = (0..w * h * 4).map(|i| (i * 37 % 256) as u8).collect();
        let image = Image::from_rgba(w, h, &rgba).unwrap();
        assert_eq!(&image.png[1..4], b"PNG");
        assert_eq!(image.to_rgba().unwrap(), rgba);
        assert!(Image::from_rgba(w, h, &rgba[1..]).is_err(), "wrong pixel count");
        let lying = Image { width: 9, ..image.clone() };
        assert!(lying.to_rgba().is_err(), "size must match the PNG");
        assert!(decode_png(b"not a png").is_err());
    }

    #[test]
    fn concealed_and_transient_types_are_sensitive() {
        assert!(has_sensitive_type(["public.utf8-plain-text", "org.nspasteboard.ConcealedType"]));
        assert!(has_sensitive_type(["org.nspasteboard.TransientType"]));
        assert!(!has_sensitive_type(["public.utf8-plain-text", "public.html"]));
    }

    /// Uses the real pasteboard, so it is opt-in:
    /// `cargo test -p crossdrop-daemon -- --ignored real_pasteboard`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn real_pasteboard() {
        use arboard::SetExtApple;
        let mut cb = arboard::Clipboard::new().unwrap();
        let saved = cb.get_text().ok();
        cb.set().exclude_from_history().text("hunter2").unwrap();
        let concealed = is_sensitive();
        cb.set_text("plain").unwrap();
        let plain = is_sensitive();
        if let Some(text) = saved {
            cb.set_text(text).unwrap();
        }
        assert!(concealed && !plain, "concealed: {concealed}, plain: {plain}");
    }
}

//! Shared protocol types for crossdrop.
//!
//! Two protocols live here:
//! - the **wire** protocol spoken between devices over QUIC ([`wire`]);
//! - the **IPC** protocol spoken between the local daemon and its clients
//!   (CLI, desktop app, extensions) ([`ipc`]).
//!
//! Both use the same framing: a big-endian `u32` length followed by a JSON
//! payload. After a channel has been opened (or an IPC connection attached),
//! the stream carries whatever bytes the service wants.

use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// ALPN identifier and protocol version negotiated in the QUIC handshake.
pub const ALPN: &[u8] = b"crossdrop/1";

/// Default UDP port of the daemon.
pub const DEFAULT_PORT: u16 = 47470;

/// mDNS service type announced by every daemon.
pub const MDNS_SERVICE_TYPE: &str = "_crossdrop._udp.local.";

/// Upper bound for a single control frame. Bulk data never goes through frames.
pub const MAX_FRAME_LEN: u32 = 8 * 1024 * 1024;

/// Upper bound for frames that any device on the network may send: channel
/// headers and replies, pairing messages. A few KiB, so that unpaired peers
/// cannot make the daemon allocate [`MAX_FRAME_LEN`] per stream.
pub const MAX_SMALL_FRAME_LEN: u32 = 4 * 1024;

/// Well-known service names.
pub mod services {
    /// Pairing handshake. The only service an unpaired device may open.
    pub const PAIR: &str = "crossdrop/pair";
    /// Sent by a paired device that forgets this one, so that this one
    /// forgets it too. Open to every paired device, whatever its permissions.
    pub const UNPAIR: &str = "crossdrop/unpair";
    /// Built-in file and folder transfer.
    pub const FILES: &str = "crossdrop/files";
    /// Built-in clipboard sharing.
    pub const CLIPBOARD: &str = "crossdrop/clipboard";
    /// Images copied to the clipboard: a [`wire::ClipboardImage`] header,
    /// then the PNG bytes. Allowed together with [`CLIPBOARD`]. A separate
    /// service so that devices without image support simply refuse it.
    pub const CLIPBOARD_IMAGE: &str = "crossdrop/clipboard-image";
    /// interdesk's input service. A device allowed to drive this one may
    /// also drop files on its desktop without asking.
    pub const INTERDESK_INPUT: &str = "interdesk/input";
}

/// Longest device name kept from another device, in characters.
pub const MAX_NAME_CHARS: usize = 64;

/// Makes a name received from another device safe to show: no control or
/// bidirectional-override characters (which can disguise a name), single
/// spaces, at most [`MAX_NAME_CHARS`] characters. `None` if nothing is left.
pub fn clean_device_name(raw: &str) -> Option<String> {
    let bidi =
        |c: char| matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}');
    let kept: String = raw.chars().map(|c| if c.is_control() { ' ' } else { c }).filter(|c| !bidi(*c)).collect();
    let name = kept.split_whitespace().collect::<Vec<_>>().join(" ");
    let name: String = name.chars().take(MAX_NAME_CHARS).collect();
    let name = name.trim_end();
    (!name.is_empty()).then(|| name.to_string())
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame too large: {0} bytes")]
    TooLarge(u32),
    #[error("invalid frame: {0}")]
    Json(#[from] serde_json::Error),
    #[error("stream closed")]
    Closed,
}

/// Writes one length-prefixed JSON frame.
pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    // Length and body in one buffer, so that a frame is one write: one
    // syscall on the IPC socket, one QUIC stream frame rather than two.
    let mut frame = vec![0u8; 4];
    serde_json::to_writer(&mut frame, msg)?;
    let len = u32::try_from(frame.len() - 4).map_err(|_| FrameError::TooLarge(u32::MAX))?;
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(len));
    }
    frame[..4].copy_from_slice(&len.to_be_bytes());
    w.write_all(&frame).await?;
    w.flush().await?;
    Ok(())
}

/// Reads one length-prefixed JSON frame. Returns [`FrameError::Closed`] on a
/// clean end of stream before the first byte.
pub async fn read_frame<R, T>(r: &mut R) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    read_frame_max(r, MAX_FRAME_LEN).await
}

/// [`read_frame`] with a smaller size limit, for frames read before the peer
/// is known to be trusted (see [`MAX_SMALL_FRAME_LEN`]).
pub async fn read_frame_max<R, T>(r: &mut R, max: u32) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    // `read_exact` cannot tell "nothing" from "half a header": read the
    // first byte on its own so that only a stream ending between frames
    // counts as a clean close.
    let mut len = [0u8; 4];
    if r.read(&mut len[..1]).await? == 0 {
        return Err(FrameError::Closed);
    }
    r.read_exact(&mut len[1..]).await?;
    let len = u32::from_be_bytes(len);
    if len > max.min(MAX_FRAME_LEN) {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// Messages exchanged between two devices.
pub mod wire {
    use serde::{Deserialize, Serialize};

    /// First frame on every new QUIC stream.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct OpenChannel {
        pub service: String,
        pub version: u32,
    }

    /// Answer to [`OpenChannel`].
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum ChannelReply {
        Accepted,
        Rejected { reason: String },
    }

    /// Pairing service messages.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Pair {
        /// Sent by the initiator once its user confirmed the code.
        Request { name: String },
        /// Sent by the responder once its user decided.
        Response { accepted: bool, name: String },
    }

    /// One file of a transfer. `path` is relative and `/`-separated
    /// (`photos/2026/a.jpg`); its first component is the transfer root.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct FileEntry {
        pub path: String,
        pub size: u64,
        /// Modification time, seconds since the Unix epoch. Part of the
        /// resume key, so a changed file restarts from zero; given back to
        /// the received file.
        pub mtime: u64,
        /// Unix: the file is executable (a script, a binary). Omitted when
        /// false, so older peers see the same offers as before.
        #[serde(default, skip_serializing_if = "is_false")]
        pub executable: bool,
    }

    fn is_false(b: &bool) -> bool {
        !*b
    }

    /// File service messages.
    ///
    /// `Offer` → `Answer`, then for each entry: the bytes from its offset to
    /// its end, raw, followed by `FileEnd`; finally `End` → `Done` / `Error`.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Files {
        Offer {
            entries: Vec<FileEntry>,
            /// Sender asks for the files to land on the desktop (drag to a
            /// neighbouring screen). Honoured only for devices allowed
            /// `interdesk/input`.
            #[serde(default)]
            desktop: bool,
            /// Empty folders to create, same path rules as entries. Omitted
            /// when there are none, so older peers see the same offers.
            #[serde(default, skip_serializing_if = "Vec::is_empty")]
            dirs: Vec<String>,
        },
        /// `offsets[i]` = bytes of entry `i` the receiver already holds.
        Answer {
            accepted: bool,
            offsets: Vec<u64>,
        },
        /// BLAKE3 (hex) of the whole file that was just sent.
        FileEnd {
            blake3: String,
        },
        End,
        Done {
            received: u32,
        },
        Error {
            message: String,
        },
    }

    /// Clipboard service: one message per channel.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct Clipboard {
        pub text: String,
    }

    /// Header of a clipboard image; `size` bytes of PNG follow, then the
    /// receiver answers with a `ChannelReply`.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct ClipboardImage {
        pub width: u32,
        pub height: u32,
        pub size: u64,
    }
}

/// Messages between the local daemon and its clients.
pub mod ipc {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Request {
        Status,
        ListDevices,
        /// Start pairing with a device: id prefix, name, or `ip:port`.
        PairStart {
            target: String,
        },
        /// The local user compared the code and decided.
        PairConfirm {
            pair_id: String,
            accept: bool,
        },
        /// Incoming pairing requests waiting for a decision.
        ListPairRequests,
        /// Answer an incoming pairing request.
        PairAnswer {
            pair_id: String,
            accept: bool,
        },
        Unpair {
            device: String,
        },
        SetPermission {
            device: String,
            service: String,
            allow: bool,
        },
        /// Become the handler of a service. Turns the connection into an
        /// event stream of [`Event::IncomingChannel`].
        Register {
            service: String,
        },
        /// Open a channel to a paired device.
        OpenChannel {
            device: String,
            service: String,
        },
        /// First and only request of a data connection: bind it to a channel.
        /// After `Response::Ok`, the connection carries raw bytes.
        Attach {
            channel_id: String,
        },
        /// Incoming file transfers waiting for a decision.
        ListOffers,
        /// Incoming file transfers being received now.
        ListReceiving,
        /// Stops receiving a transfer and deletes what it stored so far.
        CancelReceiving {
            transfer_id: String,
        },
        OfferAnswer {
            offer_id: String,
            accept: bool,
        },
        /// Accept files from this device without asking.
        SetAutoAccept {
            device: String,
            enabled: bool,
        },
        /// Send (or stop sending) this device's clipboard copies to a device.
        /// By default they go to the devices allowed `crossdrop/clipboard`.
        SetClipboardSend {
            device: String,
            enabled: bool,
        },
        /// Send text to one device, or to every device this device shares its clipboard with.
        ClipboardSend {
            text: String,
            device: Option<String>,
        },
        /// Last clipboard text received from another device.
        ClipboardGet,
        /// Put a PNG file in the clipboard of one device, or of every device
        /// this device shares its clipboard with.
        ClipboardSendImage {
            path: String,
            device: Option<String>,
        },
        /// Save the last clipboard image received from another device as PNG.
        ClipboardGetImage {
            path: String,
        },
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Response {
        Ok,
        Error {
            message: String,
        },
        Status {
            device_id: String,
            name: String,
            port: u16,
            version: String,
        },
        Devices {
            devices: Vec<DeviceInfo>,
        },
        PairChallenge {
            pair_id: String,
            sas: String,
            peer_id: String,
            peer_name: String,
        },
        PairResult {
            paired: bool,
            peer_id: String,
            peer_name: String,
        },
        PairRequests {
            requests: Vec<PairRequestInfo>,
        },
        ChannelReady {
            channel_id: String,
        },
        Offers {
            offers: Vec<OfferInfo>,
        },
        Receiving {
            transfers: Vec<ReceivingInfo>,
        },
        Sent {
            count: u32,
        },
        Clipboard {
            text: Option<String>,
            from: Option<String>,
        },
        /// `width` is 0 when no image was received.
        ClipboardImage {
            width: u32,
            height: u32,
            from: Option<String>,
        },
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Event {
        IncomingChannel { channel_id: String, device: String, service: String },
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct DeviceInfo {
        pub id: String,
        pub name: String,
        pub addrs: Vec<String>,
        pub paired: bool,
        pub online: bool,
        pub permissions: Vec<String>,
        #[serde(default)]
        pub auto_accept: bool,
        /// This device's clipboard copies are sent to it.
        #[serde(default)]
        pub clipboard_send: bool,
    }

    /// An accepted transfer on its way to this device.
    #[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
    pub struct ReceivingInfo {
        /// For `CancelReceiving`.
        #[serde(default)]
        pub transfer_id: String,
        pub peer_id: String,
        pub peer_name: String,
        pub files: u32,
        pub total_bytes: u64,
        /// Bytes stored so far, including those kept from an earlier attempt.
        pub received_bytes: u64,
        /// Path of the file being received.
        pub current: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct OfferInfo {
        pub offer_id: String,
        pub peer_id: String,
        pub peer_name: String,
        pub files: u32,
        pub total_bytes: u64,
        /// First few paths, for display.
        pub preview: Vec<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct PairRequestInfo {
        pub pair_id: String,
        pub peer_id: String,
        pub peer_name: String,
        pub sas: String,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_names_are_cleaned() {
        assert_eq!(clean_device_name("  bravo  ").as_deref(), Some("bravo"));
        assert_eq!(clean_device_name("Mac\nBook\t Pro").as_deref(), Some("Mac Book Pro"));
        // "alpha" shown with a right-to-left override reads "ahpla": dropped.
        assert_eq!(clean_device_name("\u{202E}ahpla").as_deref(), Some("ahpla"));
        assert_eq!(clean_device_name("\u{7}\u{200F} "), None);
        assert_eq!(clean_device_name(&"é".repeat(100)).unwrap().chars().count(), MAX_NAME_CHARS);
        assert_eq!(clean_device_name("Bureau de Léa").as_deref(), Some("Bureau de Léa"));
    }

    #[test]
    fn file_entries_stay_compatible_with_older_peers() {
        let plain = wire::FileEntry { path: "a.txt".into(), size: 1, mtime: 2, executable: false };
        assert_eq!(serde_json::to_string(&plain).unwrap(), r#"{"path":"a.txt","size":1,"mtime":2}"#);
        let old: wire::FileEntry = serde_json::from_str(r#"{"path":"a.txt","size":1,"mtime":2}"#).unwrap();
        assert_eq!(old, plain);
        let exe = wire::FileEntry { executable: true, ..plain };
        assert!(serde_json::to_string(&exe).unwrap().contains(r#""executable":true"#));
    }

    #[tokio::test]
    async fn frame_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let msg = wire::OpenChannel { service: services::FILES.into(), version: 1 };
        write_frame(&mut a, &msg).await.unwrap();
        let got: wire::OpenChannel = read_frame(&mut b).await.unwrap();
        assert_eq!(got, msg);
    }

    /// Counts the writes a frame takes.
    struct Writes(Vec<usize>);

    impl tokio::io::AsyncWrite for Writes {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.0.push(buf.len());
            std::task::Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn a_frame_is_written_at_once() {
        let mut w = Writes(vec![]);
        write_frame(&mut w, &wire::ChannelReply::Accepted).await.unwrap();
        assert_eq!(w.0, [4 + r#"{"type":"accepted"}"#.len()]);
    }

    #[tokio::test]
    async fn closed_stream_is_reported() {
        let (a, mut b) = tokio::io::duplex(16);
        drop(a);
        let err = read_frame::<_, wire::ChannelReply>(&mut b).await.unwrap_err();
        assert!(matches!(err, FrameError::Closed));
    }

    #[tokio::test]
    async fn small_frame_limit_applies() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let big = wire::Pair::Request { name: "x".repeat(MAX_SMALL_FRAME_LEN as usize) };
        write_frame(&mut a, &big).await.unwrap();
        let err = read_frame_max::<_, wire::Pair>(&mut b, MAX_SMALL_FRAME_LEN).await.unwrap_err();
        assert!(matches!(err, FrameError::TooLarge(_)));
    }

    #[tokio::test]
    async fn a_stream_cut_inside_a_header_is_an_error_not_a_close() {
        let (mut a, mut b) = tokio::io::duplex(16);
        tokio::io::AsyncWriteExt::write_all(&mut a, &[0, 0]).await.unwrap();
        drop(a);
        let err = read_frame::<_, wire::ChannelReply>(&mut b).await.unwrap_err();
        assert!(matches!(err, FrameError::Io(_)), "{err:?}");
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected() {
        let (mut a, mut b) = tokio::io::duplex(16);
        tokio::io::AsyncWriteExt::write_all(&mut a, &(MAX_FRAME_LEN + 1).to_be_bytes()).await.unwrap();
        let err = read_frame::<_, wire::ChannelReply>(&mut b).await.unwrap_err();
        assert!(matches!(err, FrameError::TooLarge(_)));
    }
}

# crossdrop protocol, version 1

Status: draft; describes what this repository implements. Breaking changes
bump the ALPN.

## Transport

- QUIC over UDP, IPv4 and IPv6 (one dual-stack socket), default port **47470**, ALPN **`crossdrop/1`**.
- TLS 1.3 only, **mutual** authentication. Each device presents a self-signed
  certificate over an Ed25519 key (`identity.crt` / `identity.key` in its data
  directory).
- Verifiers accept any certificate but check every handshake signature, so a
  peer always proves it holds the key of the certificate it shows.
- One connection per pair of devices, shared by every service. Keep-alive every
  10 s, idle timeout 60 s.

## Device id

`DeviceId = BLAKE3(certificate DER)[0..20]`, displayed in base32 (RFC 4648, no
padding) as 8 groups of 4 characters: `VCZU-QFPU-HPBA-BVF6-MS4O-4O7R-TZU5-C2TA`.
Input is case-insensitive and dashes are optional. A short form (first two
groups) is used in listings.

## Discovery

mDNS / DNS-SD service type `_crossdrop._udp.local.`. Instance name = compact
device id. TXT records:

| key    | value                     |
| ------ | ------------------------- |
| `id`   | device id (dashed form)   |
| `name` | user-visible device name  |
| `v`    | protocol version, `1`     |

Devices can also be reached by `ip:port` when multicast is blocked.

## Framing

Control messages are frames: a big-endian `u32` length, then a UTF-8 JSON body
(max 8 MiB; 4 KiB for channel headers and replies and for pairing messages,
which any device on the network may send). Tagged enums use a `"type"` field
in `snake_case`. Bulk data is never framed.

## Channels

A channel is one bidirectional QUIC stream. The opener sends:

```json
{"service": "crossdrop/files", "version": 1}
```

The other side answers `{"type":"accepted"}` or
`{"type":"rejected","reason":"…"}`. After `accepted`, the stream belongs to
the service.

Acceptance rules on the receiving device:

1. `version` must be `1`, else `channel version … is not supported`.
2. `crossdrop/pair` is open to anyone (it is how trust starts).
3. Any other service requires the peer to be in `trusted.toml`, else `not paired`.
4. The service must be in that device's `permissions`, else `permission denied`.
5. The service must be built in or registered by a running extension, else
   `service … is not available`.

## Pairing (`crossdrop/pair`)

1. A connects to B (TLS gives each the other's device id).
2. Both compute the **SAS**: TLS exporter, label `crossdrop pairing sas v1`,
   context = the two device ids sorted and concatenated, 32 bytes out. The
   first 4 bytes as a big-endian integer, modulo 10⁶, shown as `ddd ddd`.
   A man in the middle holds two TLS sessions, so the codes would differ.
3. A's user confirms the code. A opens `crossdrop/pair` and sends
   `{"type":"request","name":"alpha"}`.
4. B shows the request and the code. B's user accepts or rejects within 120 s.
5. B answers `{"type":"response","accepted":true,"name":"bravo"}`. Each side
   stores the other in `trusted.toml` with the default permission
   `crossdrop/files`.

## Unpairing (`crossdrop/unpair`)

When the user forgets a device, the daemon first opens `crossdrop/unpair` to
it (best effort, 3 s). The receiver, if it still trusts the sender, removes it
from `trusted.toml`, closes the connection and answers `{"type":"accepted"}`.
Any paired device may open this service, whatever its permissions.

## Files (`crossdrop/files`)

```
→ {"type":"offer","desktop":false,"entries":[{"path":"photos/2026/a.jpg","size":123456,"mtime":1793900000}, …]}
← {"type":"answer","accepted":true,"offsets":[0, …]}
   for each entry, in order:
→ <size - offset raw bytes>
→ {"type":"file_end","blake3":"<hex of the WHOLE file>"}
→ {"type":"end"}
← {"type":"done","received":N}  |  {"type":"error","message":"…"}
```

- `path` is relative, `/`-separated; its first component is the transfer
  root (a file name or a folder name). Folders are sent recursively;
  symbolic links met inside a folder are skipped, a link named on the
  command line is followed, and empty folders go in `dirs` (below).
- `mtime` is given back to the received file. `"executable": true` (omitted
  when false) marks a Unix executable; the receiver sets its `x` bits.
- `"dirs": ["photos/empty", …]` (omitted when empty) lists the empty
  folders to create, with the same path rules; an offer may hold only
  folders.
- `"desktop": true` in the offer asks for the files to land on the
  receiver's desktop. It is honoured, without asking, only when the sender
  is allowed `interdesk/input` on the receiver; otherwise the offer is
  handled like any other.
- Before answering, the receiver checks that the bytes still to come fit on
  its disk with 64 MiB to spare; if not, it replies
  `{"type":"error","message":"not enough disk space: …"}` instead.
- The receiver may wait for its user (`crossdrop accept`, up to 5 minutes)
  unless the sender's device is set to auto-accept. A refusal is
  `{"type":"answer","accepted":false,"offsets":[]}`.
- **Resume.** Data is written to `<downloads>/.crossdrop-partial/<key>.part`,
  `key = BLAKE3(sender id, path, size, mtime)`. When the same file is offered
  again, `offsets[i]` is the length already held; the sender skips those
  bytes but still hashes them, so the final check covers the whole file. A
  journal per transfer keeps the chosen root name and the files already
  completed. Leftovers untouched for 7 days are deleted.
- **Cancel.** The receiver may stop at any point (`crossdrop cancel`): it
  sends `{"type":"error","message":"cancelled on the receiving side"}`,
  closes the stream and deletes that transfer's partial files and journal.
  A sender whose write fails reads that last frame to report the reason.
- **Safety.** Path components are cleaned (control characters removed,
  `\ : * ? " < > |` replaced by `_`; on Windows, device names such as `CON`
  or `nul.txt` get a `_` prefix); `..`, empty components and absolute paths
  are refused. A root that already exists in the download folder gets a
  free name: `photos (1)`, `report (1).pdf`.

## Clipboard (`crossdrop/clipboard`)

One channel per update: the sender writes `{"text":"…"}`, the receiver
applies it and answers `{"type":"accepted"}`. Texts over 1 MiB are not shared
(send them as a file).

Sharing is opt-in per device and symmetric by default: `crossdrop allow X
clipboard` lets X write to this device's clipboard **and** makes this device
send its own copies to X. `crossdrop clip share X --off` keeps this device's
copies from X (receive only); `crossdrop clip share X` sends them even if X
may not write here (send only). The choice is stored as `clipboard_send` in
`trusted.toml` only once made. Each daemon polls the system clipboard every
500 ms; text received from a peer is remembered so it is not sent back.
Headless devices without a system clipboard still accept and expose the last
text through `crossdrop clip get`.

### Images (`crossdrop/clipboard-image`)

Allowed by the same `clipboard` permission. The sender writes
`{"width":1920,"height":1080,"size":N}`, then exactly `N` bytes of PNG; the
receiver checks that it decodes to that size, applies it and answers
`{"type":"accepted"}` (or `rejected` with a reason). Images over 32 MiB as
PNG are not shared. A separate service keeps older devices safe: they refuse
it, and the sender simply skips them. Reading an image from the clipboard
means converting it, so on macOS and Windows it is read again only when the
system's change counter moves; on Linux an image is looked for at most every
2 s, when there is no text. Text wins when the clipboard holds both.

Copies that their app marks as sensitive are never sent: on macOS the
`org.nspasteboard.ConcealedType` / `TransientType` pasteboard types (set by
1Password, Bitwarden, KeePassXC…), on Windows the
`ExcludeClipboardContentFromMonitorProcessing` and `Clipboard Viewer Ignore`
formats. Not detected yet on Linux.

## Reserved service names

| name                  | owner                    |
| --------------------- | ------------------------ |
| `crossdrop/*`         | built-in services        |
| `interdesk/input`     | interdesk (v0.3)         |

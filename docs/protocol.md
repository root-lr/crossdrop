# crossdrop protocol, version 1

Status: draft, implemented by crossdrop v0.2. Breaking changes bump the ALPN.

## Transport

- QUIC over UDP, default port **47470**, ALPN **`crossdrop/1`**.
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

1. `crossdrop/pair` is open to anyone (it is how trust starts).
2. Any other service requires the peer to be in `trusted.toml`, else `not paired`.
3. The service must be in that device's `permissions`, else `permission denied`.
4. The service must be built in or registered by a running extension, else
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
  symbolic links and empty folders are skipped.
- `"desktop": true` in the offer asks for the files to land on the
  receiver's desktop. It is honoured, without asking, only when the sender
  is allowed `interdesk/input` on the receiver; otherwise the offer is
  handled like any other.
- The receiver may wait for its user (`crossdrop accept`, up to 5 minutes)
  unless the sender's device is set to auto-accept. A refusal is
  `{"type":"answer","accepted":false,"offsets":[]}`.
- **Resume.** Data is written to `<downloads>/.crossdrop-partial/<key>.part`,
  `key = BLAKE3(sender id, path, size, mtime)`. When the same file is offered
  again, `offsets[i]` is the length already held; the sender skips those
  bytes but still hashes them, so the final check covers the whole file. A
  journal per transfer keeps the chosen root name and the files already
  completed. Leftovers untouched for 7 days are deleted.
- **Safety.** Path components are cleaned (control characters removed,
  `\ : * ? " < > |` replaced by `_`); `..`, empty components and absolute
  paths are refused. A root that already exists in the download folder gets a
  free name: `photos (1)`, `report (1).pdf`.

## Clipboard (`crossdrop/clipboard`)

One channel per update: the sender writes `{"text":"…"}`, the receiver
applies it and answers `{"type":"accepted"}`.

Sharing is opt-in per device and symmetric in intent: `crossdrop allow X
clipboard` lets X write to this device's clipboard **and** makes this device
send its own copies to X. Each daemon polls the system clipboard every
500 ms; text received from a peer is remembered so it is not sent back.
Headless devices without a system clipboard still accept and expose the last
text through `crossdrop clip get`.

## Reserved service names

| name                  | owner                    |
| --------------------- | ------------------------ |
| `crossdrop/*`         | built-in services        |
| `interdesk/input`     | interdesk (v0.3)         |

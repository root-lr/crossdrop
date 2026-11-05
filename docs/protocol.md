# crossdrop protocol, version 1

Status: draft, implemented by crossdrop v0.1. Breaking changes bump the ALPN.

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
(max 1 MiB). Tagged enums use a `"type"` field in `snake_case`. Bulk data is
never framed.

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

## Files (`crossdrop/files`)

Sender, for each file:

```
{"type":"file","name":"report.pdf","size":123456}   frame
<size raw bytes>
{"type":"file_end","blake3":"<hex>"}                frame
```

then `{"type":"end"}`. The receiver writes to a hidden `.name.crossdrop-part`
file, checks the BLAKE3 hash, renames to a free name in its download folder
(`name (1).ext` if taken) and finally answers `{"type":"done","received":N}`
or `{"type":"error","message":"…"}`. File names are reduced to their last
path component; control characters and leading dots are removed.

## Reserved service names

| name                  | owner                    |
| --------------------- | ------------------------ |
| `crossdrop/*`         | built-in services        |
| `interdesk/input`     | interdesk (v0.3)         |

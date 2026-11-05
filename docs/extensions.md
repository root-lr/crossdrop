# Writing a crossdrop extension

An extension is a separate program that offers a **service** to your paired
devices — interdesk (shared mouse and keyboard) is one. It never touches keys,
certificates or the network: the daemon does, and hands the extension plain
byte streams that are already authenticated, encrypted and permission-checked.

```
extension ──IPC──▶ crossdropd ══QUIC/TLS══ crossdropd ──IPC──▶ extension
```

## In Rust, with `crossdrop-sdk`

```rust
use crossdrop_sdk::Client;

// Serve "myapp/notes" on this device.
let mut reg = Client::connect_default().await?.register("myapp/notes").await?;
while let Some(incoming) = reg.next().await? {
    println!("channel from {}", incoming.device);
    let stream = incoming.accept().await?; // AsyncRead + AsyncWrite
    // … speak your own protocol on `stream`
}

// Open a channel to a paired device that serves "myapp/notes".
let stream = Client::connect_default().await?.open_channel("laptop", "myapp/notes").await?;
```

A runnable example lives in [`crates/crossdrop-sdk/examples/echo.rs`](../crates/crossdrop-sdk/examples/echo.rs).

The remote user must allow your service for your device first:

```sh
crossdrop allow <your device> myapp/notes
```

## Service names

`<project>/<feature>`: ASCII letters, digits, `/ - _ .`, at most 64
characters. `crossdrop/*` is reserved. Only one client may register a given
name at a time; the registration ends when its connection closes.

## The IPC protocol (any language)

Endpoint: `<data dir>/daemon.sock` (Unix socket) on Linux and macOS,
`\\.\pipe\crossdrop-<hash>` on Windows. The data directory is
`$CROSSDROP_HOME`, else the OS config directory + `crossdrop`.

Frames are a big-endian `u32` length followed by JSON, as on the wire. A
connection's **first request** decides what it is:

| first request | connection becomes |
| --- | --- |
| `{"type":"register","service":"myapp/notes"}` | an event stream: `{"type":"incoming_channel","channel_id":"…","device":"…","service":"…"}` |
| `{"type":"attach","channel_id":"…"}` | after `{"type":"ok"}`, a raw byte pipe to the remote service |
| anything else | request / response, as many as you like |

Request / response calls: `status`, `list_devices`, `pair_start`,
`pair_confirm`, `list_pair_requests`, `pair_answer`, `unpair`,
`set_permission`, `open_channel`, `list_offers`, `offer_answer`,
`set_auto_accept`, `clipboard_send`, `clipboard_get`. Errors come back as
`{"type":"error","message":"…"}`. Field names are in
[`crossdrop-proto`](../crates/crossdrop-proto/src/lib.rs).

To use a channel:

1. `open_channel` (outgoing) or receive `incoming_channel` (incoming): you get a `channel_id`.
2. Open a **new** IPC connection and send `attach` with that id within 30 s.
3. Read and write bytes. Closing your side closes the QUIC stream's send half.

## Security model

- Any process running as the same OS user can talk to the daemon; other users
  cannot (socket mode `0600` in a `0700` directory, user-only named pipe).
- An unpaired device can open nothing but pairing.
- A paired device reaches your service only after an explicit `crossdrop allow`.

# crossdrop

**A simple, open-source way to share your clipboard and files across devices.**

crossdrop links your computers with direct, end-to-end encrypted connections.
No account, no cloud, no server: devices find each other on the local
network, you pair them once by comparing a 6-digit code, and they talk
peer-to-peer over QUIC.

It is also a **platform**: extensions plug into the same secure links through
a small local API. The first one, **interdesk**, will let one mouse and
keyboard drive every PC on your desk.

> Status: **v0.1, early**. File transfer and pairing work on the LAN from the
> command line. Clipboard sync, the desktop app and interdesk are next — see
> the [roadmap](#roadmap).

## Features (v0.1)

- **Zero setup discovery** over mDNS, or connect by `ip:port`.
- **Pairing with a short code** (SAS) derived from the TLS session: a
  man-in-the-middle cannot produce matching codes.
- **Mutual TLS 1.3 over QUIC**, device identities pinned like Syncthing; no
  certificate authority.
- **Per-device, per-service permissions**: a paired device may send you files,
  and nothing else until you allow it.
- **File transfer** streamed with constant memory and verified with BLAKE3.
- **Extension API**: register a service, receive authenticated byte streams
  ([docs/extensions.md](docs/extensions.md)).

## Quick start

```sh
cargo install --git https://github.com/root-lr/crossdrop crossdrop-cli
```

On each computer:

```sh
crossdrop daemon            # keep it running (or use a service manager)
```

Then, from computer A:

```sh
crossdrop devices           # bravo  GTDT-W6OV  nearby  192.168.1.20:47470
crossdrop pair bravo        # shows a code, e.g. 387 668
```

On computer B:

```sh
crossdrop accept            # shows the same code; confirm
```

Send files:

```sh
crossdrop send report.pdf photos.zip --to bravo
```

Files land in `~/Downloads/crossdrop` (configurable in `config.toml`).

| Command | What it does |
| --- | --- |
| `crossdrop status` | This device's name, id and port |
| `crossdrop devices` | Paired and nearby devices |
| `crossdrop pair <name \| id \| ip:port>` | Start pairing |
| `crossdrop requests` / `accept` / `reject` | Answer pairing requests |
| `crossdrop send <files…> --to <device>` | Send files |
| `crossdrop allow <device> <service>` / `deny` | Manage permissions (`files`, `clipboard`, `input`, or a full name) |
| `crossdrop unpair <device>` | Forget a device |

All commands take `--home <dir>` to use another data directory, handy to run
two daemons on one machine.

## How it works

```
CLI / app / extensions ──IPC──▶ crossdropd ══QUIC + TLS 1.3══ crossdropd
```

One daemon per device owns the keys and the connections. Everything else —
the CLI, the future desktop app, interdesk — talks to it over a local socket.
Each service gets its own QUIC stream ("channel") on a shared connection.
Details: [docs/protocol.md](docs/protocol.md).

| Crate | Role |
| --- | --- |
| [`crossdrop-proto`](crates/crossdrop-proto) | Wire and IPC messages, framing |
| [`crossdrop-core`](crates/crossdrop-core) | Identity, TLS, QUIC endpoint, channels, mDNS |
| [`crossdrop-daemon`](crates/crossdrop-daemon) | `crossdropd`: trust, permissions, routing, built-in services |
| [`crossdrop-sdk`](crates/crossdrop-sdk) | Client library for the CLI, apps and extensions |
| [`crossdrop-cli`](crates/crossdrop-cli) | The `crossdrop` command |

## Roadmap

| Version | Scope |
| --- | --- |
| **v0.1** | Identity, pairing, encrypted channels, extension API, file transfer (CLI) |
| v0.2 | Folders, resumable transfers, accept/decline incoming files, clipboard sync |
| v0.3 | **interdesk**: shared mouse and keyboard (Windows, macOS, X11) |
| v0.4 | Desktop app (Tauri), drag & drop files across screens |
| v0.5 | Connections over the internet: hole punching, self-hostable relay |
| v1.0 | Frozen protocol, signed binaries, published benchmarks |

## Development

```sh
cargo test --workspace      # includes end-to-end tests with two daemons
cargo clippy --workspace --all-targets
```

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.

# Contributing to crossdrop

Thanks for helping. Bug reports, fixes and new extensions are all welcome.
For a security issue, follow [SECURITY.md](SECURITY.md) instead of opening
an issue.

## Layout

| Path | What lives there |
| --- | --- |
| `crates/crossdrop-proto` | Wire and IPC messages, framing |
| `crates/crossdrop-core` | Identity, TLS, QUIC endpoint, channels, mDNS |
| `crates/crossdrop-daemon` | `crossdropd`: trust, permissions, built-in services |
| `crates/crossdrop-sdk` | Client library used by the CLI, the app and extensions |
| `crates/crossdrop-cli` | The `crossdrop` command |
| `crates/interdesk` | Shared mouse and keyboard, an extension |
| `apps/desktop` | Tauri 2 + React desktop app (outside the Cargo workspace) |
| `docs/` | Protocol, extensions, interdesk |

## Before opening a pull request

The CI runs the same commands on Linux, macOS and Windows:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace          # includes end-to-end tests with two daemons
```

interdesk's X11 backend has a test that needs a real X server:

```sh
Xvfb :99 -screen 0 1920x1080x24 &
DISPLAY=:99 cargo test -p interdesk --lib -- --ignored
```

The desktop app builds on its own:

```sh
cd apps/desktop
npm ci && npm run build
cd src-tauri && cargo clippy --features custom-protocol -- -D warnings
```

The end-to-end tests run real daemons on loopback; a test that passes once
but fails sometimes is a bug too. Running the suite a few times in a row
before pushing catches most timing issues.

## Changes

- Keep a pull request to one change, with a test when the behaviour can be
  tested.
- Protocol changes stay compatible with peers running the previous release
  when possible (new fields optional and omitted when empty), and are
  described in `docs/protocol.md`.
- Commit messages follow the style already in the history: a type, an
  optional scope, and a summary in the imperative, then a body that says
  what was wrong and why the change fixes it.

  ```
  fix(daemon): refuse oversized clipboard text with a clear message
  ```

  Types in use: `feat`, `fix`, `perf`, `test`, `docs`, `ci`, `build`,
  `chore`.

## License

By contributing, you agree that your contributions are licensed under the
same terms as the project: MIT or Apache-2.0, at the user's option.

## Fuzzing

The frame decoder and the cleaning of device names have fuzz targets in
[`fuzz/`](fuzz/README.md) (`cargo +nightly fuzz run frames`).

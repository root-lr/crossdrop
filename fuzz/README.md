# Fuzzing

Targets for [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) (needs a
nightly toolchain):

```sh
cargo install cargo-fuzz
cargo +nightly fuzz run frames        # frame decoding of every message type
cargo +nightly fuzz run device_name   # cleaning names received from devices
```

CI runs each target for a short while on every push to `main`.

# third_party

- `xattr/` – `xattr` 0.2.3 (MIT OR Apache-2.0, https://github.com/Stebalien/xattr) with a one-line fix in
  `src/sys/mod.rs` so it builds against current `libc` on Linux. Wired in through `[patch.crates-io]` in the root
  `Cargo.toml`. Remove the patch once `zed-async-tar` moves to a fixed `xattr`.

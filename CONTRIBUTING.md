# Contributing

Thanks for your interest in `pnio`.

## Development

```bash
cargo test                                   # unit + integration tests
cargo fmt --all                              # format (max_width = 100)
cargo clippy --all-targets -- -D warnings    # lint (warnings are errors)
```

All three must be clean before a PR (CI also checks the library on the MSRV, `rust-version` in
`Cargo.toml`, and `cargo doc` with `-D warnings`). The `AF_PACKET` backend and the HIL examples
are Linux-only.

## Guidelines

- **Pure Rust**, no bundled third-party C stack; no GPL/copyleft code.
- Wire-format code is validated **byte-exact** against real captures
  (see [`docs/dcp-golden-frames.md`](docs/dcp-golden-frames.md)); add a test vector
  with any new frame type.
- No IEC standard text copied into the repo — paraphrase only.
- Keep big-endian on the wire.

## License

By contributing, you agree your contribution is dual-licensed under
**MIT OR Apache-2.0**, matching the project.

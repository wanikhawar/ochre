# Contributing

Bug reports, ideas and pull requests are welcome. For a bigger change, please
open an issue first so we can agree on the approach.

## Building

See [Developing](README.md#developing) in the README: get pdfium into
`vendor/pdfium`, then `cargo run --release -- some.pdf`.

## Before opening a pull request

CI runs these, and they must pass:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The tests include headless end-to-end runs of the real UI against pdfium; they
need pdfium in `vendor/pdfium` (or `$OCHRE_PDFIUM`), otherwise they skip
themselves.

## Releases

Bump the version in `Cargo.toml`, then push a tag `vX.Y.Z`. The release
workflow builds the Linux archive (`packaging/make-tarball.sh`) and attaches it
to the release for that tag.

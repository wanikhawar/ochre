#!/usr/bin/env bash
# Builds the release archive dist/ochre-<version>-x86_64-linux.tar.gz and its
# .sha256: the binary, pdfium (from vendor/pdfium) and its licenses, the
# install/uninstall scripts and the desktop entry and icon.
# Used by the release workflow; also works locally.
set -euo pipefail
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
name="ochre-$version-x86_64-linux"
[[ -f vendor/pdfium/lib/libpdfium.so ]] || { echo "vendor/pdfium is missing (see README, Developing)" >&2; exit 1; }

cargo build --release --locked

rm -rf "dist/$name" "dist/$name.tar.gz" "dist/$name.tar.gz.sha256"
dir="dist/$name"
mkdir -p "$dir/lib" "$dir/licenses/pdfium"
install -m755 target/release/ochre install.sh uninstall.sh "$dir/"
install -m644 README.md LICENSE packaging/ochre.desktop packaging/ochre.svg "$dir/"
install -m644 vendor/pdfium/lib/libpdfium.so "$dir/lib/"
install -m644 vendor/pdfium/LICENSE vendor/pdfium/licenses/* "$dir/licenses/pdfium/"

tar -C dist -czf "dist/$name.tar.gz" "$name"
(cd dist && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
echo "dist/$name.tar.gz"

#!/usr/bin/env bash
# Installs Ochre for the current user (default prefix: ~/.local).
# Works from a source checkout (builds with cargo) or an extracted release
# archive (uses the prebuilt binary). Set PREFIX to install elsewhere.
set -euo pipefail
cd "$(dirname "$0")"
PREFIX="${PREFIX:-$HOME/.local}"

if [[ -f Cargo.toml ]]; then
    cargo build --release
    bin=target/release/ochre
    lib=vendor/pdfium/lib/libpdfium.so
    assets=packaging
    if [[ ! -f $lib ]]; then
        echo "Downloading pdfium..."
        mkdir -p vendor/pdfium
        curl -fsSL https://github.com/bblanchon/pdfium-binaries/releases/latest/download/pdfium-linux-x64.tgz \
            | tar -xz -C vendor/pdfium
    fi
else
    bin=ochre
    lib=lib/libpdfium.so
    assets=.
fi

install -Dm755 "$bin" "$PREFIX/bin/ochre"
install -Dm644 "$lib" "$PREFIX/lib/ochre/libpdfium.so"
# Absolute path: app launchers often don't have ~/.local/bin on their PATH.
mkdir -p "$PREFIX/share/applications"
sed -e "s|^Exec=ochre |Exec=$PREFIX/bin/ochre |" -e "s|^TryExec=.*|TryExec=$PREFIX/bin/ochre|" \
    "$assets/ochre.desktop" > "$PREFIX/share/applications/ochre.desktop"
chmod 644 "$PREFIX/share/applications/ochre.desktop"
install -Dm644 "$assets/ochre.svg" "$PREFIX/share/icons/hicolor/scalable/apps/ochre.svg"
command -v update-desktop-database >/dev/null && update-desktop-database "$PREFIX/share/applications" || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q "$PREFIX/share/icons/hicolor" 2>/dev/null || true

echo "Installed Ochre to $PREFIX/bin/ochre"
case ":$PATH:" in *":$PREFIX/bin:"*) ;; *) echo "Note: $PREFIX/bin is not on your PATH." ;; esac

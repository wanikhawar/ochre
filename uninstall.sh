#!/usr/bin/env bash
# Removes what install.sh installed (your settings in ~/.config/ochre are kept).
set -euo pipefail
PREFIX="${PREFIX:-$HOME/.local}"
rm -f "$PREFIX/bin/ochre" "$PREFIX/share/applications/ochre.desktop" \
      "$PREFIX/share/icons/hicolor/scalable/apps/ochre.svg"
rm -rf "$PREFIX/lib/ochre"
echo "Removed Ochre from $PREFIX"

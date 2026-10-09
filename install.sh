#!/bin/sh
# Builds Sonance and installs it for the current user (no root needed).
#   ./install.sh            build + install to ~/.local
#   ./install.sh uninstall  remove it again
set -e
BIN="$HOME/.local/bin"
APPS="$HOME/.local/share/applications"

if [ "$1" = "uninstall" ]; then
    rm -f "$BIN/sonance" "$APPS/dev.sonance.Sonance.desktop"
    echo "Sonance removed. Settings are kept in ~/.config/sonance (delete it to reset)."
    exit 0
fi

missing=""
for tool in cargo pactl pw-cat pw-play ffmpeg; do
    command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
if [ -n "$missing" ]; then
    echo "Missing:$missing  (see README: Requirements)" >&2
    exit 1
fi

cargo build --release
install -Dm755 target/release/sonance "$BIN/sonance"
# Absolute Exec path: ~/.local/bin isn't on every desktop session's PATH.
sed "s|^Exec=sonance|Exec=$BIN/sonance|" data/dev.sonance.Sonance.desktop > /tmp/dev.sonance.Sonance.desktop
install -Dm644 /tmp/dev.sonance.Sonance.desktop "$APPS/dev.sonance.Sonance.desktop"
rm -f /tmp/dev.sonance.Sonance.desktop
command -v update-desktop-database >/dev/null && update-desktop-database "$APPS" 2>/dev/null || true
echo "Installed: $BIN/sonance (and an app-menu entry)."

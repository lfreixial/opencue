#!/usr/bin/env bash
# Install OpenCUE for the current user.
#   ./install.sh          build and install binaries, service and desktop entry (no root)
#   sudo ./install.sh --udev   install the udev rule that grants device access (needs root, once)
set -euo pipefail
cd "$(dirname "$0")"

if [[ "${1:-}" == "--udev" ]]; then
    install -Dm644 data/70-opencue.rules /etc/udev/rules.d/70-opencue.rules
    udevadm control --reload-rules
    udevadm trigger --subsystem-match=hidraw --subsystem-match=misc
    echo "udev rule installed. Corsair devices are now accessible to the logged-in user."
    exit 0
fi

if [[ $EUID -eq 0 ]]; then
    echo "Run without sudo (only '--udev' needs root)." >&2
    exit 1
fi

cargo build --release
install -Dm755 target/release/opencue "$HOME/.local/bin/opencue"
install -Dm755 target/release/opencue-gui "$HOME/.local/bin/opencue-gui"
install -Dm644 data/opencue.service "$HOME/.config/systemd/user/opencue.service"
install -Dm644 data/opencue.desktop "$HOME/.local/share/applications/opencue.desktop"
install -Dm644 data/opencue.svg "$HOME/.local/share/icons/hicolor/scalable/apps/opencue.svg"
systemctl --user daemon-reload
systemctl --user enable opencue.service
systemctl --user restart opencue.service

echo
echo "OpenCUE installed. Launch it from your app menu or run: opencue-gui"
if [[ ! -e /etc/udev/rules.d/70-opencue.rules ]]; then
    echo "One more step (needs your password once): sudo ./install.sh --udev"
fi

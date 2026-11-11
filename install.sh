#!/usr/bin/env bash
# Install OpenCUE for the current user.
#   ./install.sh               install binaries, service and desktop entry (no root). From a source
#                              checkout this builds first; from a release download it uses the
#                              binaries in bin/.
#   sudo ./install.sh --udev   install the udev rule that grants device access (needs root, once)
#   ./install.sh --uninstall   remove everything ./install.sh installed for this user
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

if [[ "${1:-}" == "--uninstall" ]]; then
    systemctl --user disable --now opencue.service 2>/dev/null || true
    rm -f "$HOME/.local/bin/opencue" "$HOME/.local/bin/opencue-gui" \
        "$HOME/.config/systemd/user/opencue.service" \
        "$HOME/.local/share/applications/opencue.desktop" \
        "$HOME/.local/share/icons/hicolor/scalable/apps/opencue.svg" \
        "$HOME/.config/autostart/opencue.desktop"
    systemctl --user daemon-reload
    echo "OpenCUE removed. Your settings are kept in ~/.config/opencue."
    if [[ -e /etc/udev/rules.d/70-opencue.rules ]]; then
        echo "To remove the device access rule too: sudo rm /etc/udev/rules.d/70-opencue.rules"
    fi
    exit 0
fi

# A release download ships prebuilt binaries in bin/; a source checkout builds them.
if [[ -x bin/opencue && -x bin/opencue-gui ]]; then
    bin=bin
else
    if ! command -v cargo >/dev/null; then
        echo "Building from source needs Rust (pacman -S rust, or https://rustup.rs)." >&2
        echo "Or download a prebuilt release: https://github.com/lfreixial/opencue/releases/latest" >&2
        exit 1
    fi
    cargo build --release
    bin=target/release
fi

if [[ -x /usr/bin/opencue && "$(command -v opencue)" == /usr/bin/opencue ]]; then
    echo "Note: OpenCUE is also installed system-wide (/usr/bin/opencue, e.g. from the AUR)." >&2
    echo "This user install takes over for you; remove one of them to avoid confusion." >&2
fi

install -Dm755 "$bin/opencue" "$HOME/.local/bin/opencue"
install -Dm755 "$bin/opencue-gui" "$HOME/.local/bin/opencue-gui"
install -Dm644 data/opencue.service "$HOME/.config/systemd/user/opencue.service"
# App launchers don't read the shell's PATH, which often lacks ~/.local/bin, so point
# the desktop entry at the installed binary directly.
install -d "$HOME/.local/share/applications"
sed "s|^Exec=opencue-gui$|Exec=$HOME/.local/bin/opencue-gui|" data/opencue.desktop \
    > "$HOME/.local/share/applications/opencue.desktop"
chmod 644 "$HOME/.local/share/applications/opencue.desktop"
# Open the app minimized at login. Left alone if it exists, since the app's Settings change it.
if [[ ! -e "$HOME/.config/autostart/opencue.desktop" ]]; then
    install -d "$HOME/.config/autostart"
    sed "s|^Exec=opencue-gui|Exec=$HOME/.local/bin/opencue-gui|" data/opencue-autostart.desktop \
        > "$HOME/.config/autostart/opencue.desktop"
fi
command -v update-desktop-database >/dev/null && update-desktop-database -q "$HOME/.local/share/applications" || true
install -Dm644 data/opencue.svg "$HOME/.local/share/icons/hicolor/scalable/apps/opencue.svg"
systemctl --user daemon-reload
systemctl --user enable opencue.service
systemctl --user restart opencue.service

echo
echo "OpenCUE installed. Launch it from your app menu or run: opencue-gui"
if [[ ! -e /etc/udev/rules.d/70-opencue.rules && ! -e /usr/lib/udev/rules.d/70-opencue.rules ]]; then
    echo "One more step (needs your password once): sudo ./install.sh --udev"
fi

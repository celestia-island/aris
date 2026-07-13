#!/bin/bash
# ============================================================
#  Entelecheia Gateway — Evernight auto-installer for macOS
# ============================================================
#  When the user double-clicks this .command file, Terminal opens
#  and this script runs. It installs the evernight client.
#
#  What it does:
#    1. Detects Intel vs Apple Silicon
#    2. Installs the binary to /usr/local/bin
#    3. Creates a launchd agent (auto-start on login)
#    4. Configures the USB-C NCM network interface
#    5. Opens Safari to the gateway dashboard
# ============================================================

set -euo pipefail

echo ""
echo "  ============================================"
echo "    Entelecheia Gateway — Evernight Installer"
echo "  ============================================"
echo ""

# --- Determine script directory ---
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
USB_ROOT="$(dirname "${SCRIPT_DIR}")"

# --- Check architecture ---
ARCH="$(uname -m)"
case "${ARCH}" in
    x86_64)  EVERNIGHT_BIN="evernight-darwin-amd64" ;;
    arm64)   EVERNIGHT_BIN="evernight-darwin-arm64" ;;
    *)
        echo "  [!] Unsupported architecture: ${ARCH}"
        exit 1
        ;;
esac

echo "  [ok] macOS ${ARCH} detected."

# --- Install binary ---
echo "  [..] Installing evernight to /usr/local/bin/ ..."

SRC="${USB_ROOT}/common/${EVERNIGHT_BIN}"
DEST="/usr/local/bin/evernight"

if [ ! -f "${SRC}" ]; then
    echo "  [!!] ${SRC} not found on the USB drive."
    echo "       Please re-flash the gateway firmware."
    exit 1
fi

sudo mkdir -p /usr/local/bin
sudo cp "${SRC}" "${DEST}"
sudo chmod +x "${DEST}"

# Remove quarantine attribute (gatekeeper)
sudo xattr -d com.apple.quarantine "${DEST}" 2>/dev/null || true

echo "  [ok] evernight installed."

# --- Configure USB-C NCM interface ---
# macOS automatically creates a "USB NCM" interface when the gadget connects.
USB_IFACE="$(ifconfig -l 2>/dev/null | tr ' ' '\n' | grep -iE 'ncm|usb' | head -1 || true)"

if [ -n "${USB_IFACE}" ]; then
    echo "  [..] Configuring ${USB_IFACE} ..."
    # macOS uses networksetup or ipconfig for DHCP
    sudo ipconfig set "${USB_IFACE}" DHCP 2>/dev/null || \
        sudo ifconfig "${USB_IFACE}" 10.0.99.100 netmask 255.255.255.0 2>/dev/null || true
    echo "  [ok] ${USB_IFACE} configured."
else
    echo "  [!] No USB NCM interface detected."
    echo "      Check System Settings > Network for a new interface."
fi

# --- Register launchd agent ---
echo "  [..] Registering launchd agent..."

PLIST_PATH="${HOME}/Library/LaunchAgents/com.celestia-island.evernight-gateway.plist"
mkdir -p "$(dirname "${PLIST_PATH}")"

cat > "${PLIST_PATH}" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.celestia-island.evernight-gateway</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/evernight</string>
        <string>serve</string>
        <string>--mode</string>
        <string>client</string>
        <string>--gateway</string>
        <string>10.0.99.1:50000</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/tmp/evernight-gateway.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/evernight-gateway.err</string>
</dict>
</plist>
PLIST

launchctl unload "${PLIST_PATH}" 2>/dev/null || true
launchctl load "${PLIST_PATH}"
echo "  [ok] launchd agent installed and started."

# --- Open dashboard ---
echo "  [..] Opening dashboard..."
open "http://10.0.99.1:8080" 2>/dev/null || true

echo ""
echo "  ============================================"
echo "    Installation complete!"
echo "  ============================================"
echo ""
echo "  Dashboard: http://10.0.99.1:8080"
echo "  Manage:    launchctl list | grep evernight"
echo "  Remove:    launchctl unload ${PLIST_PATH}"
echo ""

#!/bin/bash
# ------
#  Entelecheia Gateway — Evernight auto-installer for Linux
# ------
#  This script installs the evernight client on a Linux machine
#  when the gateway is connected via USB-C.
#
#  Usage:  ./linux/install_evernight.sh
#
#  What it does:
#    1. Detects Linux distribution and architecture
#    2. Installs the evernight binary to /usr/local/bin
#    3. Creates a systemd service (or openrc script)
#    4. Configures the USB-C NCM network interface
#    5. Registers this machine as a node with the gateway
#    5. Opens the browser to the gateway dashboard
# ------

set -euo pipefail

echo ""
echo "  ============================================"
echo "    Entelecheia Gateway — Evernight Installer"
echo "  ============================================"
echo ""

# --- Determine script directory (USB drive root) ---
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
USB_ROOT="$(dirname "${SCRIPT_DIR}")"

# --- Check architecture ---
ARCH="$(uname -m)"
case "${ARCH}" in
    x86_64)  EVERNIGHT_BIN="evernight-linux-amd64" ;;
    aarch64) EVERNIGHT_BIN="evernight-linux-arm64" ;;
    *)
        echo "  [!] Unsupported architecture: ${ARCH}"
        exit 1
        ;;
esac

echo "  [ok] Linux ${ARCH} detected."

# --- Check root privileges ---
if [ "$(id -u)" -ne 0 ]; then
    echo "  [!] This installer needs root privileges."
    echo "      Re-running with sudo..."
    exec sudo -E bash "$0" "$@"
fi

# --- Install binary ---
echo "  [..] Installing evernight to /usr/local/bin/ ..."

SRC="${USB_ROOT}/common/${EVERNIGHT_BIN}"
DEST="/usr/local/bin/evernight"

if [ ! -f "${SRC}" ]; then
    echo "  [!!] ${SRC} not found on the USB drive."
    echo "       Please re-flash the gateway firmware."
    exit 1
fi

cp "${SRC}" "${DEST}"
chmod +x "${DEST}"
echo "  [ok] evernight installed."

# --- Configure USB-C NCM interface ---
# The NCM gadget creates a usb0 / enxc0... interface on the host side.
USB_IFACE="$(ip -o link show | grep -E 'usb|ncm' | awk -F': ' '{print $2}' | head -1 || true)"

if [ -n "${USB_IFACE}" ]; then
    echo "  [..] Configuring USB network interface ${USB_IFACE} ..."
    # Try DHCP first (gateway runs a DHCP server on 10.0.99.x)
    dhclient "${USB_IFACE}" 2>/dev/null || \
        ip addr add 10.0.99.100/24 dev "${USB_IFACE}" 2>/dev/null || true
    ip link set "${USB_IFACE}" up 2>/dev/null || true
    echo "  [ok] ${USB_IFACE} configured."
else
    echo "  [!] No USB network interface found. The NCM driver may not be loaded."
    echo "      Try: sudo modprobe cdc_ncm"
fi

# --- Register as systemd service ---
if command -v systemctl &>/dev/null; then
    echo "  [..] Registering systemd service..."

    cat > /etc/systemd/system/evernight-gateway.service <<'UNIT'
[Unit]
Description=Entelecheia Evernight Gateway Client
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/evernight serve --mode client --gateway 10.0.99.1:50000
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT

    systemctl daemon-reload
    systemctl enable --now evernight-gateway
    echo "  [ok] systemd service installed and started."

elif command -v rc-update &>/dev/null; then
    echo "  [..] Registering OpenRC service..."

    cat > /etc/init.d/evernight-gateway <<'RC'
#!/sbin/openrc-run
description="Entelecheia Evernight Gateway Client"
command="/usr/local/bin/evernight"
command_args="serve --mode client --gateway 10.0.99.1:50000"
command_background=true
pidfile="/run/evernight-gateway.pid"
depend() {
    need net
}
RC
    chmod +x /etc/init.d/evernight-gateway
    rc-update add evernight-gateway default
    rc-service evernight-gateway start
    echo "  [ok] OpenRC service installed and started."

else
    echo "  [!] Neither systemd nor OpenRC detected."
    echo "      evernight is installed but not registered as a service."
fi

# --- Open dashboard ---
echo "  [..] Opening dashboard..."
if command -v xdg-open &>/dev/null; then
    xdg-open "http://10.0.99.1:8080" 2>/dev/null || true
fi

echo ""
echo "  ============================================"
echo "    Installation complete!"
echo "  ============================================"
echo ""
echo "  Dashboard: http://10.0.99.1:8080"
echo "  Manage:    systemctl status evernight-gateway"
echo ""

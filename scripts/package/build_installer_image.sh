#!/bin/bash
# ------
#  build_installer_image.sh — Assemble the USB mass-storage image
# ------
#
#  This script is run during firmware build (or manually) to create
#  the FAT32 image that the USB composite gadget exposes as a virtual
#  USB drive to the host.
#
#  The image contains:
#    /autorun.inf                  — Windows AutoRun trigger
#    /windows/install_evernight.bat
#    /linux/install_evernight.sh
#    /macos/install_evernight.command
#    /android/install_evernight.txt
#    /common/README.txt
#    /common/evernight-windows-amd64.exe
#    /common/evernight-linux-amd64
#    /common/evernight-linux-arm64
#    /common/evernight-darwin-amd64
#    /common/evernight-darwin-arm64
#
#  Usage:
#    ./build_installer_image.sh [output_path] [evernight_build_dir]
#
#  Example:
#    ./build_installer_image.sh output/installer.img ../evernight/target
# ------

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
OUTPUT="${1:-${SCRIPT_DIR}/../output/installer.img}"
EVERNIGHT_DIR="${2:-${SCRIPT_DIR}/../output/evernight-binaries}"
IMAGE_SIZE_MB="${IMAGE_SIZE_MB:-32}"
PAYLOAD_DIR="${PAYLOAD_DIR:-$(mktemp -d)}"

echo "Building installer image → ${OUTPUT}"
echo "Payload staging dir: ${PAYLOAD_DIR}"

# --- Stage the payload directory ---
mkdir -p "${PAYLOAD_DIR}/windows" \
         "${PAYLOAD_DIR}/linux" \
         "${PAYLOAD_DIR}/macos" \
         "${PAYLOAD_DIR}/android" \
         "${PAYLOAD_DIR}/common"

# Copy installer scripts from package/
cp -r "${SCRIPT_DIR}/windows/"*     "${PAYLOAD_DIR}/windows/"
cp -r "${SCRIPT_DIR}/linux/"*       "${PAYLOAD_DIR}/linux/"
cp -r "${SCRIPT_DIR}/macos/"*       "${PAYLOAD_DIR}/macos/"
cp -r "${SCRIPT_DIR}/android/"*     "${PAYLOAD_DIR}/android/"
cp -r "${SCRIPT_DIR}/common/"*      "${PAYLOAD_DIR}/common/"
# Copy root-level files (autorun.inf, etc.) — exclude build scripts
for f in "${SCRIPT_DIR}"/*; do
    [ -f "$f" ] || continue
    case "$(basename "$f")" in
        build_installer_image.sh) continue ;;
    esac
    cp "$f" "${PAYLOAD_DIR}/"
done

# Make scripts executable
chmod +x "${PAYLOAD_DIR}/linux/"*.sh \
         "${PAYLOAD_DIR}/macos/"*.command 2>/dev/null || true

# --- Copy evernight binaries (cross-compiled) ---
# These are expected to be pre-built by the firmware build system.
copy_binary() {
    local src="$1"
    local dest="$2"
    if [ -f "${src}" ]; then
        cp "${src}" "${dest}"
        echo "  [ok] ${dest}"
    else
        echo "  [skip] ${src} not found (will not be included)"
    fi
}

if [ -d "${EVERNIGHT_DIR}" ]; then
    copy_binary "${EVERNIGHT_DIR}/x86_64-pc-windows-gnu/release/evernight.exe" \
                "${PAYLOAD_DIR}/common/evernight-windows-amd64.exe"
    copy_binary "${EVERNIGHT_DIR}/x86_64-unknown-linux-musl/release/evernight" \
                "${PAYLOAD_DIR}/common/evernight-linux-amd64"
    copy_binary "${EVERNIGHT_DIR}/aarch64-unknown-linux-musl/release/evernight" \
                "${PAYLOAD_DIR}/common/evernight-linux-arm64"
    copy_binary "${EVERNIGHT_DIR}/x86_64-apple-darwin/release/evernight" \
                "${PAYLOAD_DIR}/common/evernight-darwin-amd64"
    copy_binary "${EVERNIGHT_DIR}/aarch64-apple-darwin/release/evernight" \
                "${PAYLOAD_DIR}/common/evernight-darwin-arm64"
else
    echo "  [!] Evernight binaries directory not found: ${EVERNIGHT_DIR}"
    echo "      The installer image will not contain the evernight client."
    echo "      Build evernight for each target first, or provide the directory."
fi

# --- Create the FAT32 image ---
echo "Creating ${IMAGE_SIZE_MB} MB FAT32 image..."

mkdir -p "$(dirname "${OUTPUT}")"

# Create zero-filled file
dd if=/dev/zero of="${OUTPUT}" bs=1M count="${IMAGE_SIZE_MB}" status=none

# Format as FAT32
mkfs.vfat -F 32 -n "ARIS_GW" "${OUTPUT}" >/dev/null

# Copy payload using mtools (mcopy) — works without root
if command -v mcopy &>/dev/null; then
    echo "Populating image with mcopy..."
    mcopy -s -i "${OUTPUT}" "${PAYLOAD_DIR}/." "::"
    echo "  [ok] image populated"
elif command -v mount &>/dev/null && [ "$(id -u)" -eq 0 ]; then
    echo "Populating image with mount (root)..."
    MOUNT_POINT="$(mktemp -d)"
    mount -o loop "${OUTPUT}" "${MOUNT_POINT}"
    cp -r "${PAYLOAD_DIR}/." "${MOUNT_POINT}/"
    sync
    umount "${MOUNT_POINT}"
    rmdir "${MOUNT_POINT}"
    echo "  [ok] image populated"
else
    echo "  [!] Neither mcopy nor root mount available."
    echo "      Install mtools (apt install mtools) for non-root image building."
    echo "      The image is created but empty."
fi

# --- Cleanup ---
rm -rf "${PAYLOAD_DIR}"

echo ""
echo "Done: ${OUTPUT} (${IMAGE_SIZE_MB} MB)"
echo "Install to: /usr/share/evernight-gadget/installer.img"

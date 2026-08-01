#!/bin/bash
# ------
#  Build the installer FAT image using Docker for root operations.
#  This wraps build_installer_image.sh — Docker provides the root
#  environment needed for loop-mount, even when the host user isn't root.
# ------

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ARIS_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
ARIS_TEST_TMP="${ARIS_TEST_TMP:-/tmp/aris-test}"
OUTPUT_DIR="${ARIS_TEST_TMP}/output"
EVERNIGHT_DIR="${ARIS_ROOT}/tests/fixtures/evernight-binaries"

mkdir -p "${OUTPUT_DIR}"

echo "Building installer image via Docker..."
echo "  aris root:   ${ARIS_ROOT}"
echo "  evernight:   ${EVERNIGHT_DIR}"
echo "  output:      ${OUTPUT_DIR}/installer.img"

# Run the build inside Docker (Ubuntu image with dosfstools + mtools)
docker run --rm \
    -v "${ARIS_ROOT}/package:/pkg:ro" \
    -v "${OUTPUT_DIR}:/output" \
    -v "${EVERNIGHT_DIR}:/evernight:ro" \
    -e IMAGE_SIZE_MB=16 \
    ubuntu:22.04 \
    bash -c '
        set -euo pipefail
        apt-get update -qq && apt-get install -y -qq dosfstools mtools >/dev/null 2>&1

        IMAGE="/output/installer.img"
        PAYLOAD="$(mktemp -d)"

        mkdir -p "${PAYLOAD}/windows" "${PAYLOAD}/linux" "${PAYLOAD}/macos" \
                 "${PAYLOAD}/android" "${PAYLOAD}/common"

        cp -r /pkg/windows/*     "${PAYLOAD}/windows/"
        cp -r /pkg/linux/*       "${PAYLOAD}/linux/"
        cp -r /pkg/macos/*       "${PAYLOAD}/macos/"
        cp -r /pkg/android/*     "${PAYLOAD}/android/"
        cp -r /pkg/common/*      "${PAYLOAD}/common/"
        # Copy root-level files (autorun.inf, etc.) — exclude build scripts
        for f in /pkg/*; do
            [ -f "$f" ] || continue
            case "$(basename "$f")" in
                build_installer_image.sh) continue ;;
            esac
            cp "$f" "${PAYLOAD}/"
        done
        chmod +x "${PAYLOAD}/linux/"*.sh "${PAYLOAD}/macos/"*.command 2>/dev/null || true

        cp /evernight/x86_64-pc-windows-gnu/release/evernight.exe   "${PAYLOAD}/common/evernight-windows-amd64.exe"
        cp /evernight/x86_64-unknown-linux-musl/release/evernight   "${PAYLOAD}/common/evernight-linux-amd64"
        cp /evernight/aarch64-unknown-linux-musl/release/evernight  "${PAYLOAD}/common/evernight-linux-arm64"
        cp /evernight/x86_64-apple-darwin/release/evernight         "${PAYLOAD}/common/evernight-darwin-amd64"
        cp /evernight/aarch64-apple-darwin/release/evernight        "${PAYLOAD}/common/evernight-darwin-arm64"
        chmod +x "${PAYLOAD}/common/evernight-linux-"* "${PAYLOAD}/common/evernight-darwin-"* 2>/dev/null || true

        echo "Creating ${IMAGE_SIZE_MB:-64} MB FAT image..."
        dd if=/dev/zero of="${IMAGE}" bs=1M count="${IMAGE_SIZE_MB:-64}" status=none
        # Use FAT16 for smaller images (< 512MB), FAT32 for larger
        FATSZ="16"
        if [ "${IMAGE_SIZE_MB:-64}" -ge 512 ]; then FATSZ="32"; fi
        mkfs.vfat -F "${FATSZ}" -n "ARIS_GW" "${IMAGE}" >/dev/null

        echo "Populating image with mcopy..."
        # Copy each top-level item individually (avoids mcopy "." entry issue)
        for item in "${PAYLOAD}"/*; do
            echo "  adding $(basename "${item}")..."
            mcopy -s -i "${IMAGE}" "${item}" "::"
        done

        echo ""
        echo "Verifying image..."
        mdir -i "${IMAGE}" ::

        rm -rf "${PAYLOAD}"
        echo "DONE: ${IMAGE}"
    '

echo ""
echo "Image size: $(ls -lh "${OUTPUT_DIR}/installer.img" | awk '{print $5}')"
echo "Image at:   ${OUTPUT_DIR}/installer.img"

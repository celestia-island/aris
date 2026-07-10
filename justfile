# aris — build commands
# Usage: just <recipe>

set unstable
set shell := ["bash", "-c"]
set windows-shell := ["wsl.exe", "bash", "-c"]
set lists

import "./celestia-devtools.just"

default: build

# ── Environment ─────────────────────────────────────────────

# Inspect the build environment: host kind, WSL2 distros (on Windows),
# selected distro, and container backend. Pre-flight check before build.
env-check:
    {{python_cmd}} scripts/check_env.py

# Build/fetch the HMI browser engine (webkitgtk | servo | cef) per [display]
# config. Renders the gateway dashboard on an attached screen.
build-browser BOARD="nanopi-r3s":
    {{python_cmd}} scripts/build_browser.py {{BOARD}}

# ── Development ────────────────────────────────────────────

check:
    cargo check --workspace

lint:
    cargo clippy --workspace -- -D warnings

test:
    cargo test --workspace

# Format Rust + Markdown docs
fmt:
    cargo fmt --all
    just fmt-markdown

# Check formatting without modifying
fmt-check:
    cargo fmt --all -- --check
    just fmt-markdown --check

# ── Cross-compilation Setup ────────────────────────────────

setup-cross:
    {{python_cmd}} scripts/setup_cross.py

# ── Build ──────────────────────────────────────────────────

# Build firmware with kei kernel (default, Phase 2)
build:
    just cache-guard
    {{python_cmd}} scripts/build.py nanopi-r3s --kernel-source kei

# Build firmware with Linux kernel (Phase 1)
build-linux:
    just cache-guard
    {{python_cmd}} scripts/build.py nanopi-r3s --kernel-source linux

build-board BOARD:
    {{python_cmd}} scripts/build.py {{BOARD}} --kernel-source kei

# ── Flash ──────────────────────────────────────────────────

flash-sd DEVICE="/dev/sdb":
    {{python_cmd}} scripts/flash_sd.py {{DEVICE}}

flash-board BOARD DEVICE="/dev/sdb":
    {{python_cmd}} scripts/flash_sd.py -b {{BOARD}} {{DEVICE}}

# ── Testing ────────────────────────────────────────────────

# First ignition test: evernight-server + Modbus sim + sensor-poll (host, no QEMU)
ignition-test:
    {{python_cmd}} scripts/ignition_test.py

# QEMU ignition test with Linux kernel backend (baseline)
qemu-ignition-linux:
    {{python_cmd}} scripts/qemu_ignition.py --kernel linux

# QEMU ignition test with kei kernel backend (experimental)
qemu-ignition-kei:
    {{python_cmd}} scripts/qemu_ignition.py --kernel kei

# QEMU ignition test with official asterinas backend
qemu-ignition-asterinas:
    {{python_cmd}} scripts/qemu_ignition.py --kernel asterinas

qemu-test:
    {{python_cmd}} scripts/qemu_test.py nanopi-r3s

hw-test:
    cargo test --test hardware -- --test-threads=1

# ── Utilities ──────────────────────────────────────────────

# ── Testing ────────────────────────────────────────────────

# Run all USB gadget tests
test-gadget:
    python3 tests/run_all.py

# Quick test run (skip image build and QEMU)
test-quick:
    python3 tests/run_all.py --quick

# Build the USB mass-storage installer image (exposed to hosts via USB-C)
build-installer-image OUTPUT="output/installer.img" EVERNIGHT_DIR="output/evernight-binaries":
    bash package/build_installer_image.sh {{OUTPUT}} {{EVERNIGHT_DIR}}

# Create fixture binaries for testing
create-fixtures:
    bash tests/fixtures/create_fixtures.sh

# ── Windows Testing ──────────────────────────────────────

# Test Windows installer via Wine (fast, no VM needed)
test-windows-wine:
    python3 tests/installer/test_windows_wine.py

# Install Windows DLLs via winetricks for better Wine compatibility
wine-setup:
    export WINEPREFIX="${WINEPREFIX:-$$HOME/.wine-aris}"
    export WINE=/usr/lib/wine/wine64
    /tmp/winetricks corefonts vcrun2019

# Run a Windows batch file through Wine
wine-bat BAT:
    export WINEPREFIX="${WINEPREFIX:-$$HOME/.wine-aris}"
    export WINE=/usr/lib/wine/wine64
    /usr/lib/wine/wine64 cmd /c "Z:$(realpath {{BAT}} | tr / '\')"

# QEMU Windows VM: check status
windows-status:
    python3 tests/windows/setup_vm.py --status

# QEMU Windows VM: auto-download Win11 eval ISO + setup VM (one-time, ~6.6GB)
windows-setup:
    python3 tests/windows/setup_vm.py --auto-download

# QEMU Windows VM: boot and run USB gadget test (requires --download first)
windows-test:
    python3 tests/windows/setup_vm.py --test

# QEMU Windows VM: boot interactively (VNC on localhost:5900)
windows-interactive:
    python3 tests/windows/setup_vm.py --interactive

dev-shell:
    {{python_cmd}} scripts/dev_shell.py

clean:
    rm -rf output/ target/ build/
    cargo clean

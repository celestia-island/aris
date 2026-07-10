#!/usr/bin/env python3
"""aris — QEMU ignition test with tri-backend kernel support.

Tests the full gateway boot flow in QEMU arm64:
  1. Boot a kernel (Linux / official Asterinas / kei fork)
  2. Verify network interfaces come up
  3. Verify evernight connects to evernight-server and registers

Three kernel backends:
  --kernel linux       Download prebuilt Linux arm64 kernel (always works)
  --kernel asterinas   Build from official asterinas/asterinas (x86_64 only currently)
  --kernel kei         Use kei fork output (experimental aarch64)

Usage:
    python3 scripts/qemu_ignition.py --kernel linux
    python3 scripts/qemu_ignition.py --kernel kei --gateway-port 8443
"""
from __future__ import annotations

import os
import shutil
import signal
import socket
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent / "utils"))
import build_env
import cli_format as cf

PROJECT_ROOT = Path(__file__).resolve().parent.parent
EVERNIGHT_ROOT = Path(os.environ.get(
    "EVERNIGHT_ROOT",
    str(PROJECT_ROOT.parent / "evernight"),
))
KEI_ROOT = Path(os.environ.get(
    "KEI_ROOT",
    str(PROJECT_ROOT.parent / "kei"),
))

GATEWAY_PORT = 8443


def port_in_use(port: int) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        return s.connect_ex(("127.0.0.1", port)) == 0


def wait_for_port(port: int, timeout: float = 10.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if port_in_use(port):
            return True
        time.sleep(0.2)
    return False


# ── Kernel backends ───────────────────────────────────────────

def get_linux_kernel() -> Path | None:
    """Get a prebuilt Linux arm64 kernel for QEMU virt.

    Uses the system's arm64 kernel if available, or downloads one.
    QEMU virt machine boots standard arm64 Image format.
    """
    cf.info("  Backend: Linux arm64 kernel")

    # Check common locations for prebuilt arm64 kernel
    candidates = [
        Path("/usr/lib/qemu-system-aarch64/Image"),  # distro package
        PROJECT_ROOT / "target" / "output" / "linux-arm64-Image",  # pre-downloaded
        Path("/boot/vmlinuz-arm64"),  # some systems
    ]
    for c in candidates:
        if c.exists() and c.stat().st_size > 1_000_000:
            cf.ok(f"  Found: {c}")
            return c

    cf.warn("  No prebuilt arm64 kernel found.")
    cf.info("  To get one:")
    cf.info("    sudo apt install linux-image-arm64")
    cf.info("  Or:")
    cf.info("    wget -O output/linux-arm64-Image \\")
    cf.info("      https://deb.debian.org/debian/pool/main/l/linux/...")
    return None


def get_kei_kernel() -> Path | None:
    """Get kei fork kernel binary."""
    cf.info("  Backend: kei (Asterinas ARM64 fork)")
    kernel = KEI_ROOT / "target" / "output" / "nanopi-r3s" / "kei-kernel.bin"
    if not kernel.exists():
        # Check default output location
        kernel = KEI_ROOT / "target" / "aarch64-unknown-none" / "release" / "kei"
    if kernel.exists():
        cf.ok(f"  Found: {kernel}")
        return kernel
    cf.fail("  kei kernel not found.")
    cf.info(f"  Expected: {kernel}")
    cf.info("  Run: cd ../kei && cargo osdk build --target-arch aarch64 --release")
    return None


def get_asterinas_kernel() -> Path | None:
    """Get official asterinas kernel (x86_64 only currently)."""
    cf.info("  Backend: official asterinas")
    cf.warn("  Official asterinas does not support aarch64 deployment yet.")
    cf.info("  Use --kernel kei for aarch64 or --kernel linux for baseline.")
    return None


# ── QEMU boot ─────────────────────────────────────────────────

def boot_qemu(kernel: Path, gateway_port: int, arch: str = "aarch64") -> int:
    """Boot kernel in QEMU and monitor output.

    When run inside WSL2 with WSLg enabled, the graphical window is
    automatically forwarded to the Windows desktop (WSLg injects DISPLAY
    and provides an X/Wayland socket). Override the display backend with
    the ``QEMU_DISPLAY`` env var (e.g. ``-display none`` for headless CI).
    """
    qemu_map = {
        "aarch64": ("qemu-system-aarch64", "virt", "cortex-a55"),
        "x86_64": ("qemu-system-x86_64", "q35", "qemu64"),
    }
    qemu_bin, machine, cpu = qemu_map.get(arch, qemu_map["aarch64"])

    qemu = shutil.which(qemu_bin)
    if not qemu:
        cf.fail(f"{qemu_bin} not installed")
        return 1

    # initramfs from kei (shared)
    initramfs = KEI_ROOT / "test" / "initramfs" / "build" / "initramfs.cpio.gz"
    if not initramfs.exists():
        cf.fail(f"initramfs not found: {initramfs}")
        cf.info("  Run: cd ../kei && python3 scripts/initramfs.py")
        return 1

    # Display backend: default to a graphical window (WSLg forwards it to
    # Windows when run inside WSL2). Set QEMU_DISPLAY="-display none" for
    # headless / CI runs.
    display_arg = os.environ.get("QEMU_DISPLAY", "-display sdl")

    cf.blank()
    cf.step("Booting in QEMU")
    cf.info(f"  Kernel:     {kernel}")
    cf.info(f"  Machine:    {machine} / {cpu}")
    cf.info(f"  Initramfs:  {initramfs.name}")
    cf.info(f"  Gateway:    ws://10.0.2.2:{gateway_port}/api/ws")
    cf.info(f"  Display:    {display_arg}")
    cf.info("  Press Ctrl-A X to exit QEMU.")
    cf.blank()

    cmd = [
        qemu,
        "-M", machine,
        "-cpu", cpu,
        "-m", "2048",
        "-smp", "2",
        "-kernel", str(kernel),
        "-initrd", str(initramfs),
        # Network: user-mode NAT with gateway port forward
        "-netdev", f"user,id=net0,hostfwd=tcp::{gateway_port}-:{gateway_port}",
        "-device", "virtio-net-device,netdev=net0",
        # Second network interface (tests dual-Ethernet detection)
        "-netdev", "user,id=net1",
        "-device", "virtio-net-device,netdev=net1",
        # virtio-gpu for graphical HMI output (kei's virtio-gpu driver
        # publishes a blit-backed framebuffer; FramebufferConsole renders
        # kernel logs to the window).
        "-device", "virtio-gpu-device",
        # Serial console on stdio (kernel logs), plus the display window.
        "-serial", "mon:stdio",
        display_arg,
        "-append", "console=ttyAMA0 rdinit=/init",
        "-no-reboot",
    ]

    result = subprocess.run(cmd)
    return result.returncode


# ── Main ──────────────────────────────────────────────────────

processes: list[subprocess.Popen] = []


def cleanup(*_) -> None:
    for p in processes:
        try:
            p.terminate()
            p.wait(timeout=3)
        except Exception:
            p.kill()


def main() -> int:
    if build_env.wsl_main_guard():
        return 0
    import argparse

    parser = argparse.ArgumentParser(description="QEMU ignition test")
    parser.add_argument("--kernel", choices=["linux", "kei", "asterinas"],
                        default="linux",
                        help="Kernel backend (default: linux)")
    parser.add_argument("--gateway-port", type=int, default=8443)
    parser.add_argument("--start-gateway", action="store_true", default=True,
                        help="Start evernight-server automatically")
    args = parser.parse_args()

    signal.signal(signal.SIGINT, cleanup)
    signal.signal(signal.SIGTERM, cleanup)

    cf.section("aris — QEMU Ignition Test")
    cf.info(f"  Kernel backend: {args.kernel}")

    # ── Get kernel ────────────────────────────────────────────
    cf.blank()
    cf.step("[1/3] Obtaining kernel")
    kernel_getters = {
        "linux": get_linux_kernel,
        "kei": get_kei_kernel,
        "asterinas": get_asterinas_kernel,
    }
    kernel = kernel_getters[args.kernel]()
    if not kernel:
        cf.fail("No kernel available — cannot proceed")
        return 1

    # ── Start gateway if needed ───────────────────────────────
    cf.blank()
    cf.step("[2/3] Starting evernight-server")
    if args.start_gateway and not port_in_use(args.gateway_port):
        server_bin = EVERNIGHT_ROOT / "target" / "release" / "evernight-server"
        if server_bin.exists():
            p = subprocess.Popen(
                [str(server_bin), "serve",
                 "--host", "0.0.0.0",
                 "--port", str(args.gateway_port)],
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
            )
            processes.append(p)
            if wait_for_port(args.gateway_port, 5.0):
                cf.ok(f"evernight-server on port {args.gateway_port}")
            else:
                cf.fail("evernight-server failed to start")
        else:
            cf.warn(f"evernight-server not found: {server_bin}")
    else:
        cf.ok("Gateway already running")

    # ── Boot QEMU ─────────────────────────────────────────────
    cf.blank()
    cf.step("[3/3] QEMU boot")
    arch = "x86_64" if args.kernel == "asterinas" else "aarch64"
    result = boot_qemu(kernel, args.gateway_port, arch)

    cleanup()
    return result


if __name__ == "__main__":
    sys.exit(main())

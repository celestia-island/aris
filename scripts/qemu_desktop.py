#!/usr/bin/env python3
"""QEMU desktop launcher for aris HMI testing.

Boots a Linux kernel + aris rootfs in QEMU with virtio-gpu display,
launching the HMI kiosk browser (Blitz + Vello CPU, fbdev backend)
pointed at the evernight dashboard.

Usage:
    python3 scripts/qemu_desktop.py [board] [--kernel-source linux|kei]

By default uses the Linux kernel backend (Phase 1). Use --kernel-source kei
for the kei kernel backend (Phase 2+, experimental).

The script reads the board config from configs/<board>.toml to determine:
  - Display resolution and kiosk URL
  - QEMU machine type and CPU
  - evernight features to compile

The rendering engine is Blitz + Vello CPU rendering directly to /dev/fb0
via virtio-gpu. No X11/Wayland compositor required. The aris kiosk binary
(kei_fbtest or similar) runs inside the VM and writes RGBA frames to the
framebuffer.
"""

import argparse
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parent.parent
CONFIGS_DIR = PROJECT_ROOT / "configs"
OUTPUT_DIR = PROJECT_ROOT / "output"


def find_qemu(arch: str) -> str:
    """Find the QEMU binary for the given architecture."""
    qemu = f"qemu-system-{arch}"
    path = shutil.which(qemu)
    if path:
        return path
    # Try Windows path
    win_path = f"C:/Program Files/qemu/{qemu}.exe"
    if os.path.exists(win_path):
        return win_path
    print(f"ERROR: {qemu} not found. Install QEMU for {arch}.")
    sys.exit(1)


def build_qemu_cmd(
    board: str,
    kernel: Path,
    initrd: Path,
    dtb: Path | None,
    config: dict,
    gateway_port: int = 8443,
) -> list[str]:
    """Build the QEMU command line from board config."""
    qemu_section = config.get("qemu", {})
    display_section = config.get("display", {})
    machine = qemu_section.get("machine", "virt")
    cpu = qemu_section.get("cpu", "cortex-a72")
    smp = str(qemu_section.get("smp", 2))
    ram = str(config.get("ram_mb", 4096))
    display = qemu_section.get("display", "sdl")
    devices = qemu_section.get("devices", ["virtio-net-device"])

    qemu_bin = find_qemu(config.get("arch", "aarch64"))

    cmd = [
        qemu_bin,
        "-M", machine,
        "-cpu", cpu,
        "-m", ram,
        "-smp", smp,
        "--no-reboot",
        "-display", display,
    ]

    if dtb:
        cmd.extend(["-dtb", str(dtb)])

    # Network: user-mode NAT with gateway port forward
    cmd.extend([
        "-netdev", f"user,id=net0,hostfwd=tcp::{gateway_port}-:{gateway_port}",
    ])

    for dev in devices:
        if dev == "virtio-net-device":
            cmd.extend(["-device", f"{dev},netdev=net0"])
        else:
            cmd.extend(["-device", dev])

    # Serial console
    cmd.extend(["-serial", "mon:stdio"])

    # Kernel and initrd
    cmd.extend([
        "-kernel", str(kernel),
        "-initrd", str(initrd),
        "-append", "console=ttyAMA0 rdinit=/init",
    ])

    return cmd


def main():
    parser = argparse.ArgumentParser(description="QEMU desktop launcher for aris HMI")
    parser.add_argument("board", nargs="?", default="qemu-hmi",
                       help="Board config name (default: qemu-hmi)")
    parser.add_argument("--kernel-source", choices=["linux", "kei"], default="linux",
                       help="Kernel backend (default: linux for Phase 1)")
    parser.add_argument("--gateway-port", type=int, default=8443,
                       help="Port to forward for evernight gateway (default: 8443)")
    parser.add_argument("--display", default=None,
                       help="Override QEMU display backend (sdl/gtk/none/vnc=:0)")
    args = parser.parse_args()

    # Load board config
    import tomllib
    config_path = CONFIGS_DIR / f"{args.board}.toml"
    if not config_path.exists():
        print(f"ERROR: Board config not found: {config_path}")
        sys.exit(1)

    with open(config_path, "rb") as f:
        config = tomllib.load(f)

    display_section = config.get("display", {})
    engine = display_section.get("engine", "none")
    if engine == "none":
        print(f"WARNING: Board '{args.board}' has display engine = 'none' (headless)")
    else:
        res = display_section.get("resolution", [1024, 768])
        url = display_section.get("kiosk_url", "http://127.0.0.1:8080/")
        print(f"[desktop] Board: {args.board}")
        print(f"[desktop] Engine: {engine}")
        print(f"[desktop] Resolution: {res[0]}x{res[1]}")
        print(f"[desktop] Kiosk URL: {url}")

    # Find kernel
    board_output = OUTPUT_DIR / args.board
    if args.kernel_source == "kei":
        # Look for kei kernel in sibling directory
        kei_root = PROJECT_ROOT.parent / "kei"
        kernel = kei_root / "target" / "output" / args.board / "kei-kernel.bin"
        if not kernel.exists():
            # Fallback to QEMU ELF
            kernel = kei_root / "target" / "osdk" / "aster-kernel" / "aster-kernel-osdk-bin.qemu_elf"
        if not kernel.exists():
            print(f"ERROR: kei kernel not found. Run 'cd ../kei && just build-arch aarch64'")
            sys.exit(1)
    else:
        # Linux kernel: look for prebuilt Image
        kernel = board_output / "Image"
        if not kernel.exists():
            # Try system QEMU kernel
            qemu_img = Path("/usr/lib/qemu-system-aarch64/Image")
            if qemu_img.exists():
                kernel = qemu_img
            else:
                print(f"ERROR: Linux kernel Image not found at {kernel}")
                print(f"Hint: Install qemu-system-aarch64 package or build Linux kernel")
                sys.exit(1)

    # Find initramfs
    kei_root = PROJECT_ROOT.parent / "kei"
    initrd = kei_root / "test" / "initramfs" / "build" / "initramfs_aarch64.cpio.gz"
    if not initrd.exists():
        initrd = board_output / "initramfs.cpio.gz"
    if not initrd.exists():
        print(f"ERROR: initramfs not found")
        sys.exit(1)

    dtb = board_output / "board.dtb" if (board_output / "board.dtb").exists() else None

    print(f"[desktop] Kernel: {kernel}")
    print(f"[desktop] Initrd: {initrd}")
    if dtb:
        print(f"[desktop] DTB: {dtb}")
    print(f"[desktop] Kernel source: {args.kernel_source}")
    print()

    # Override display if specified
    if args.display:
        config.setdefault("qemu", {})["display"] = args.display

    # Build and run QEMU
    cmd = build_qemu_cmd(args.board, kernel, initrd, dtb, config, args.gateway_port)
    print(f"[desktop] QEMU command:")
    print(f"  {' '.join(cmd[:6])} ...")
    print()

    try:
        proc = subprocess.run(cmd)
        sys.exit(proc.returncode)
    except KeyboardInterrupt:
        print("\n[desktop] Interrupted by user")


if __name__ == "__main__":
    main()

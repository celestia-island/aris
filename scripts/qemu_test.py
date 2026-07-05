#!/usr/bin/env python3
"""aris — QEMU arm64 VM smoke test.

Boots the firmware image in an emulated aarch64 environment.

Usage:
    python3 scripts/qemu_test.py [board]
    python3 scripts/qemu_test.py nanopi-r3s
"""
from __future__ import annotations

import shutil
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent / "utils"))
import cli_format as cf

PROJECT_ROOT = Path(__file__).resolve().parent.parent


def main() -> int:
    import argparse

    parser = argparse.ArgumentParser(description="QEMU boot test")
    parser.add_argument("board", nargs="?", default="nanopi-r3s")
    args = parser.parse_args()

    board = args.board
    output_dir = PROJECT_ROOT / "target" / "output" / board
    image_path = output_dir / "image.img"

    if not image_path.exists():
        cf.fail(f"Image not found: {image_path}")
        cf.info("  Run: python3 scripts/build.py " + board)
        return 1

    qemu = shutil.which("qemu-system-aarch64")
    if not qemu:
        cf.fail("qemu-system-aarch64 not installed")
        cf.info("  Install: sudo apt install qemu-system-arm")
        return 1

    kernel = output_dir / "Image"
    dtb = output_dir / "rk3566-nanopi-r3s.dtb"

    cf.section(f"Booting {board} in QEMU (arm64)")

    cmd = [qemu, "-M", "virt", "-cpu", "cortex-a55", "-m", "2048", "-smp", "4"]
    if kernel.exists():
        cmd.extend(["-kernel", str(kernel)])
    if dtb.exists():
        cmd.extend(["-dtb", str(dtb)])
    cmd.extend([
        "-drive", f"file={image_path},format=raw,if=virtio",
        "-netdev", "user,id=net0,hostfwd=tcp::2222-:22",
        "-device", "virtio-net-device,netdev=net0",
        "-nographic",
        "-no-reboot",
    ])

    cf.pending(" ".join(cmd[:6]) + " ...")
    result = subprocess.run(cmd)
    return result.returncode


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""aris — flash firmware image to SD card.

Usage:
    python3 scripts/flash_sd.py [-b BOARD] /dev/sdX
    python3 scripts/flash_sd.py /dev/sdb
    python3 scripts/flash_sd.py -b nanopi-r3s /dev/sdb
"""
from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent / "utils"))
import cli_format as cf

PROJECT_ROOT = Path(__file__).resolve().parent.parent


def find_latest_image() -> Path | None:
    output_dir = PROJECT_ROOT / "output"
    if not output_dir.exists():
        return None
    images = sorted(output_dir.glob("*/image.img"), key=lambda p: p.stat().st_mtime)
    return images[-1] if images else None


def main() -> int:
    import argparse

    parser = argparse.ArgumentParser(description="Flash aris firmware to SD card")
    parser.add_argument("device", help="Block device (e.g. /dev/sdb)")
    parser.add_argument("-b", "--board", default=None, help="Board name")
    args = parser.parse_args()

    device = args.device
    if not os.path.exists(device):
        cf.fail(f"Device not found: {device}")
        cf.info("  Check with: lsblk")
        return 1

    if args.board:
        image_path = PROJECT_ROOT / "output" / args.board / "image.img"
    else:
        image_path = find_latest_image()

    if not image_path or not image_path.exists():
        cf.fail("Image not found. Run build first.")
        return 1

    cf.section(f"Flashing {image_path.name} to {device}")
    cf.warn("This will DESTROY all data on the device.")
    response = input("Continue? [y/N] ")
    if response.lower() != "y":
        cf.info("Aborted.")
        return 0

    cf.pending("Writing image...")
    result = subprocess.run(
        ["sudo", "dd",
         f"if={image_path}",
         f"of={device}",
         "bs=4M",
         "status=progress",
         "conv=fsync"],
    )
    if result.returncode != 0:
        cf.fail("dd failed")
        return 1

    subprocess.run(["sync"], check=False)
    cf.ok("Firmware flashed successfully")
    cf.info("Remove SD card and insert into device.")
    return 0


if __name__ == "__main__":
    sys.exit(main())

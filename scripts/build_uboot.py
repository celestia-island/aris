#!/usr/bin/env python3
"""aris — obtain or build U-Boot for a target board.

For Rockchip boards (RK3566/RK3588), U-Boot consists of two artifacts:
  - idbloader.img  — SPL + DDR init, written at SD offset 32KB (sector 64)
  - u-boot.itb     — U-Boot proper FIT image, written at SD offset 8MB

Strategy (in order of preference):
  1. Use prebuilt binaries from board directory (board/<board>/uboot/)
  2. Download prebuilt from official vendor/manufacturer
  3. Build from mainline U-Boot source inside Docker (needs aarch64 GCC)

Usage:
    python3 scripts/build_uboot.py nanopi-r3s
    python3 scripts/build_uboot.py nanopi-r3s --build   # build from source
"""
from __future__ import annotations

import shutil
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent / "utils"))
import build_env
import cli_format as cf

PROJECT_ROOT = Path(__file__).resolve().parent.parent

# Prebuilt U-Boot download URLs keyed by board name.
# These point to FriendlyELEC's official GitHub release assets.
PREBUILT_URLS = {
    "nanopi-r3s": {
        # FriendlyELEC rk3568 uboot works for rk3566 nanopi-r3s
        # We bundle our own prebuilt in board/nanopi-r3s/uboot/ instead.
    },
}


def find_prebuilt(board: str) -> dict[str, Path] | None:
    """Look for prebuilt U-Boot artifacts in board/<board>/uboot/."""
    uboot_dir = PROJECT_ROOT / "board" / board / "uboot"
    artifacts = {}
    for name in ("idbloader.img", "u-boot.itb"):
        p = uboot_dir / name
        if p.exists():
            artifacts[name] = p
    if "idbloader.img" in artifacts and "u-boot.itb" in artifacts:
        return artifacts
    return None


def build_uboot_docker(board: str, output_dir: Path) -> dict[str, Path] | None:
    """Build U-Boot from mainline source inside a Docker container.

    Uses the official U-Boot defconfig for the board. Produces
    idbloader.img and u-boot.itb.
    """
    board_cfg = {
        "nanopi-r3s": {
            "defconfig": "nanopi-r3s-rk3566_defconfig",
            "arch": "arm64",
        },
    }

    cfg = board_cfg.get(board)
    if not cfg:
        cf.fail(f"No U-Boot build config for board '{board}'")
        return None

    cf.info(f"  Building U-Boot from mainline source ({cfg['defconfig']})")
    cf.info("  This requires Docker and ~2GB download (one-time)")

    uboot_src = PROJECT_ROOT / "uboot-src"
    docker_image = "ubuntu:22.04"

    # Clone U-Boot if not present
    if not (uboot_src / ".git").exists():
        cf.pending("  Cloning mainline U-Boot...")
        subprocess.run(
            ["git", "clone", "--depth=1",
             "https://github.com/u-boot/u-boot.git",
             str(uboot_src)],
            check=False,
        )

    if not (uboot_src / "Makefile").exists():
        cf.fail("Failed to clone U-Boot source")
        return None

    # Build in Docker with cross-compiler
    cmd = f"""
set -ex
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq build-essential bc bison flex libssl-dev \
    libgnutls28-dev swig gcc-aarch64-linux-gnu python3-dev \
    >/dev/null 2>&1

cd /uboot
make CROSS_COMPILE=aarch64-linux-gnu- {cfg['defconfig']}
make CROSS_COMPILE=aarch64-linux-gnu- -j$(nproc) tools

# idbloader.img = spl/u-boot-spl.bin wrapped with Rockchip header
if [ -f spl/u-boot-spl.bin ]; then
    ./tools/mkimage -n -T rksd -d spl/u-boot-spl.bin /output/idbloader.img
fi

# u-boot.itb is produced by the main build
if [ -f u-boot.itb ]; then
    cp u-boot.itb /output/u-boot.itb
fi

echo "BUILD_OK"
"""

    result = subprocess.run(
        [*build_env.docker_cmd(), "run", "--rm",
         "-v", f"{uboot_src}:/uboot",
         "-v", f"{output_dir}:/output",
         docker_image,
         "bash", "-c", cmd],
        capture_output=True, text=True,
    )

    if result.returncode != 0:
        cf.fail("U-Boot Docker build failed")
        cf.info(result.stderr[-500:] if result.stderr else "")
        return None

    artifacts = {}
    for name in ("idbloader.img", "u-boot.itb"):
        p = output_dir / name
        if p.exists():
            artifacts[name] = p

    if artifacts:
        cf.ok(f"  Built: {list(artifacts.keys())}")
        return artifacts

    cf.fail("U-Boot build produced no artifacts")
    return None


def obtain_uboot(board: str, output_dir: Path, force_build: bool = False) -> dict[str, Path] | None:
    """Main entry: obtain U-Boot artifacts for a board."""
    cf.step(f"Obtaining U-Boot for {board}")

    # Strategy 1: prebuilt in board directory
    if not force_build:
        prebuilt = find_prebuilt(board)
        if prebuilt:
            cf.ok(f"  Found prebuilt: {list(prebuilt.keys())}")
            result = {}
            for name, src in prebuilt.items():
                dst = output_dir / name
                shutil.copy2(src, dst)
                result[name] = dst
            return result

    # Strategy 2: build from source in Docker
    return build_uboot_docker(board, output_dir)


if __name__ == "__main__":
    if build_env.wsl_main_guard():
        sys.exit(0)
    import argparse
    parser = argparse.ArgumentParser(description="Obtain U-Boot for a board")
    parser.add_argument("board", nargs="?", default="nanopi-r3s")
    parser.add_argument("--build", action="store_true", help="Build from source (Docker)")
    args = parser.parse_args()

    out = PROJECT_ROOT / "output" / args.board
    out.mkdir(parents=True, exist_ok=True)
    result = obtain_uboot(args.board, out, force_build=args.build)
    if result:
        print("\nU-Boot artifacts:")
        for name, path in result.items():
            print(f"  {name}: {path}")
    else:
        sys.exit(1)

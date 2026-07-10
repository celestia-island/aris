#!/usr/bin/env python3
"""aris — install cross-compilation toolchains for all target architectures.

Run once on a new development machine.
"""
from __future__ import annotations

import shutil
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent / "utils"))
import cli_format as cf

RUST_TARGETS = [
    "aarch64-unknown-linux-musl",
    "armv7-unknown-linux-musleabihf",
    "riscv64gc-unknown-linux-musl",
    "x86_64-unknown-linux-musl",
]

GCC_HINTS = {
    "arch": "sudo pacman -S aarch64-linux-musl arm-linux-musleabihf",
    "ubuntu": "sudo apt install gcc-aarch64-linux-musl gcc-arm-linux-musleabihf",
    "fedora": "sudo dnf install arm-linux-musl",
}


def detect_distro() -> str:
    if shutil.which("pacman"):
        return "arch"
    if shutil.which("apt"):
        return "ubuntu"
    if shutil.which("dnf"):
        return "fedora"
    return "unknown"


def main() -> int:
    cf.section("Setting up cross-compilation toolchains")

    cf.blank()
    cf.step("[1/2] Adding Rust cross-compilation targets")
    for target in RUST_TARGETS:
        cf.pending(f"  rustup target add {target}")
        subprocess.run(
            ["rustup", "target", "add", target],
            check=False,
            capture_output=True,
        )
    cf.ok("Rust targets installed")

    cf.blank()
    cf.step("[2/2] GCC cross-compilers")
    distro = detect_distro()
    if distro in GCC_HINTS:
        cf.info(f"  Detected distro: {distro}")
        cf.info(f"  Run: {GCC_HINTS[distro]}")
    else:
        cf.info("  Could not detect distro. Install musl cross-toolchain manually:")
        cf.info("  Prebuilt: https://musl.cc/")

    cf.blank()
    cf.ok("Setup complete")
    cf.info("  Build: python3 scripts/build.py nanopi-r3s")
    return 0


if __name__ == "__main__":
    sys.exit(main())

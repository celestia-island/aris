#!/usr/bin/env python3
"""build_installer_image.py — Assemble the USB mass-storage image.

Python port of the former build_installer_image.sh (de-bash doctrine: no
.sh/.ps1 helper scripts in the justfile chain). This tool is Linux-side:
it needs dd/mkfs.vfat plus mtools (mcopy) or root loop-mounting.

The image is the FAT32 payload the USB composite gadget exposes to the host:

    /autorun.inf                   — Windows AutoRun trigger
    /windows/install_evernight.bat
    /linux/install_evernight.sh
    /macos/install_evernight.command
    /android/install_evernight.txt
    /common/README.txt
    /common/evernight-<os>-<arch>  — pre-built evernight clients

Usage:
    python build_installer_image.py [output_path] [evernight_build_dir]
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent

BINARIES = [
    ("x86_64-pc-windows-gnu/release/evernight.exe", "evernight-windows-amd64.exe"),
    ("x86_64-unknown-linux-musl/release/evernight", "evernight-linux-amd64"),
    ("aarch64-unknown-linux-musl/release/evernight", "evernight-linux-arm64"),
    ("x86_64-apple-darwin/release/evernight", "evernight-darwin-amd64"),
    ("aarch64-apple-darwin/release/evernight", "evernight-darwin-arm64"),
]


def have(tool: str) -> bool:
    return shutil.which(tool) is not None


def main() -> int:
    output = Path(sys.argv[1]) if len(sys.argv) > 1 else SCRIPT_DIR / "../output/installer.img"
    evernight_dir = Path(sys.argv[2]) if len(sys.argv) > 2 else SCRIPT_DIR / "../output/evernight-binaries"
    image_size_mb = int(os.environ.get("IMAGE_SIZE_MB", "32"))

    print(f"Building installer image → {output}")
    payload = Path(tempfile.mkdtemp(prefix="aris-installer-"))
    print(f"Payload staging dir: {payload}")

    # --- Stage the payload directory ---
    for sub in ("windows", "linux", "macos", "android", "common"):
        (payload / sub).mkdir(parents=True)

    # Copy installer scripts from package/<sub>/.
    for sub in ("windows", "linux", "macos", "android", "common"):
        src_dir = SCRIPT_DIR / sub
        if src_dir.is_dir():
            shutil.copytree(src_dir, payload / sub, dirs_exist_ok=True)
    # Copy root-level files (autorun.inf, …) — exclude build scripts.
    for f in SCRIPT_DIR.iterdir():
        if f.is_file() and f.suffix not in (".py", ".sh"):
            shutil.copy2(f, payload / f.name)

    # Make installer scripts executable.
    for pattern in ("linux/*.sh", "macos/*.command"):
        for f in payload.glob(pattern):
            f.chmod(0o755)

    # --- Copy evernight binaries (cross-compiled by the firmware build) ---
    if evernight_dir.is_dir():
        for rel, name in BINARIES:
            src = evernight_dir / rel
            dest = payload / "common" / name
            if src.is_file():
                shutil.copy2(src, dest)
                print(f"  [ok] {dest}")
            else:
                print(f"  [skip] {src} not found (will not be included)")
    else:
        print(f"  [!] Evernight binaries directory not found: {evernight_dir}")
        print("      The installer image will not contain the evernight client.")
        print("      Build evernight for each target first, or provide the directory.")

    # --- Create the FAT32 image (Linux tooling required) ---
    print(f"Creating {image_size_mb} MB FAT32 image...")
    output.parent.mkdir(parents=True, exist_ok=True)

    with open(output, "wb") as f:
        f.truncate(image_size_mb * 1024 * 1024)

    subprocess.run(
        ["mkfs.vfat", "-F", "32", "-n", "ARIS_GW", str(output)],
        check=True, stdout=subprocess.DEVNULL,
    )

    if have("mcopy"):
        print("Populating image with mcopy...")
        subprocess.run(
            ["mcopy", "-s", "-i", str(output), str(payload) + "/.", "::"],
            check=True,
        )
        print("  [ok] image populated")
    elif have("mount") and os.geteuid() == 0:
        print("Populating image with mount (root)...")
        mount_point = Path(tempfile.mkdtemp(prefix="aris-mount-"))
        subprocess.run(["mount", "-o", "loop", str(output), str(mount_point)], check=True)
        shutil.copytree(payload, mount_point, dirs_exist_ok=True)
        subprocess.run(["sync"])
        subprocess.run(["umount", str(mount_point)], check=True)
        mount_point.rmdir()
        print("  [ok] image populated")
    else:
        print("  [!] Neither mcopy nor root mount available.")
        print("      Install mtools (apt install mtools) for non-root image building.")
        print("      The image is created but empty.")

    shutil.rmtree(payload, ignore_errors=True)

    print()
    print(f"Done: {output} ({image_size_mb} MB)")
    print("Install to: /usr/share/evernight-gadget/installer.img")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

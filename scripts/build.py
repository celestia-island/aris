#!/usr/bin/env python3
"""aris — build firmware image for a target board.

Produces a complete bootable firmware image:
  1. Cross-compile aris-core (supervisor)
  2. Cross-compile evernight (protocol broker)
  3. Obtain kernel (from kei or build Linux)
  4. Assemble rootfs (musl + busybox + binaries)
  5. Package SD card image (placeholder)

Usage:
    python3 scripts/build.py [board] [--kernel-source linux|kei]
    python3 scripts/build.py nanopi-r3s
    python3 scripts/build.py nanopi-r3s --kernel-source kei
"""
from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib

sys.path.insert(0, str(Path(__file__).parent / "utils"))
import cli_format as cf

PROJECT_ROOT = Path(__file__).resolve().parent.parent

# evernight source locations (checked in order)
EVERNIGHT_CANDIDATES = [
    PROJECT_ROOT.parent / "evernight",           # sibling directory
    Path(os.environ.get("EVERNIGHT_ROOT", "")),  # env override
]

# kei output locations (checked in order)
KEI_CANDIDATES = [
    PROJECT_ROOT.parent / "kei" / "target" / "output",      # sibling directory
    Path(os.environ.get("KEI_ROOT", "")) / "output",
]


def load_board_config(board: str) -> dict:
    config_path = PROJECT_ROOT / "configs" / f"{board}.toml"
    if not config_path.exists():
        cf.warn(f"Config not found: {config_path}, using defaults")
        return {
            "name": board, "arch": "aarch64",
            "evernight_features": ["hardware", "protocol", "serial", "sensor",
                                   "bin", "api", "manifest"],
            "entelecheia_server": "",
        }
    with config_path.open("rb") as f:
        return tomllib.load(f)


def find_evernight_source() -> Path | None:
    for candidate in EVERNIGHT_CANDIDATES:
        if candidate.is_dir() and (candidate / "Cargo.toml").exists():
            return candidate
    return None


def find_kei_kernel(board: str) -> Path | None:
    for base in KEI_CANDIDATES:
        kernel = base / board / "kei-kernel.bin"
        if kernel.exists():
            return kernel
    return None


def build_aris_core(rust_target: str) -> bool:
    cf.step("[2/7] Building aris-core")
    result = subprocess.run(
        ["cargo", "build", "--package", "core",
         "--target", rust_target, "--release"],
        cwd=PROJECT_ROOT,
    )
    if result.returncode != 0:
        cf.fail("aris-core build failed")
        return False
    cf.ok("aris-core built")
    return True


def build_evernight(rust_target: str, features: list[str]) -> Path | None:
    cf.step("[3/7] Building evernight")

    evernight_src = find_evernight_source()
    if not evernight_src:
        cf.warn("evernight source not found")
        cf.info("  Checked:")
        for c in EVERNIGHT_CANDIDATES:
            cf.info(f"    {c}")
        cf.info("  Set EVERNIGHT_ROOT or clone evernight as a sibling directory")
        cf.info("  Skipping evernight build — rootfs will not contain it")
        return None

    cf.info(f"  Source: {evernight_src}")
    cf.info(f"  Target: {rust_target}")
    cf.info(f"  Features: {', '.join(features)}")

    cmd = [
        "cargo", "build",
        "--target", rust_target,
        "--release",
        "--no-default-features",
        "--features", ",".join(features),
    ]
    result = subprocess.run(cmd, cwd=evernight_src)
    if result.returncode != 0:
        cf.fail("evernight build failed")
        return None

    binary = evernight_src / "target" / rust_target / "release" / "evernight"
    if not binary.exists():
        # Check CARGO_TARGET_DIR
        target_dir = Path(os.environ.get("CARGO_TARGET_DIR", ""))
        if target_dir:
            binary = target_dir / rust_target / "release" / "evernight"
    if not binary.exists():
        cf.fail(f"evernight binary not found at {binary}")
        return None

    cf.ok("evernight built")
    return binary


def obtain_kernel(board: str, kernel_source: str, arch: str) -> Path | None:
    cf.step("[4/7] Obtaining kernel")

    if kernel_source == "kei":
        kernel = find_kei_kernel(board)
        if kernel:
            cf.ok(f"kei kernel found: {kernel}")
            return kernel
        cf.fail(f"kei kernel not found for board '{board}'")
        cf.info("  Run: cd ../kei && python3 scripts/build.py " + board)
        cf.info("  Or use --kernel-source linux to build from source")
        return None

    # kernel_source == "linux"
    cf.info("  Building Linux kernel from source (placeholder)")
    cf.info("  TODO: download linux-6.12.tar.xz, cross-compile with defconfig")
    cf.info("  For now, kernel must be provided manually or via kei")
    return None


def assemble_rootfs(
    output_dir: Path,
    rust_target: str,
    evernight_bin: Path | None,
    board: str,
) -> Path:
    cf.step("[5/7] Assembling rootfs")
    rootfs = output_dir / "rootfs"
    bin_dir = rootfs / "usr" / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)

    # aris-core — check CARGO_TARGET_DIR (or /tmp/cargo-target) and project target/
    target_dir = Path(os.environ.get("CARGO_TARGET_DIR", ""))
    if not target_dir or not target_dir.is_absolute():
        target_dir = PROJECT_ROOT / "target"
    core_candidates = [
        target_dir / rust_target / "release" / "aris-core",
        PROJECT_ROOT / "target" / rust_target / "release" / "aris-core",
        Path("/tmp/cargo-target") / rust_target / "release" / "aris-core",
    ]
    core_bin = next((p for p in core_candidates if p.exists()), None)
    if core_bin:
        shutil.copy2(core_bin, bin_dir / "aris-core")
        cf.ok(f"aris-core → /usr/bin/aris-core (from {core_bin})")
    else:
        cf.warn("aris-core binary not found")
        cf.info(f"  Looked in: {[str(p) for p in core_candidates]}")

    # evernight
    if evernight_bin and evernight_bin.exists():
        shutil.copy2(evernight_bin, bin_dir / "evernight")
        cf.ok("evernight → /usr/bin/evernight")
    else:
        cf.warn("evernight binary not found")

    # overlay files (config, init scripts)
    overlay_dir = PROJECT_ROOT / "overlay" / board
    if overlay_dir.exists():
        shutil.copytree(overlay_dir, rootfs, dirs_exist_ok=True)
        cf.ok(f"Overlay ({board}) applied")

    # create essential system directories for a bootable rootfs
    for d in ["dev", "proc", "sys", "tmp", "run",
              "bin", "sbin", "lib", "usr/bin", "usr/sbin", "usr/lib",
              "mnt", "root",
              "var/run", "var/log", "var/lock",
              "etc/evernight", "etc/init.d", "data"]:
        (rootfs / d).mkdir(parents=True, exist_ok=True)

    cf.ok(f"Rootfs assembled: {rootfs}")
    return rootfs


def main() -> int:
    import argparse

    parser = argparse.ArgumentParser(description="Build aris firmware image")
    parser.add_argument("board", nargs="?", default="nanopi-r3s")
    parser.add_argument("--kernel-source", choices=["linux", "kei"],
                        default="kei",
                        help="Kernel source: 'kei' (from ../kei) or 'linux' (build from source)")
    parser.add_argument("--arch", default=None,
                        help="Override architecture (default: from board config)")
    parser.add_argument("--skip-evernight", action="store_true",
                        help="Skip evernight cross-compilation")
    args = parser.parse_args()

    board = args.board
    config = load_board_config(board)
    arch = args.arch or config.get("arch", "aarch64")
    rust_target = f"{arch}-unknown-linux-musl"
    features = config.get("evernight_features", ["hardware", "protocol",
                                                   "serial", "sensor", "bin",
                                                   "api", "manifest"])

    output_dir = PROJECT_ROOT / "target" / "output" / board
    output_dir.mkdir(parents=True, exist_ok=True)

    cf.section(f"aris build: {board}")
    cf.info(f"  Kernel source: {args.kernel_source}")
    cf.info(f"  Target: {rust_target}")

    # [1/7] Toolchain
    cf.blank()
    cf.step("[1/7] Setting up toolchain")
    subprocess.run(["rustup", "target", "add", rust_target],
                   check=False, capture_output=True)
    cf.ok(f"Rust target {rust_target} ready")

    # [2/7] aris-core
    cf.blank()
    if not build_aris_core(rust_target):
        return 1

    # [3/7] evernight
    cf.blank()
    evernight_bin = None
    if not args.skip_evernight:
        evernight_bin = build_evernight(rust_target, features)

    # [4/7] Kernel
    cf.blank()
    kernel = obtain_kernel(board, args.kernel_source, arch)
    if kernel:
        shutil.copy2(kernel, output_dir / "Image")
        cf.ok(f"Kernel → {output_dir / 'Image'}")

    # [5/7] rootfs
    cf.blank()
    rootfs = assemble_rootfs(output_dir, rust_target, evernight_bin, board)

    # [6/7] U-Boot
    cf.blank()
    cf.step("[6/7] Obtaining U-Boot")
    uboot_script = PROJECT_ROOT / "scripts" / "build_uboot.py"
    uboot_artifacts = None
    uboot_result = subprocess.run(
        [sys.executable, str(uboot_script), board],
        cwd=PROJECT_ROOT, capture_output=True, text=True,
    )
    if uboot_result.returncode == 0:
        uboot_artifacts = {}
        for name in ("idbloader.img", "u-boot.itb"):
            p = output_dir / name
            if p.exists():
                uboot_artifacts[name] = p
                cf.ok(f"  {name} → {p}")
    if not uboot_artifacts:
        cf.warn("  U-Boot not available (image will boot only if U-Boot is pre-installed)")

    # [7/7] Image assembly
    cf.blank()
    cf.step("[7/7] Assembling SD card image")
    dtb_path = output_dir / "board.dtb"
    image_script = PROJECT_ROOT / "scripts" / "build_image.py"
    image_result = subprocess.run(
        [sys.executable, str(image_script), board],
        cwd=PROJECT_ROOT,
    )

    cf.blank()
    cf.ok(f"Build artifacts: {output_dir}")
    cf.info(f"  rootfs:  {rootfs}")
    if kernel:
        cf.info(f"  kernel:  {output_dir / 'Image'}")
    sdcard = output_dir / "sdcard.img"
    if sdcard.exists():
        cf.info(f"  image:   {sdcard}")
    else:
        cf.info("  image:   (assembly failed — check output above)")
    return 0


if __name__ == "__main__":
    sys.exit(main())

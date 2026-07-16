#!/usr/bin/env python3
"""aris — build or fetch the HMI browser engine for a target board.

Implements the "Chromebook-style local HMI" rendering path. The gateway
device renders its own dashboard on an attached screen using a browser
engine, rather than only serving a web page to a host browser.

Engines are selected by ``configs/<board>.toml``::

    [display]
    engine = "blitz-vello" | "webkitgtk" | "servo" | "cef" | "none"

Acquisition strategy per engine:

  * **blitz-vello** (default) — The browser is built as part of
    ``aris-render`` (Blitz DOM + Vello CPU rasterization). Cross-compile
    via ``cargo zigbuild --target aarch64-unknown-linux-musl``. No
    external binary or system library needed; renders directly to
    ``/dev/fb0`` (virtio-gpu framebuffer).
  * **webkitgtk** — Install the prebuilt arm64 ``libwebkitgtk`` + ``cogs``
    (or ``epiphany``) packages from Debian/Ubuntu arm64 repos via QEMU user
    emulation. No source compilation. (Legacy; replaced by blitz-vello.)
  * **servo** — Cross-compile from the Servo source (Rust,
    ``aarch64-unknown-linux-gnu`` target). (Legacy.)
  * **cef** — No official prebuilt aarch64 Linux binary exists.

The resulting browser binary + its runtime libraries are placed in
``output/<board>/rootfs/usr/bin/`` and ``rootfs/usr/lib/`` for the rootfs
assembler to pick up.

Usage::

    python3 scripts/build_browser.py [board]
    python3 scripts/build_browser.py qemu-hmi
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
import build_env
import cli_format as cf

PROJECT_ROOT = Path(__file__).resolve().parent.parent


def load_board_config(board: str) -> dict:
    config_path = PROJECT_ROOT / "configs" / f"{board}.toml"
    if not config_path.exists():
        config_path = PROJECT_ROOT / "configs" / "default.toml"
    with config_path.open("rb") as f:
        return tomllib.load(f)


def build_webkitgtk(arch: str, rootfs: Path) -> bool:
    """Install prebuilt WebKitGTK + Cogs kiosk browser from arm64 repos.

    Uses apt download + dpkg extract (no installation into the host). The
    packages are fetched for the target arch and unpacked into rootfs.
    """
    if arch != "aarch64":
        cf.warn(f"  WebKitGTK prebuilt path only implemented for aarch64, got {arch}")
        cf.info("  For x86_64, install cogs/libwebkitgtk directly in the rootfs chroot")
        return False

    cf.step("[webkitgtk] 下载预编译 arm64 包")
    # Cogs is a minimal Wayland kiosk browser built on WebKitGTK.
    # libwebkitgtk-6.0 is the engine shared library.
    packages = [
        "cogs",
        "libwebkitgtk-6.0-4",
        "libglib2.0-0t64",
        "libgtk-4-1",
        "libwayland-server0",
        "libwayland-client0",
    ]
    tmp = rootfs / ".apt-cache"
    tmp.mkdir(parents=True, exist_ok=True)

    # Download each package for arm64 from Ubuntu ports.
    for pkg in packages:
        cf.pending(f"  下载 {pkg}:arm64 ...")
        r = subprocess.run(
            ["bash", "-c",
             f"cd {tmp} && apt-get download {pkg}:arm64 2>/dev/null"],
            capture_output=True, text=True,
        )
        if r.returncode != 0:
            cf.warn(f"    {pkg} 下载失败（可能包名不匹配此发行版）")

    # Extract all debs into rootfs.
    debs = list(tmp.glob("*.deb"))
    if not debs:
        cf.fail("  没有下载到任何 .deb 包")
        cf.info("  确保已运行 dpkg --add-architecture arm64 && apt-get update")
        return False

    cf.step("[webkitgtk] 解包到 rootfs")
    for deb in debs:
        subprocess.run(
            ["dpkg-deb", "-x", str(deb), str(rootfs)],
            check=False, capture_output=True,
        )
    shutil.rmtree(tmp, ignore_errors=True)

    # Verify cogs binary landed in the right place.
    cogs = rootfs / "usr" / "bin" / "cogs"
    if cogs.exists():
        cf.ok(f"  cogs → {cogs}")
        return True
    cf.warn("  cogs 二进制未在预期位置，检查包内容")
    return True  # libraries may still be useful even if cogs isn't present


def build_servo(arch: str, rootfs: Path) -> bool:
    """Cross-compile Servo for aarch64.

    No official prebuilt aarch64 Servo binary exists. This clones Servo
    and cross-compiles via Rust's aarch64-unknown-linux-gnu target.
    Takes ~30 min on a fast machine.
    """
    rust_target = {
        "aarch64": "aarch64-unknown-linux-gnu",
        "x86_64": "x86_64-unknown-linux-gnu",
    }.get(arch)
    if not rust_target:
        cf.fail(f"  Unsupported arch for Servo: {arch}")
        return False

    servo_src = PROJECT_ROOT.parent / "servo"
    if not (servo_src / "Cargo.toml").exists():
        cf.step("[servo] 克隆 Servo 源码")
        subprocess.run(
            ["git", "clone", "--depth=1",
             "https://github.com/servo/servo.git", str(servo_src)],
            check=False,
        )

    cf.step(f"[servo] 交叉编译 ({rust_target})")
    cf.info("  这大约需要 30 分钟（首次）...")
    env = {**os.environ, "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER": "aarch64-linux-gnu-gcc"}
    r = subprocess.run(
        ["cargo", "build", "--release", "--target", rust_target,
         "--bin", "servo"],
        cwd=servo_src, env=env,
    )
    if r.returncode != 0:
        cf.fail("  Servo 编译失败")
        return False

    binary = servo_src / "target" / rust_target / "release" / "servo"
    if binary.exists():
        shutil.copy2(binary, rootfs / "usr" / "bin" / "servo-browser")
        cf.ok("  servo-browser → /usr/bin/servo-browser")
        return True
    cf.fail("  Servo 二进制未找到")
    return False


def build_cef(arch: str, rootfs: Path) -> bool:
    """Bootstrap a CEF (Chromium Embedded Framework) build.

    WARNING: There is NO official prebuilt CEF binary for linuxarm64.
    Building from source takes 6-12 hours and ~100GB disk. This function
    only sets up the source tree and prints build instructions; the actual
    compile must be run separately (e.g. overnight in a container).

    For x86_64, prebuilt binaries ARE available from Spotify.
    """
    if arch == "aarch64":
        cf.fail("  CEF 没有 aarch64 预编译二进制")
        cf.info("  自行构建需要 6-12 小时 + ~100GB 磁盘空间")
        cf.info("  推荐改用 webkitgtk（预编译）或 servo（轻量）")
        cf.info("  若必须用 CEF，参考：")
        cf.info("    https://bitbucket.org/chromiumembedded/cef/wiki/BranchesAndBuilding.md")
        return False

    # x86_64: download prebuilt from Spotify.
    cf.step("[cef] 下载 x86_64 预编译（Spotify CEF builds）")
    cf.info("  从 https://cef-builds.spotifycdn.com/index.html 下载 cef_binary_*_linux64")
    cf.info("  本脚本暂不自动下载（版本需人工确认），请手动放置后重跑")
    return False


def build_blitz_vello(arch: str, rootfs: Path) -> bool:
    """Cross-compile aris kiosk binary (Blitz + Vello CPU, fbdev backend).

    Builds ``aris-render`` with the ``fbdev`` feature via cargo zigbuild.
    The resulting binary renders HTML/CSS directly to /dev/fb0 without
    requiring X11, Wayland, or any external browser engine.

    Prerequisites:
      - cargo-zigbuild installed (cargo install cargo-zigbuild)
      - zig installed and in PATH
    """
    rust_target = {
        "aarch64": "aarch64-unknown-linux-musl",
        "x86_64": "x86_64-unknown-linux-musl",
    }.get(arch)
    if not rust_target:
        cf.fail(f"  Unsupported arch for blitz-vello: {arch}")
        return False

    cf.step(f"[blitz-vello] 交叉编译 aris kiosk ({rust_target})")
    cf.info("  Blitz + Vello CPU → /dev/fb0 (fbdev backend)")

    r = subprocess.run(
        ["cargo", "zigbuild", "--target", rust_target, "--release",
         "-p", "aris-render", "--no-default-features",
         "--features", "fbdev",
         "--bin", "kei_fbtest"],
        cwd=PROJECT_ROOT,
    )
    if r.returncode != 0:
        cf.fail("  aris-render fbdev 编译失败")
        return False

    binary = PROJECT_ROOT / "target" / rust_target / "release" / "kei_fbtest"
    if binary.exists():
        dest = rootfs / "usr" / "bin" / "aris-kiosk"
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(binary, dest)
        cf.ok(f"  aris-kiosk → /usr/bin/aris-kiosk")
        return True

    cf.fail("  kei_fbtest 二进制未找到")
    return False


def generate_init_script(engine: str, kiosk_url: str, extra_args: str, rootfs: Path) -> None:
    """Write the /etc/init.d/S99hmi script that launches the kiosk browser."""
    init_dir = rootfs / "etc" / "init.d"
    init_dir.mkdir(parents=True, exist_ok=True)
    init_script = init_dir / "S99hmi"

    binary_map = {
        "blitz-vello": "/usr/bin/aris-kiosk",
        "webkitgtk": "/usr/bin/cogs",
        "servo": "/usr/bin/servo-browser",
        "cef": "/usr/bin/cefsimple",
    }
    binary = binary_map.get(engine, "")
    if not binary:
        return

    if engine == "blitz-vello":
        cmd = f'{binary} --url={kiosk_url}'
    elif engine == "webkitgtk":
        cmd = f'{binary} --url={kiosk_url} {extra_args}'
    elif engine == "servo":
        cmd = f'{binary} {kiosk_url} {extra_args}'
    else:
        cmd = f'{binary} {extra_args}'

    script = f"""#!/bin/sh
# HMI kiosk browser launcher — auto-generated by aris build_browser.py
case "$1" in
  start)
    echo "Starting HMI kiosk ({engine})..."
    # Wait for the evernight HTTP server to be ready.
    for i in $(seq 1 30); do
      if wget -q -O /dev/null {kiosk_url} 2>/dev/null; then break; fi
      sleep 1
    done
    {cmd} &
    ;;
  stop)
    killall $(basename {binary}) 2>/dev/null
    ;;
esac
"""
    init_script.write_text(script)
    init_script.chmod(0o755)
    cf.ok(f"  init 脚本 → {init_script}")


def main() -> int:
    if build_env.wsl_main_guard():
        return 0
    import argparse

    parser = argparse.ArgumentParser(description="Build/fetch HMI browser engine")
    parser.add_argument("board", nargs="?", default="nanopi-r3s")
    args = parser.parse_args()

    config = load_board_config(args.board)
    display_cfg = config.get("display", {})
    engine = display_cfg.get("engine", "none")
    arch = config.get("arch", "aarch64")

    cf.section(f"aris browser engine: {engine} ({arch})")

    if engine == "none":
        cf.ok("  headless 模式（engine=none），跳过浏览器构建")
        return 0

    output_dir = PROJECT_ROOT / "target" / "output" / args.board
    rootfs = output_dir / "rootfs"
    (rootfs / "usr" / "bin").mkdir(parents=True, exist_ok=True)

    builders = {
        "blitz-vello": build_blitz_vello,
        "webkitgtk": build_webkitgtk,
        "servo": build_servo,
        "cef": build_cef,
    }
    builder = builders.get(engine)
    if not builder:
        cf.fail(f"  未知引擎: {engine}")
        return 1

    ok = builder(arch, rootfs)
    if not ok:
        cf.fail(f"  {engine} 构建失败")
        return 1

    # Generate the init script that launches the browser on boot.
    generate_init_script(
        engine,
        display_cfg.get("kiosk_url", "http://127.0.0.1:8080/"),
        display_cfg.get("extra_args", ""),
        rootfs,
    )

    cf.blank()
    cf.ok(f"浏览器引擎 {engine} 就绪，rootfs: {rootfs}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

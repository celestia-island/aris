#!/usr/bin/env python3
"""aris — inspect the build environment and report what's usable.

Prints a table of detected host kind, WSL2 distros (on Windows), the
selected build distro, and the container CLI that build_image.py /
build_uboot.py / e2e_qemu_ignition.sh will use.

On Windows this is the best pre-flight check before running ``just build``
or ``just qemu-ignition-*``: it tells you which distro will be picked,
whether docker or podman is the container backend, and what's missing.

Usage:
    python3 scripts/check_env.py
"""
from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent / "utils"))
import build_env
import cli_format as cf


def main() -> int:
    cf.section("aris — 构建环境检查")

    kind = build_env.detect_host_kind()
    cf.info(f"  宿主类型：{kind}")
    cf.info(f"  宿主架构：{build_env.host_machine()}")

    if kind == "windows":
        cf.blank()
        cf.step("扫描 WSL2 发行版")
        distros = build_env.list_wsl_distros()
        if not distros:
            cf.fail("未检测到任何 WSL2 发行版")
            cf.info("  请先安装：wsl --install -d Ubuntu-24.04")
            return 1
        for d in distros:
            cf.info(f"  - {d['name']:<28} state={d['state']:<10} v{d['version']}")

        cf.blank()
        cf.step("探测每个发行版的构建工具")
        for d in distros:
            tools = build_env._probe_distro_tools(d["name"])
            score = int(tools.get("__score__", "0"))
            cf.info(f"  {d['name']}:")
            cf.info(f"      评分：{score}")
            cf.info(f"      工具：{build_env._summarise_tools(tools)}")

        cf.blank()
        cf.step("容器 CLI 选择")
        # After select_distro the re-exec would land here; for a pure check
        # we don't re-exec, just report what the WSL side would resolve.
        sel = build_env.select_distro()
        if sel is None:
            cf.fail("没有可用的构建环境")
            return 1
        distro, tools = sel
        cf.ok(f"已选发行版：{distro}")
        cf.info(f"  容器后端：{build_env._summarise_container(tools)}")
        if not tools.get("__docker_alive__") and tools.get("podman") \
                and not tools.get("__podman_alive__"):
            cf.warn("podman socket 未启动；首次容器调用时会尝试拉起")
            cf.info("  或手动：systemctl --user start podman.socket")
    else:
        import shutil
        cf.blank()
        cf.step("原生 Linux/macOS 环境")
        cmd = build_env.docker_cmd()
        cf.ok(f"容器 CLI：{' '.join(cmd)}")
        cf.info(f"  docker 二进制：{shutil.which('docker') or '未安装'}")
        cf.info(f"  podman 二进制：{shutil.which('podman') or '未安装'}")
        cf.info(f"  qemu-system-aarch64：{shutil.which('qemu-system-aarch64') or '未安装'}")

    cf.blank()
    cf.ok("检查完成")
    return 0


def shutil_which(name: str) -> str | None:
    import shutil
    return shutil.which(name)


if __name__ == "__main__":
    sys.exit(main())

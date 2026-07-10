#!/usr/bin/env python3
"""aris — enter development shell with cross-compilation environment.

Usage:
    python3 scripts/dev_shell.py [command...]
    python3 scripts/dev_shell.py
    python3 scripts/dev_shell.py -- bash -c 'echo $ARCH'
"""
from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent / "utils"))
import cli_format as cf

PROJECT_ROOT = Path(__file__).resolve().parent.parent


def main() -> int:
    env = os.environ.copy()
    env["PATH"] = f"{PROJECT_ROOT}/tools/bin:{env.get('PATH', '')}"
    env["CROSS_COMPILE"] = "aarch64-linux-musl-"
    env["ARCH"] = "arm64"

    cf.section("aris dev shell")
    cf.info(f"  ARCH={env['ARCH']}")
    cf.info(f"  CROSS_COMPILE={env['CROSS_COMPILE']}")
    cf.blank()

    cmd = sys.argv[1:] if len(sys.argv) > 1 else [os.environ.get("SHELL", "bash")]
    result = subprocess.run(cmd, env=env)
    return result.returncode


if __name__ == "__main__":
    sys.exit(main())

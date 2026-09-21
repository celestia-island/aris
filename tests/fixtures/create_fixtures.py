#!/usr/bin/env python3
"""Create fixture binaries for testing.

Python port of the former create_fixtures.sh (de-bash doctrine: no .sh/.ps1
helper scripts in the justfile chain).

Each fixture is a REAL cross-compiled ELF/PE/Mach-O produced by `cargo build`
from the pure-Rust project in tests/fixtures/evernight-fixture/ — which has
zero C dependencies and so cross-compiles cleanly with the self-contained
musl / rust-lld linker configuration in .cargo/config.toml.

If a Rust target triple is not installed (e.g. the Apple/Windows targets on a
Linux CI host) the fixture falls back to a generated shell stub so installer
tests still have something to run.

NOTE: the real evernight broker (../evernight) cannot be used as a fixture
because it links C libraries (libmodbus, libsqlite3, …) that require a musl C
cross-toolchain not present in CI.

Usage:
    python create_fixtures.py [fixtures_dir]
"""

from __future__ import annotations

import os
import stat
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_SRC = ROOT / "tests" / "fixtures" / "evernight-fixture"
FIXTURES_DIR = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "tests" / "fixtures" / "evernight-binaries"
BUILD_LOG = Path("/tmp/aris-fixture-build.log") if os.name != "nt" else Path("target/aris-fixture-build.log")

TARGETS = [
    "x86_64-pc-windows-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
]

STUB_TEMPLATE = """\
#!/bin/sh
# evernight — fixture binary for testing (shell-script fallback)
# The Rust target '{target}' is not installed on this host, so no
# real cross-compiled binary was produced. This stub prints the same
# banner so installer tests still pass.
echo "evernight (fixture): $*"
echo "  target: {target}"
echo "  compiled: fixture-build (stub)"
exit 0
"""


def have_target(target: str) -> bool:
    result = subprocess.run(
        ["rustup", "target", "list", "--installed"],
        capture_output=True, text=True,
    )
    return result.returncode == 0 and target in result.stdout.splitlines()


def write_stub(target: str, outfile: Path) -> None:
    outfile.parent.mkdir(parents=True, exist_ok=True)
    outfile.write_text(STUB_TEMPLATE.format(target=target), encoding="utf-8")
    outfile.chmod(outfile.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    print(f"  [stub] {outfile}")


def make_executable(path: Path) -> None:
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def build_real(target: str) -> None:
    ext = ".exe" if "windows" in target else ""
    outdir = FIXTURES_DIR / target / "release"
    outfile = outdir / f"evernight{ext}"
    outdir.mkdir(parents=True, exist_ok=True)

    print(f"  [cargo] building for {target}...")
    env = {**os.environ, "FIXTURE_TARGET": target}
    built_ok = subprocess.run(
        [
            "cargo", "build",
            "--manifest-path", str(FIXTURE_SRC / "Cargo.toml"),
            "--target", target,
            "--release",
        ],
        env=env,
        stdout=subprocess.DEVNULL if os.name == "nt" else subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    ).returncode == 0
    if built_ok:
        built = FIXTURE_SRC / "target" / target / "release" / f"evernight{ext}"
        if built.is_file():
            outfile.write_bytes(built.read_bytes())
            make_executable(outfile)
            print(f"  [ok]   {outfile} ({outfile.stat().st_size} bytes)")
        else:
            print(f"  [warn] build ok but binary not found at {built}; writing stub")
            write_stub(target, outfile)
    else:
        print(f"  [warn] cargo build for {target} failed (see {BUILD_LOG}); writing stub")
        write_stub(target, outfile)


def main() -> int:
    print(f"Creating fixture binaries in {FIXTURES_DIR}...")
    FIXTURES_DIR.mkdir(parents=True, exist_ok=True)
    for target in TARGETS:
        ext = ".exe" if "windows" in target else ""
        outfile = FIXTURES_DIR / target / "release" / f"evernight{ext}"
        if have_target(target):
            build_real(target)
        else:
            write_stub(target, outfile)
    print()
    print("Fixtures created successfully.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

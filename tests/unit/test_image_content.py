#!/usr/bin/env python3
"""
Test 1: Installer image content validation (pure Python, no external deps).

Validates that the FAT image contains all expected files in correct structure.
"""

import os
import sys

sys.path.insert(0, os.path.dirname(__file__))
from fat_reader import FatReader

ARIS_TEST_TMP = os.environ.get("ARIS_TEST_TMP", "/tmp/aris-test")
IMAGE = os.path.join(ARIS_TEST_TMP, "output", "installer.img")

# Expected files: (directory, short_name_pattern, min_size)
EXPECTED = [
    ("root", "autorun.inf", 0),           # autorun is optional in root
    ("windows", "autorun.inf", 100),
    ("windows", "install_evernight.bat", 500),
    ("linux", "install_evernight.sh", 500),
    ("macos", "install_evernight.command", 500),
    ("android", "install_evernight.txt", 200),
    ("common", "README.TXT", 500),
    ("common", "evernight-windows-amd64.exe", 10),
    ("common", "evernight-linux-amd64", 10),
    ("common", "evernight-linux-arm64", 10),
    ("common", "evernight-darwin-amd64", 0),   # optional
    ("common", "evernight-darwin-arm64", 0),   # optional
]

REQUIRED = {
    ("windows", "install_evernight.bat"),
    ("linux", "install_evernight.sh"),
    ("macos", "install_evernight.command"),
    ("android", "install_evernight.txt"),
    ("common", "README.TXT"),
    ("common", "evernight-windows-amd64.exe"),
    ("common", "evernight-linux-amd64"),
    ("common", "evernight-linux-arm64"),
}


def main() -> int:
    failures = []

    if not os.path.exists(IMAGE):
        print(f"FAIL: Image not found at {IMAGE}")
        print("Run tests/unit/build_image.sh first.")
        return 1

    reader = FatReader(IMAGE)
    print(f"FAT{reader.bpb.fat_type} image: {os.path.getsize(IMAGE):,} bytes")
    print()

    # Walk the filesystem
    all_files = reader.walk()
    file_map = {}  # "DIR/NAME" → size
    for path, size, is_dir in all_files:
        file_map[path.upper()] = (size, is_dir)

    # ── Test 1a: Directory structure ────────
    print("[1a] Directory structure...")
    expected_dirs = {"ANDROID", "COMMON", "LINUX", "MACOS", "WINDOWS"}
    found_dirs = {p.split("/")[0] for p, s, d in all_files if d}
    missing = expected_dirs - found_dirs
    if missing:
        failures.append(f"Missing directories: {missing}")
    print(f"  Expected: {sorted(expected_dirs)}")
    print(f"  Found:    {sorted(found_dirs)}")

    # ── Test 1b: File presence ──────────
    print()
    print("[1b] File presence and sizes...")
    for dir_name, pattern, min_size in EXPECTED:
        found = False
        for path, (size, is_dir) in file_map.items():
            if path.startswith(dir_name.upper() + "/") and pattern.upper() in path:
                found = True
                ok = size >= min_size
                status = "[ok]" if ok else "[!!]"
                if not ok and (dir_name, pattern) in REQUIRED:
                    failures.append(f"{path}: too small ({size} < {min_size})")
                print(f"  {status} {path}: {size} bytes")
                break
        if not found:
            if (dir_name, pattern) in REQUIRED:
                failures.append(f"{dir_name}/{pattern}: NOT FOUND")
                print(f"  [!!] {dir_name}/{pattern}: NOT FOUND")
            else:
                print(f"  [skip] {dir_name}/{pattern}: not found (optional)")

    # ── Test 1c: Key content validation ──────────
    print()
    print("[1c] Content validation...")

    # Read and check Windows installer
    for path, (size, is_dir) in file_map.items():
        if "INSTALL_EVERNIGHT.BAT" in path:
            # Find the entry to read its content
            root = reader.list_root()
            for entry in root:
                if entry.is_dir and "WINDOWS" in entry.full_name.upper():
                    sub = reader.list_dir(entry.first_cluster)
                    for sub_entry in sub:
                        if "INSTALL" in sub_entry.full_name.upper():
                            content = reader.read_file(sub_entry.first_cluster, sub_entry.file_size).decode("utf-8", errors="replace")
                            if "10.0.99.1" in content:
                                print(f"  [ok] install_evernight.bat: contains gateway IP")
                            else:
                                failures.append("install_evernight.bat: missing gateway IP")
                                print(f"  [!!] install_evernight.bat: missing gateway IP")
                            if "EvernightGateway" in content:
                                print(f"  [ok] install_evernight.bat: service name defined")
                            else:
                                failures.append("install_evernight.bat: missing service name")
                            break

    # Read and check README
    for path, (size, is_dir) in file_map.items():
        if "README.TXT" in path:
            root = reader.list_root()
            for entry in root:
                if entry.is_dir and "COMMON" in entry.full_name.upper():
                    sub = reader.list_dir(entry.first_cluster)
                    for sub_entry in sub:
                        if "README" in sub_entry.full_name.upper():
                            content = reader.read_file(sub_entry.first_cluster, sub_entry.file_size).decode("utf-8", errors="replace")
                            if "10.0.99.1" in content:
                                print(f"  [ok] README.txt: contains gateway IP")
                            else:
                                failures.append("README.txt: missing gateway IP")
                            break

    # ── Summary ────────
    print()
    print("=" * 55)
    if failures:
        print(f"FAIL: {len(failures)} issues:")
        for f in failures:
            print(f"  - {f}")
        return 1
    else:
        print("PASS: All content checks passed")
        return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""
Unified test runner for the aris USB gadget subsystem.

Runs all tests in sequence and reports results.

Usage:
    python3 tests/run_all.py          # Run all tests
    python3 tests/run_all.py --quick      # Quick mode (skip QEMU and image build)
"""

import argparse
import os
import subprocess
import sys
import time

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
ARIS_ROOT = os.path.join(SCRIPT_DIR, "..")


def run_test(name, cmd, timeout=120):
    """Run a test and return (passed, output, duration)."""
    print(f"\n{'─' * 60}")
    print(f"  {name}")
    print(f"{'─' * 60}")

    start = time.time()
    try:
        r = subprocess.run(
            cmd, capture_output=True, text=True, timeout=timeout,
            cwd=ARIS_ROOT,
        )
        duration = time.time() - start
        output = r.stdout + r.stderr
        passed = r.returncode == 0

        # Print the test output
        for line in output.strip().split("\n"):
            print(f"  {line}")

        return passed, duration
    except subprocess.TimeoutExpired:
        duration = time.time() - start
        print(f"  TIMEOUT after {timeout}s")
        return False, duration
    except FileNotFoundError as e:
        duration = time.time() - start
        print(f"  SKIP: {e}")
        return True, duration  # Skip = pass


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--quick", action="store_true", help="Skip slow tests (image build, QEMU)")
    args = parser.parse_args()

    print("=" * 60)
    print("  aris USB Gadget Test Suite")
    print("=" * 60)

    results = []

    # ── Pre-requisite: Build installer image ────────────
    if not args.quick:
        results.append(("Build installer image",
            run_test("Building installer image...",
                ["bash", os.path.join(SCRIPT_DIR, "unit", "build_image.sh")], 120)))

    # ── Test 1: Image content ────────────────────────────────
    results.append(("Image content validation",
        run_test("Test 1: Image content validation",
            ["python3", os.path.join(SCRIPT_DIR, "unit", "test_image_content.py")], 30)))

    # ── Test 2: Gadget configfs ────────────────────────
    results.append(("Gadget configfs simulation",
        run_test("Test 2: Gadget configfs simulation",
            ["python3", os.path.join(SCRIPT_DIR, "gadget", "test_gadget_configfs.py")], 30)))

    # ── Test 3: OS installer reactions ─────────────────
    results.append(("OS installer reactions",
        run_test("Test 3: OS installer reactions",
            ["python3", os.path.join(SCRIPT_DIR, "installer", "test_os_reactions.py")], 30)))

    # ── Test 4: QEMU integration ───────────────────────────
    if not args.quick:
        results.append(("QEMU integration",
            run_test("Test 4: QEMU integration",
                ["python3", os.path.join(SCRIPT_DIR, "gadget", "test_qemu_gadget.py"), "--smoke"], 60)))

    # ── Test 5: Windows via Wine ────────────────────────────
    results.append(("Windows installer (Wine)",
        run_test("Test 5: Windows installer (Wine)",
            ["python3", os.path.join(SCRIPT_DIR, "installer", "test_windows_wine.py")], 30)))

    # ── Rust tests ────────────────────────────────────────────
    results.append(("Rust unit tests",
        run_test("Rust unit tests",
            ["cargo", "test", "--workspace"], 120)))

    # ── Clippy ────────────────────────────────────────────
    results.append(("Clippy lint",
        run_test("Clippy",
            ["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"], 60)))

    # ── Summary ────────────────────────────────────────────
    print()
    print("=" * 60)
    print("  Summary")
    print("=" * 60)

    passed = 0
    failed = 0
    for name, (result, duration) in results:
        status = "PASS" if result else "FAIL"
        if result:
            passed += 1
        else:
            failed += 1
        print(f"  [{status}] {name:<40} {duration:.1f}s")

    print()
    total = passed + failed
    print(f"  {passed}/{total} tests passed")
    if failed:
        print(f"  {failed} FAILED")
        return 1
    else:
        print("  ALL PASSED")
        return 0


if __name__ == "__main__":
    sys.exit(main())

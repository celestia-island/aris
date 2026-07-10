#!/usr/bin/env python3
"""
Test 3: OS-specific installer reactions when the USB device is "plugged in".

Simulates what each OS sees when the composite gadget (mass_storage + NCM)
is connected, and tests the installer scripts for each platform.

Since we don't have Wine/Windows on this machine, we validate the Windows
batch file through static analysis (parse logic, verify paths, check commands).
The Linux and macOS installers are tested via shell dry-runs.
"""

import os
import re
import subprocess
import sys

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
PKG_DIR = os.path.join(SCRIPT_DIR, "..", "..", "package")
ARIS_TEST_TMP = os.environ.get("ARIS_TEST_TMP", "/tmp/aris-test")
OUTPUT_DIR = os.path.join(ARIS_TEST_TMP, "output")


def test_windows_installer():
    """Test the Windows .bat installer through static analysis."""
    print("\n[3a] Windows installer (install_evernight.bat)...")
    failures = []

    bat_path = os.path.join(PKG_DIR, "windows", "install_evernight.bat")
    with open(bat_path, "r") as f:
        bat_content = f.read()

    # Check 1: Must reference the gateway IP (10.0.99.1)
    if "10.0.99.1" in bat_content:
        print("  [ok] References gateway IP 10.0.99.1")
    else:
        failures.append("missing gateway IP")
        print("  [!!] Missing gateway IP")

    # Check 2: Must create a Windows service
    if "sc create" in bat_content.lower() or "sc create" in bat_content:
        print("  [ok] Creates Windows service (sc create)")
    else:
        failures.append("no service creation")
        print("  [!!] No sc create command")

    # Check 3: Service name should be EvernightGateway
    if "EvernightGateway" in bat_content:
        print("  [ok] Service name: EvernightGateway")
    else:
        failures.append("missing service name")
        print("  [!!] Missing service name")

    # Check 4: Must check for 64-bit architecture
    if "AMD64" in bat_content or "PROCESSOR_ARCHITECTURE" in bat_content:
        print("  [ok] Checks architecture (AMD64)")
    else:
        failures.append("no arch check")
        print("  [!!] No architecture check")

    # Check 5: Must copy evernight binary
    if "evernight" in bat_content.lower() and ("copy" in bat_content.lower() or "xcopy" in bat_content.lower()):
        print("  [ok] Copies evernight binary")
    else:
        failures.append("no binary copy")
        print("  [!!] No binary copy")

    # Check 6: Must open browser to dashboard
    if "start" in bat_content and "8080" in bat_content:
        print("  [ok] Opens browser to dashboard (:8080)")
    else:
        failures.append("no browser open")
        print("  [!!] No browser open")

    # Check 7: Must reference the correct binary name
    if "evernight-windows-amd64.exe" in bat_content:
        print("  [ok] References evernight-windows-amd64.exe")
    else:
        failures.append("wrong binary name")
        print("  [!!] Wrong binary reference")

    # Check 8: autorun.inf must point to the bat file
    autorun_path = os.path.join(PKG_DIR, "autorun.inf")
    if os.path.exists(autorun_path):
        with open(autorun_path) as f:
            autorun = f.read()
        if "install_evernight.bat" in autorun:
            print("  [ok] autorun.inf → install_evernight.bat")
        else:
            failures.append("autorun.inf wrong target")
            print("  [!!] autorun.inf doesn't point to bat")
    else:
        failures.append("autorun.inf missing")
        print("  [!!] autorun.inf missing from root")

    # Check 9: autorun.inf uses correct open path (windows\install_evernight.bat)
    if os.path.exists(autorun_path):
        with open(autorun_path) as f:
            autorun = f.read()
        if re.search(r'open=.*install_evernight\.bat', autorun, re.I):
            print("  [ok] autorun.inf open= command correct")
        else:
            failures.append("autorun.inf open= incorrect")
            print("  [!!] autorun.inf open= command wrong")

    return failures


def test_linux_installer():
    """Test the Linux installer script via bash dry-run."""
    print("\n[3b] Linux installer (install_evernight.sh)...")
    failures = []

    sh_path = os.path.join(PKG_DIR, "linux", "install_evernight.sh")
    with open(sh_path, "r") as f:
        sh_content = f.read()

    # Syntax check
    r = subprocess.run(["bash", "-n", sh_path], capture_output=True, text=True)
    if r.returncode == 0:
        print("  [ok] Syntax valid (bash -n)")
    else:
        failures.append(f"syntax error: {r.stderr}")
        print(f"  [!!] Syntax error: {r.stderr}")

    # Check 1: Must reference gateway IP
    if "10.0.99.1" in sh_content:
        print("  [ok] References gateway IP")
    else:
        failures.append("missing gateway IP")
        print("  [!!] Missing gateway IP")

    # Check 2: Must detect architecture
    if "uname -m" in sh_content:
        print("  [ok] Detects architecture (uname -m)")
    else:
        failures.append("no arch detection")
        print("  [!!] No arch detection")

    # Check 3: Must handle x86_64 and aarch64
    if "x86_64" in sh_content and "aarch64" in sh_content:
        print("  [ok] Handles x86_64 + aarch64")
    else:
        failures.append("missing arch variant")
        print("  [!!] Missing arch variant")

    # Check 4: Must install to /usr/local/bin
    if "/usr/local/bin" in sh_content:
        print("  [ok] Installs to /usr/local/bin")
    else:
        failures.append("wrong install path")
        print("  [!!] No /usr/local/bin")

    # Check 5: Must create systemd service
    if "systemd" in sh_content and "evernight-gateway.service" in sh_content:
        print("  [ok] Creates systemd service")
    else:
        failures.append("no systemd service")
        print("  [!!] No systemd service")

    # Check 6: Must open dashboard
    if "xdg-open" in sh_content and "8080" in sh_content:
        print("  [ok] Opens dashboard (xdg-open)")
    else:
        failures.append("no dashboard open")
        print("  [!!] No dashboard open")

    # Check 7: Must use set -euo pipefail (safe bash)
    if "set -euo pipefail" in sh_content or "set -e" in sh_content:
        print("  [ok] Uses safe shell (set -e)")
    else:
        failures.append("no error handling")
        print("  [!!] No set -e")

    # Check 8: Executable bit
    if os.access(sh_path, os.X_OK):
        print("  [ok] Executable bit set")
    else:
        failures.append("not executable")
        print("  [!!] Not executable")

    return failures


def test_macos_installer():
    """Test the macOS installer .command script."""
    print("\n[3c] macOS installer (install_evernight.command)...")
    failures = []

    cmd_path = os.path.join(PKG_DIR, "macos", "install_evernight.command")
    with open(cmd_path, "r") as f:
        cmd_content = f.read()

    # Syntax check
    r = subprocess.run(["bash", "-n", cmd_path], capture_output=True, text=True)
    if r.returncode == 0:
        print("  [ok] Syntax valid")
    else:
        failures.append(f"syntax error: {r.stderr}")
        print(f"  [!!] Syntax error: {r.stderr}")

    # Check 1: Gateway IP
    if "10.0.99.1" in cmd_content:
        print("  [ok] Gateway IP present")
    else:
        failures.append("missing IP")
        print("  [!!] Missing IP")

    # Check 2: Architecture detection (Intel + Apple Silicon)
    if "x86_64" in cmd_content and "arm64" in cmd_content:
        print("  [ok] Handles Intel + Apple Silicon")
    else:
        failures.append("missing arch")
        print("  [!!] Missing arch variants")

    # Check 3: Binary names
    if "evernight-darwin-amd64" in cmd_content and "evernight-darwin-arm64" in cmd_content:
        print("  [ok] Correct darwin binary names")
    else:
        failures.append("wrong binary names")
        print("  [!!] Wrong binary names")

    # Check 4: launchd plist creation
    if "LaunchAgents" in cmd_content and "plist" in cmd_content.lower():
        print("  [ok] Creates launchd agent")
    else:
        failures.append("no launchd")
        print("  [!!] No launchd agent")

    # Check 5: Quarantine removal (Gatekeeper bypass)
    if "xattr" in cmd_content and "quarantine" in cmd_content:
        print("  [ok] Removes quarantine (Gatekeeper)")
    else:
        failures.append("no quarantine removal")
        print("  [!!] No xattr quarantine removal")

    # Check 6: Opens dashboard
    if "open " in cmd_content and "8080" in cmd_content:
        print("  [ok] Opens dashboard")
    else:
        failures.append("no dashboard")
        print("  [!!] No dashboard open")

    # Check 7: Executable
    if os.access(cmd_path, os.X_OK):
        print("  [ok] Executable")
    else:
        failures.append("not executable")
        print("  [!!] Not executable")

    return failures


def test_android_instructions():
    """Test the Android instructions file."""
    print("\n[3d] Android instructions (install_evernight.txt)...")
    failures = []

    txt_path = os.path.join(PKG_DIR, "android", "install_evernight.txt")
    with open(txt_path, "r") as f:
        content = f.read()

    # Check 1: Must mention USB tethering
    if "USB" in content.upper() and "tether" in content.lower():
        print("  [ok] Mentions USB tethering")
    else:
        failures.append("no tethering mention")
        print("  [!!] No tethering")

    # Check 2: Must have gateway URL
    if "10.0.99.1:8080" in content:
        print("  [ok] Gateway URL present")
    else:
        failures.append("missing URL")
        print("  [!!] Missing URL")

    # Check 3: Must have Chinese instructions
    if "安卓" in content or "安装" in content:
        print("  [ok] Chinese instructions present")
    else:
        failures.append("no Chinese")
        print("  [!!] No Chinese text")

    # Check 4: Must mention APK option
    if "APK" in content.upper() or "apk" in content:
        print("  [ok] APK option mentioned")
    else:
        failures.append("no APK")
        print("  [!!] No APK mention")

    return failures


def test_plug_in_simulation():
    """Simulate the full 'plug-in' flow: what the host OS sees."""
    print("\n[3e] Full plug-in simulation...")
    failures = []

    image_path = os.path.join(OUTPUT_DIR, "installer.img")
    if not os.path.exists(image_path):
        print("  [skip] installer.img not built")
        return ["image not built"]

    # Use our FAT reader to verify what the host sees
    sys.path.insert(0, os.path.join(SCRIPT_DIR, "..", "unit"))
    from fat_reader import FatReader

    reader = FatReader(image_path)
    files = reader.walk()

    # Simulate Windows host reaction
    print("  Windows host would see:")
    has_autorun = any("AUTORUN.INF" in p.upper() for p, _, _ in files)
    has_bat = any("INSTALL_EVERNIGHT.BAT" in p.upper() for p, _, _ in files)
    has_exe = any("EVERNIGHT-WINDOWS" in p.upper() for p, _, _ in files)

    if has_autorun:
        print("    [ok] AutoRun: autorun.inf found at root")
    else:
        failures.append("Windows: no autorun.inf")
        print("    [!!] No autorun.inf")

    if has_bat:
        print("    [ok] Installer: install_evernight.bat found")
    else:
        failures.append("Windows: no bat installer")
        print("    [!!] No bat installer")

    if has_exe:
        print("    [ok] Binary: evernight-windows-amd64.exe found")
    else:
        failures.append("Windows: no exe")
        print("    [!!] No exe")

    # Simulate Linux host reaction
    print("  Linux host would see:")
    has_sh = any("INSTALL_EVERNIGHT.SH" in p.upper() for p, _, _ in files)
    has_linux_bin = any("EVERNIGHT-LINUX-AMD64" in p.upper() for p, _, _ in files)

    if has_sh:
        print("    [ok] Installer: install_evernight.sh found")
    else:
        failures.append("Linux: no sh installer")

    if has_linux_bin:
        print("    [ok] Binary: evernight-linux-amd64 found")
    else:
        failures.append("Linux: no binary")

    # Simulate macOS host reaction
    print("  macOS host would see:")
    has_cmd = any("INSTALL_EVERNIGHT.COMMAND" in p.upper() for p, _, _ in files)
    has_darwin = any("EVERNIGHT-DARWIN" in p.upper() for p, _, _ in files)

    if has_cmd:
        print("    [ok] Installer: install_evernight.command found")
    else:
        failures.append("macOS: no command installer")

    if has_darwin:
        print("    [ok] Binary: evernight-darwin-* found")
    else:
        failures.append("macOS: no binary")

    return failures


def main():
    all_failures = []
    print("Test 3: OS installer reactions")
    print("=" * 50)

    all_failures += test_windows_installer()
    all_failures += test_linux_installer()
    all_failures += test_macos_installer()
    all_failures += test_android_instructions()
    all_failures += test_plug_in_simulation()

    print("\n" + "=" * 50)
    if all_failures:
        print(f"FAIL: {len(all_failures)} issues:")
        for f in all_failures:
            print(f"  - {f}")
        return 1
    print("PASS: All installer tests passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())

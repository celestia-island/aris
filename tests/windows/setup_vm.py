#!/usr/bin/env python3
"""
QEMU Windows virtual machine test harness.

Provides a Windows VM for testing USB gadget detection, AutoRun,
batch file installers, and service registration. Unlike Wine,
this gives you a real Windows environment with full USB stack.

ISO ACQUISITION:
=================
Windows 11 Enterprise Evaluation ISO can be downloaded automatically
by reverse-engineering Microsoft's eval center download flow:

  1. GET eval center page → extract fwlink redirect URLs
  2. Follow the fwlink → get the real ISO URL from Microsoft's CDN
  3. Download the ISO (~6.6 GB)

This is the same approach used by dockurr/windows. No manual clicking
required. The ISO is a free 90-day evaluation license.

  just windows-setup --auto-download    # Automatic (Win11 x64 en-us)
  just windows-setup                    # Manual (point to your own ISO)

Architecture:
  ┌──────────────────┐     ┌─────────────────┐     ┌──────────────────┐
  │  Windows QEMU VM │────▶│  virtual USB    │────▶│  installer.img   │
  │  (user's ISO)    │     │  mass-storage   │     │  (FAT16 image)    │
  └──────────────────┘     └─────────────────┘     └──────────────────┘

Requirements:
  - qemu-system-x86_64 (installed: ✅)
  - ~30GB free disk for Windows VM image
  - Windows ISO (manual download, one-time)
  - genisoimage (installed: ✅) for creating test USB ISOs
  - No KVM required (works with software emulation, ~2-4x slower)

Usage:
  # One-time setup
  just windows-setup          # Create VM disk + check ISO readiness

  # Testing
  just windows-test           # Boot + attach installer.img as USB
  just windows-interactive    # Boot interactively (VNC localhost:5900)
  just windows-status         # Check readiness
"""

import argparse
import os
import shutil
import subprocess
import sys
import time

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
ARIS_ROOT = os.path.join(SCRIPT_DIR, "..", "..")
ARIS_TEST_TMP = os.environ.get("ARIS_TEST_TMP", "/tmp/aris-test")
CACHE_DIR = os.path.join(ARIS_TEST_TMP, "qemu-windows")
INSTALLER_IMG = os.path.join(ARIS_TEST_TMP, "output", "installer.img")

QEMU_BIN = "qemu-system-x86_64"
WIN_ISO = os.path.join(CACHE_DIR, "Win11_Eval.iso")
VM_DISK = os.path.join(CACHE_DIR, "windows.qcow2")
VM_RAM_MB = 4096
VM_CPU_CORES = 2
VM_DISK_SIZE = "30G"
VNC_PORT = 5900

# Microsoft eval center URLs
EVAL_WIN11_URL = "https://www.microsoft.com/en-us/evalcenter/download-windows-11-enterprise"
EVAL_WIN10_URL = "https://www.microsoft.com/en-us/evalcenter/download-windows-10-enterprise"
FW_LINK_BASE = "https://go.microsoft.com/fwlink/?linkid={linkid}&clcid=0x409&culture=en-us&country=us"


def download_win11_eval_iso(dest=None):
    """
    Auto-download Windows 11 Enterprise Evaluation ISO from Microsoft.
    
    Reverse-engineers the eval center download flow:
    1. GET the eval center web page
    2. Extract go.microsoft.com/fwlink redirect URLs
    3. Follow the fwlink to get the real ISO URL from Microsoft's CDN
    4. Download the ISO
    
    This is the same approach used by dockurr/windows.
    Returns path to downloaded ISO, or None on failure.
    """
    import re
    import subprocess as sp
    from html import unescape as html_unescape

    iso_path = dest or WIN_ISO
    
    if os.path.exists(iso_path) and os.path.getsize(iso_path) > 100_000_000:
        print(f"  ISO already cached: {iso_path}")
        return iso_path

    os.makedirs(CACHE_DIR, exist_ok=True)
    ua = "Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0"
    
    print("  Step 1: Fetching eval center page...")
    try:
        r = sp.run(["curl", "-sL", "--max-time", "30", "-A", ua, EVAL_WIN11_URL],
                   capture_output=True, text=True, timeout=35)
        html = html_unescape(r.stdout)
    except Exception as e:
        print(f"  ✗ Failed to fetch eval page: {e}")
        return None

    if len(html) < 1000:
        print(f"  ✗ Eval page returned empty/invalid response")
        return None

    # Extract fwlink URLs with culture=en-us and country=us
    pattern = r'https?://go\.microsoft\.com/fwlink/\?[^\s"<>]*?linkid=\d+[^\s"<>]*culture=en[^\s"<>]*country=us[^\s"<>]*'
    links = list(dict.fromkeys(re.findall(pattern, html, re.IGNORECASE)))
    
    if not links:
        # Fallback: extract linkid values and construct fwlink URLs
        linkids = list(dict.fromkeys(re.findall(r'linkid=(\d+)', html)))
        print(f"  Found {len(linkids)} linkid values (building fwlinks manually)")
        links = [FW_LINK_BASE.format(linkid=lid) for lid in linkids[:8]]  # test first 8
    
    if not links:
        print("  ✗ No fwlink URLs found on the eval page")
        print("    The Microsoft page structure may have changed.")
        print(f"    Manual download: {EVAL_WIN11_URL}")
        return None

    print(f"  Step 2: Found {len(links)} fwlink(s), resolving download URL...")

    # Follow each fwlink to find the x64 one
    dl_url = None
    for fwlink in links:
        try:
            r = sp.run(["curl", "-sL", "--max-time", "15", "-o", "/dev/null",
                        "-w", "%{url_effective}", "-A", ua, fwlink],
                       capture_output=True, text=True, timeout=20)
            url = r.stdout.strip()
            fn = url.split("/")[-1] if "/" in url else ""
            
            if not fn:
                continue
            
            # Look for x64 English ISO
            if "x64" in fn and "en-us" in fn.lower():
                dl_url = url
                break
            # Accept any x64 ISO
            if "x64" in fn and not dl_url:
                dl_url = url
        except Exception:
            continue
    
    if not dl_url:
        # Use the first link as fallback
        try:
            r = sp.run(["curl", "-sL", "--max-time", "15", "-o", "/dev/null",
                        "-w", "%{url_effective}", "-A", ua, links[0]],
                       capture_output=True, text=True, timeout=20)
            dl_url = r.stdout.strip()
        except Exception:
            pass

    if not dl_url or not dl_url.startswith("http"):
        print("  ✗ Could not resolve download URL from any fwlink")
        return None

    fn = dl_url.split("/")[-1]
    print(f"  Step 3: Downloading {fn[:60]}...")
    
    # Get content-length for progress reporting
    try:
        r = sp.run(["curl", "-sI", "--max-time", "15", dl_url],
                   capture_output=True, text=True, timeout=20)
        cl = re.search(r'content-length:\s*(\d+)', r.stdout, re.IGNORECASE)
        if cl:
            size_gb = int(cl.group(1)) / (1024**3)
            print(f"  Size: {size_gb:.1f} GB")
    except Exception:
        pass

    print(f"  Downloading to: {iso_path}")
    print(f"  This will take a while. Press Ctrl+C to cancel.")
    print()

    # Download with curl (shows progress)
    os.makedirs(os.path.dirname(iso_path), exist_ok=True)
    rc = os.system(
        f'curl -L --max-time 3600 --retry 3 -C - '
        f'-A "{ua}" '
        f'-o "{iso_path}" '
        f'--progress-bar '
        f'"{dl_url}"'
    )
    
    if rc == 0 and os.path.exists(iso_path) and os.path.getsize(iso_path) > 100_000_000:
        size_gb = os.path.getsize(iso_path) / (1024**3)
        print(f"\n  ✓ Downloaded: {iso_path} ({size_gb:.1f} GB)")
        return iso_path
    else:
        print(f"\n  ✗ Download failed (exit code: {rc})")
        if os.path.exists(iso_path):
            print(f"  Partial file size: {os.path.getsize(iso_path):,} bytes")
        return None


def find_windows_iso():
    """Find any Windows ISO in the cache directory."""
    if not os.path.isdir(CACHE_DIR):
        return None
    # First check the expected path
    if os.path.exists(WIN_ISO) and os.path.getsize(WIN_ISO) > 100_000_000:
        return WIN_ISO
    # Then check any ISO
    for f in sorted(os.listdir(CACHE_DIR)):
        if f.endswith(".iso"):
            fp = os.path.join(CACHE_DIR, f)
            try:
                if os.path.getsize(fp) > 100_000_000:
                    return fp
            except OSError:
                pass
    return None


def check_qemu():
    if not shutil.which(QEMU_BIN):
        print(f"ERROR: {QEMU_BIN} not found.")
        sys.exit(1)


def create_vm_disk():
    if os.path.exists(VM_DISK):
        print(f"  VM disk exists: {VM_DISK}")
        return VM_DISK
    os.makedirs(CACHE_DIR, exist_ok=True)
    print(f"  Creating {VM_DISK_SIZE} qcow2 disk...")
    subprocess.run([QEMU_BIN, "img", "create", "-f", "qcow2", VM_DISK, VM_DISK_SIZE],
                   check=True, capture_output=True)
    print(f"  Created: {VM_DISK}")
    return VM_DISK


def create_autounattend_xml():
    """Generate Windows unattended install answer file.
    
    This automates the Windows OOBE, creates an admin account, and skips
    the interactive prompts during installation. Attached as a floppy disk
    to the VM so Windows Setup picks it up automatically.
    """
    os.makedirs(CACHE_DIR, exist_ok=True)
    xml_path = os.path.join(CACHE_DIR, "Autounattend.xml")
    
    # Only write if it doesn't exist (don't overwrite user edits)
    if os.path.exists(xml_path):
        return xml_path

    xml = """<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend">
    <settings pass="windowsPE">
        <component name="Microsoft-Windows-Setup" 
                   processorArchitecture="amd64"
                   publicKeyToken="31bf3856ad364e35"
                   language="neutral" versionScope="nonSxS">
            <UserData>
                <AcceptEula>true</AcceptEula>
                <FullName>Aris Test</FullName>
                <Organization>celestia-island</Organization>
            </UserData>
            <DiskConfiguration>
                <Disk wcm:action="add">
                    <CreatePartitions>
                        <CreatePartition wcm:action="add">
                            <Order>1</Order><Size>500</Size><Type>Primary</Type>
                        </CreatePartition>
                        <CreatePartition wcm:action="add">
                            <Order>2</Order><Extend>true</Extend><Type>Primary</Type>
                        </CreatePartition>
                    </CreatePartitions>
                    <ModifyPartitions>
                        <ModifyPartition wcm:action="add">
                            <Order>1</Order><PartitionID>1</PartitionID>
                            <Format>NTFS</Format><Label>System</Label>
                        </ModifyPartition>
                        <ModifyPartition wcm:action="add">
                            <Order>2</Order><PartitionID>2</PartitionID>
                            <Format>NTFS</Format><Label>Windows</Label>
                        </ModifyPartition>
                    </ModifyPartitions>
                </Disk>
                <WillShowUI>OnError</WillShowUI>
            </DiskConfiguration>
            <ImageInstall>
                <OSImage>
                    <InstallTo>
                        <DiskID>0</DiskID><PartitionID>2</PartitionID>
                    </InstallTo>
                </OSImage>
            </ImageInstall>
        </component>
        <component name="Microsoft-Windows-International-Core-WinPE"
                   processorArchitecture="amd64"
                   publicKeyToken="31bf3856ad364e35"
                   language="neutral" versionScope="nonSxS">
            <SetupUILanguage><UILanguage>en-US</UILanguage></SetupUILanguage>
            <InputLocale>en-US</InputLocale>
            <SystemLocale>en-US</SystemLocale>
            <UILanguage>en-US</UILanguage>
            <UserLocale>en-US</UserLocale>
        </component>
    </settings>
    <settings pass="oobeSystem">
        <component name="Microsoft-Windows-Shell-Setup"
                   processorArchitecture="amd64"
                   publicKeyToken="31bf3856ad364e35"
                   language="neutral" versionScope="nonSxS">
            <OOBE>
                <HideEULAPage>true</HideEULAPage>
                <HideOEMRegistrationScreen>true</HideOEMRegistrationScreen>
                <HideOnlineAccountScreens>true</HideOnlineAccountScreens>
                <HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE>
                <ProtectYourPC>3</ProtectYourPC>
            </OOBE>
            <UserAccounts>
                <AdministratorPassword>
                    <Value>ArisTest1</Value>
                    <PlainText>true</PlainText>
                </AdministratorPassword>
            </UserAccounts>
            <AutoLogon>
                <Password><Value>ArisTest1</Value><PlainText>true</PlainText></Password>
                <Enabled>true</Enabled>
                <Username>Administrator</Username>
            </AutoLogon>
            <FirstLogonCommands>
                <SynchronousCommand wcm:action="add">
                    <Order>1</Order>
                    <CommandLine>cmd /c start D:\\windows\\install_evernight.bat</CommandLine>
                    <Description>Run Evernight installer</Description>
                </SynchronousCommand>
            </FirstLogonCommands>
        </component>
    </settings>
</unattend>"""
    with open(xml_path, "w") as f:
        f.write(xml)
    return xml_path


def create_floppy_with_autounattend():
    """Create a floppy disk image with Autounattend.xml.
    
    Windows Setup automatically looks for Autounattend.xml on removable
    media at boot. By attaching it as a floppy disk, the installation
    becomes fully unattended.
    """
    xml_path = create_autounattend_xml()
    floppy_path = os.path.join(CACHE_DIR, "autounattend.vfd")

    if os.path.exists(floppy_path):
        return floppy_path

    # Create a 1.44MB floppy image
    subprocess.run([
        QEMU_BIN, "img", "create", "-f", "raw", floppy_path, "1474560"
    ], capture_output=True, check=True)

    # Format as FAT12 and copy Autounattend.xml
    # We need mtools for this
    if shutil.which("mformat"):
        subprocess.run(
            ["mformat", "-f", "1440", "-i", floppy_path, "::"],
            capture_output=True,
        )
        subprocess.run(
            ["mcopy", "-i", floppy_path, xml_path, "::AUTOUNA~1.XML"],
            capture_output=True,
        )
        print(f"  Created autounattend floppy: {floppy_path}")
    else:
        print("  [!] mtools not installed — autounattend floppy not created")
        print("      Windows will require manual interaction during install")

    return floppy_path if os.path.exists(floppy_path) else None


def create_usb_test_iso():
    """
    Create a small bootable ISO that simulates the aris gadget USB drive.
    
    This ISO is attached as a USB mass-storage device to the QEMU VM,
    so Windows sees it as a removable USB drive. It contains:
    - autorun.inf at the root (triggers AutoRun)
    - windows/install_evernight.bat (the actual installer)
    - common/evernight-windows-amd64.exe (a sample binary)

    Windows will detect this volume as a USB drive and optionally run
    the AutoRun installer.
    """
    iso_path = os.path.join(CACHE_DIR, "aris-usb-test.iso")
    if os.path.exists(iso_path):
        return iso_path

    workdir = os.path.join(CACHE_DIR, "usb-iso-build")
    if os.path.exists(workdir):
        shutil.rmtree(workdir)
    os.makedirs(os.path.join(workdir, "windows"))
    os.makedirs(os.path.join(workdir, "common"))

    pkg = os.path.join(ARIS_ROOT, "package")

    # Copy installer files
    for sub in ["windows", "linux", "macos", "android", "common"]:
        src = os.path.join(pkg, sub)
        dst = os.path.join(workdir, sub)
        if os.path.isdir(src):
            for f in os.listdir(src):
                shutil.copy(os.path.join(src, f), os.path.join(dst, f))

    # Copy root-level files
    for f in os.listdir(pkg):
        fp = os.path.join(pkg, f)
        if os.path.isfile(fp) and not f.endswith(".sh"):
            shutil.copy(fp, workdir)

    # Mock evernight binary
    exe = os.path.join(workdir, "common", "evernight-windows-amd64.exe")
    with open(exe, "wb") as f:
        f.write(b"MZ\x90\x00\x03\x00\x00\x00\x04\x00\x00\x00\xff\xff\x00\x00"
                + b"\xb8\x00\x00\x00\x00\x00\x00\x00\x40\x00" * 8
                + b"evernight fixture binary - placeholder for testing\n")

    subprocess.run([
        "genisoimage", "-o", iso_path,
        "-J", "-R", "-V", "ARIS_GW",
        workdir
    ], capture_output=True, check=True)

    shutil.rmtree(workdir, ignore_errors=True)
    print(f"  Created USB test ISO: {iso_path}")
    return iso_path


def boot_windows(iso: str, test_mode: bool = False):
    """Boot Windows in QEMU with the test USB drive attached."""
    usb_iso = create_usb_test_iso() if test_mode else None
    floppy = create_floppy_with_autounattend()

    qemu_args = [
        QEMU_BIN,
        "-name", "aris-win10",
        "-m", str(VM_RAM_MB),
        "-smp", str(VM_CPU_CORES),
        "-cpu", "qemu64,+ssse3,+sse4.1,+sse4.2,+popcnt",
        "-machine", "type=pc,accel=tcg",
        # Boot disk (created once, persists)
        "-drive", f"file={VM_DISK},if=none,id=drive0,format=qcow2",
        "-device", "virtio-blk-pci,drive=drive0",
        # Windows installation ISO
        "-cdrom", iso,
        "-boot", "order=cd,menu=on",
        # Network (NAT + port forwarding for RDP)
        "-netdev", "user,id=net0,hostfwd=tcp::3389-:3389",
        "-device", "e1000,netdev=net0",
        # USB
        "-usb",
        "-device", "usb-tablet",
        # VNC display
        "-vnc", f"0.0.0.0:{VNC_PORT - 5900}",
        # QMP for automation
        "-qmp", "tcp:localhost:4444,server,nowait",
    ]

    # Attach autounattend floppy
    if floppy:
        qemu_args += [
            "-drive", f"file={floppy},if=floppy,format=raw",
        ]
        print("  Attached autounattend floppy (unattended install)")

    # Attach test USB ISO
    if usb_iso:
        qemu_args += [
            "-drive", f"file={usb_iso},if=none,id=usbdrive,format=raw",
            "-device", "usb-storage,drive=usbdrive,removable=on",
        ]
        print("  Attached USB test drive (simulates aris gadget)")

    # Attach the real installer.img as a second USB drive
    if os.path.exists(INSTALLER_IMG):
        qemu_args += [
            "-drive", f"file={INSTALLER_IMG},if=none,id=usbimg,format=raw",
            "-device", "usb-storage,drive=usbimg,removable=on",
        ]
        print("  Attached installer.img (production FAT image)")

    print()
    print("=" * 55)
    print("  QEMU Windows VM")
    print("=" * 55)
    print(f"  Display:  VNC localhost:{VNC_PORT}  (connect with vncviewer)")
    print(f"  RDP:      localhost:3389           (after Windows boots)")
    print(f"  QMP:      localhost:4444           (automation)")

    if test_mode:
        print()
        print("  Windows will boot. After the desktop appears:")
        print("  Open 'This PC' → see the ARIS_GW USB drive")
        print("  Double-click install_evernight.bat in the windows/ folder")
        print("  The installer copies evernight.exe and registers the service")

    print()
    print("  Press Ctrl+C to stop the VM.")
    print("=" * 55)

    try:
        subprocess.run(qemu_args, check=True)
    except KeyboardInterrupt:
        print("\nVM stopped.")


def cmd_status():
    print("VM status:")
    print(f"  Disk:      {'✓' if os.path.exists(VM_DISK) else '✗ not created'}")
    iso = find_windows_iso()
    if iso:
        sz = os.path.getsize(iso)
        print(f"  ISO:       ✓ {os.path.basename(iso)} ({sz:,} bytes)")
    else:
        print(f"  ISO:       ✗ not found")
        print(f"              Auto-download:  just windows-setup --auto-download")
        print(f"              Manual:         download from {EVAL_WIN11_URL}")
        print(f"              Expected:       {WIN_ISO}")
    print(f"  Installer: {'✓' if os.path.exists(INSTALLER_IMG) else '✗ (run: just test-gadget first)'}")
    print(f"  Cache:     {CACHE_DIR}")


def cmd_setup(auto_download=False):
    os.makedirs(CACHE_DIR, exist_ok=True)
    check_qemu()

    if auto_download:
        print("Step 1: Auto-download Windows 11 Evaluation ISO")
        iso = download_win11_eval_iso()
        if not iso:
            print("\n✗ Auto-download failed.")
            print(f"  Manual download: {EVAL_WIN11_URL}")
            return 1
    else:
        print("Step 1: Check Windows ISO")
        iso = find_windows_iso()
        if iso:
            print(f"  ✓ Found: {iso}")
        else:
            print(f"  ✗ Windows ISO not found.")
            print(f"    Expected location: {WIN_ISO}")
            print()
            print("  To auto-download (Win11 x64 en-us):")
            print("    just windows-setup --auto-download")
            print()
            print("  Or download manually from:")
            print(f"    {EVAL_WIN11_URL}")
            return 1

    print()
    print("Step 2: Create VM disk")
    create_vm_disk()

    print()
    print("Step 3: Create autounattend floppy")
    create_floppy_with_autounattend()

    print()
    print("✓ Setup complete. Run: just windows-test")
    print()
    print("  Connect with:  vncviewer localhost:5900")
    print("  Windows user:  Administrator")
    print("  Password:      ArisTest1")
    return 0


def main():
    parser = argparse.ArgumentParser(
        description="QEMU Windows test harness for aris USB gadget testing",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  just windows-status      Check readiness
  just windows-setup       Create VM disk, verify ISO
  just windows-test        Boot VM with USB test drive
  just windows-interactive Boot VM for manual exploration
        """,
    )
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--setup", action="store_true", help="Create VM disk, check ISO")
    group.add_argument("--auto-download", action="store_true", help="Auto-download Win11 eval ISO + setup VM")
    group.add_argument("--test", action="store_true", help="Boot VM with USB test drive attached")
    group.add_argument("--interactive", action="store_true", help="Boot VM without test USB")
    group.add_argument("--status", action="store_true", help="Show readiness")
    parser.add_argument("--iso", help="Path to Windows ISO (override auto-detection)")

    args = parser.parse_args()
    os.makedirs(CACHE_DIR, exist_ok=True)
    check_qemu()

    if args.status:
        cmd_status()
        return 0

    if args.setup:
        return cmd_setup(auto_download=False)

    if args.auto_download:
        return cmd_setup(auto_download=True)

    iso = args.iso or find_windows_iso()
    if not iso:
        print("ERROR: No Windows ISO found.")
        print("Run 'just windows-setup --auto-download' to get one.")
        return 1

    create_vm_disk()

    if args.test:
        boot_windows(iso, test_mode=True)
    elif args.interactive:
        boot_windows(iso, test_mode=False)
    else:
        parser.print_help()

    return 0


if __name__ == "__main__":
    sys.exit(main())

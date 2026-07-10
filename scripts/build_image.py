#!/usr/bin/env python3
"""aris — assemble bootable SD card image.

Pure-Python assembly (no Docker needed). All partitions use ext4 because
U-Boot supports ext4 and it eliminates the FAT/mtools dependency.

Produces sdcard.img flashable with:
    dd if=sdcard.img of=/dev/sdX bs=4M conv=fsync
    bmaptool copy sdcard.img /dev/sdX

For Rockchip eMMC flashing via USB (maskrom mode):
    rkdeveloptool db MiniLoaderAll.bin
    rkdeveloptool wl 0 sdcard.img

Usage:
    python3 scripts/build_image.py nanopi-r3s
"""
from __future__ import annotations

import shutil
import struct
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent / "utils"))
import build_env
import cli_format as cf

PROJECT_ROOT = Path(__file__).resolve().parent.parent
SECTOR = 512

# Partition layout (LBA units)
IDBLOADER_LBA = 64          # 32KB — Rockchip BootROM
UBOOT_ITB_LBA = 16384       # 8MB

BOOT_START_LBA = 32768      # 16MB
BOOT_SIZE_SEC = 262144      # 128MB
ROOTFS_START_LBA = BOOT_START_LBA + BOOT_SIZE_SEC  # 144MB
ROOTFS_SIZE_SEC = 524288    # 256MB
DATA_START_LBA = ROOTFS_START_LBA + ROOTFS_SIZE_SEC # 400MB
DATA_SIZE_SEC = 131072      # 64MB

TOTAL_LBA = DATA_START_LBA + DATA_SIZE_SEC + 2048  # extra padding

# GPT partition type GUIDs
GPT_TYPE_LINUX = "0fc63daf-8483-4772-8e79-3d69d8477de4"


def lba_to_mb(lba: int) -> int:
    return lba * SECTOR // (1024 * 1024)


def create_ext4_image(
    image_path: Path, source_dir: Path | None = None,
    size_mb: int = 64, label: str = "ARIS",
) -> bool:
    """Create an ext4 filesystem image, optionally populated from source_dir."""
    truncate_cmd = ["truncate", "-s", f"{size_mb}M", str(image_path)]
    subprocess.run(truncate_cmd, check=True)

    mkfs = ["/usr/sbin/mkfs.ext4", "-q", "-F", "-L", label]
    if source_dir and source_dir.is_dir():
        mkfs.extend(["-d", str(source_dir)])
    mkfs.append(str(image_path))

    result = subprocess.run(mkfs, capture_output=True, text=True)
    return result.returncode == 0


def compile_boot_scr(board: str, output_dir: Path) -> Path | None:
    """Compile boot.cmd → boot.scr using mkimage inside Docker (lightweight)."""
    boot_cmd = PROJECT_ROOT / "board" / board / "boot.cmd"
    if not boot_cmd.exists():
        return None

    boot_scr = output_dir / "boot.scr"
    result = subprocess.run(  # noqa: F841  (subprocess side-effect: runs docker boot.cmd)
        [*build_env.docker_cmd(), "run", "--rm",
         "-v", f"{boot_cmd}:/in/boot.cmd:ro",
         "-v", f"{output_dir}:/out",
         "ubuntu:22.04",
         "bash", "-c",
         "apt-get update -qq >/dev/null 2>&1 && "
         "apt-get install -y -qq u-boot-tools >/dev/null 2>&1 && "
         "mkimage -A arm64 -O linux -T script -C none "
         "-d /in/boot.cmd /out/boot.scr"],
        capture_output=True, text=True,
    )
    if boot_scr.exists():
        cf.ok("  boot.scr compiled")
        return boot_scr
    cf.warn("  boot.scr compilation failed — skipping")
    return None


def write_gpt(image_path: Path, partitions: list[dict]) -> None:
    """Write a GPT partition table to an image file (pure Python).

    partitions: list of dicts with keys: name, type_guid, start_lba, end_lba
    """
    total_lba = image_path.stat().st_size // SECTOR

    with open(image_path, "r+b") as f:
        # ── Protective MBR (sector 0) ──
        f.seek(0)
        f.write(b"\x00" * 446)  # partition table area
        # One partition entry: type 0xEE, spanning disk
        f.write(struct.pack("<B", 0))       # status
        f.write(struct.pack("<3s", b"\x00\x01\x00"))  # CHS first
        f.write(struct.pack("<B", 0xEE))    # type = GPT protective
        f.write(struct.pack("<3s", b"\xff\xff\xff"))  # CHS last
        f.write(struct.pack("<I", 1))       # LBA first
        max_lba = min(total_lba - 1, 0xFFFFFFFF)
        f.write(struct.pack("<I", max_lba)) # LBA last
        f.write(b"\x00" * 48)               # remaining entries
        f.write(struct.pack("<H", 0xAA55))  # boot signature

        # ── GPT Header (sector 1) ──
        f.seek(SECTOR)
        header = bytearray(SECTOR)
        sig = b"EFI PART"
        header[0:8] = sig
        struct.pack_into("<I", header, 8, 0x00010000)  # revision 1.0
        struct.pack_into("<I", header, 12, 92)          # header size
        # CRC32 of header (fill 0 for now, calculate later)
        struct.pack_into("<I", header, 16, 0)           # header CRC32 placeholder
        struct.pack_into("<I", header, 20, 0)           # reserved
        struct.pack_into("<Q", header, 24, 1)           # my LBA
        struct.pack_into("<Q", header, 32, total_lba - 1)  # backup LBA
        struct.pack_into("<Q", header, 40, IDBLOADER_LBA)  # first usable LBA
        struct.pack_into("<Q", header, 48, total_lba - 34) # last usable LBA
        # Disk GUID (random-ish)
        disk_guid = bytes([0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0,
                          0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0])
        header[56:72] = disk_guid
        struct.pack_into("<Q", header, 72, 2)           # partition entries start LBA
        struct.pack_into("<I", header, 80, len(partitions))  # num entries
        struct.pack_into("<I", header, 84, 128)         # entry size

        # Calculate header CRC32 (with CRC field zeroed)
        import zlib
        header_crc = zlib.crc32(header[:92]) & 0xFFFFFFFF
        struct.pack_into("<I", header, 16, header_crc)
        f.write(bytes(header))

        # ── Partition entries (sectors 2+) ──
        f.seek(2 * SECTOR)
        for i, p in enumerate(partitions):
            entry = bytearray(128)
            # Type GUID (string → bytes, little-endian groups)
            type_guid = uuid_str_to_bytes(p["type_guid"])
            entry[0:16] = type_guid
            # Unique GUID (random-ish, derived from index)
            unique = bytearray(16)
            unique[0] = i + 1
            entry[16:32] = bytes(unique)
            struct.pack_into("<Q", entry, 32, p["start_lba"])
            struct.pack_into("<Q", entry, 40, p["end_lba"])
            struct.pack_into("<Q", entry, 48, 0)  # attributes
            # Name (UTF-16LE, padded to 72 bytes)
            name_bytes = p["name"].encode("utf-16-le")[:72]
            entry[56:56 + len(name_bytes)] = name_bytes
            f.write(bytes(entry))

        # Pad remaining entries
        for i in range(len(partitions), 128):
            f.write(b"\x00" * 128)

        # ── Backup partition entries (sector -33 to -2) ──
        backup_entries_lba = total_lba - 33
        f.seek(2 * SECTOR)
        entries_data = f.read(128 * 128)
        f.seek(backup_entries_lba * SECTOR)
        f.write(entries_data)

        # ── Backup GPT header (last sector) ──
        f.seek((total_lba - 1) * SECTOR)
        backup = bytearray(header)
        struct.pack_into("<Q", backup, 24, total_lba - 1)  # my LBA
        struct.pack_into("<Q", backup, 32, 1)              # backup (primary) LBA
        struct.pack_into("<Q", backup, 72, backup_entries_lba)  # entries start LBA
        struct.pack_into("<I", backup, 16, 0)              # CRC placeholder
        backup_crc = zlib.crc32(backup[:92]) & 0xFFFFFFFF
        struct.pack_into("<I", backup, 16, backup_crc)
        f.write(bytes(backup))


def uuid_str_to_bytes(s: str) -> bytes:
    """Convert a UUID string to little-endian bytes (GPT format)."""
    parts = s.split("-")
    if len(parts) != 5:
        return b"\x00" * 16
    data = b""
    data += struct.pack("<I", int(parts[0], 16))  # time_low (LE)
    data += struct.pack("<H", int(parts[1], 16))  # time_mid (LE)
    data += struct.pack("<H", int(parts[2], 16))  # time_hi (LE)
    data += bytes([int(parts[3][2:4], 16), int(parts[3][4:6], 16)])  # clock seq + node
    data += bytes(int(parts[4][i:i+2], 16) for i in range(0, 12, 2))
    return data


def write_at_offset(image: Path, data: Path, offset_bytes: int) -> None:
    """Write data file content at a byte offset in the image."""
    with open(image, "r+b") as img:
        img.seek(offset_bytes)
        with open(data, "rb") as src:
            while True:
                chunk = src.read(1024 * 1024)
                if not chunk:
                    break
                img.write(chunk)


def main() -> int:
    if build_env.wsl_main_guard():
        return 0
    import argparse

    parser = argparse.ArgumentParser(description="Assemble SD card image")
    parser.add_argument("board", nargs="?", default="nanopi-r3s")
    args = parser.parse_args()

    board = args.board
    output_dir = PROJECT_ROOT / "output" / board

    cf.section(f"aris image assembly: {board}")

    rootfs = output_dir / "rootfs"
    kernel = output_dir / "Image"
    dtb = output_dir / "board.dtb"
    uboot_idbloader = output_dir / "idbloader.img"
    uboot_itb = output_dir / "u-boot.itb"

    if not rootfs.exists():
        cf.fail(f"Rootfs not found: {rootfs}")
        cf.info("  Run: python3 scripts/build.py " + board)
        return 1

    # ── Prepare boot directory ──
    cf.blank()
    cf.step("Preparing boot partition contents")
    boot_dir = output_dir / "boot-staging"
    if boot_dir.exists():
        shutil.rmtree(boot_dir)
    boot_dir.mkdir()

    if kernel.exists():
        shutil.copy2(kernel, boot_dir / "Image")
        cf.ok(f"  Image → boot ({kernel.stat().st_size // 1024}KB)")
    if dtb.exists():
        shutil.copy2(dtb, boot_dir / "board.dtb")
        cf.ok("  board.dtb → boot")

    # Compile boot.scr
    boot_scr = compile_boot_scr(board, output_dir)
    if boot_scr:
        shutil.copy2(boot_scr, boot_dir / "boot.scr")

    # ── Create partition images ──
    cf.blank()
    cf.step("Creating partition filesystem images")

    boot_img = output_dir / "boot.ext4"
    if create_ext4_image(boot_img, boot_dir, BOOT_SIZE_SEC * SECTOR // (1024*1024), "ARISBOOT"):
        cf.ok(f"  boot.ext4 ({lba_to_mb(BOOT_SIZE_SEC)}MB)")
    else:
        cf.fail("Failed to create boot.ext4")
        return 1

    rootfs_img = output_dir / "rootfs.ext4"
    if create_ext4_image(rootfs_img, rootfs, ROOTFS_SIZE_SEC * SECTOR // (1024*1024), "ARISROOT"):
        cf.ok(f"  rootfs.ext4 ({lba_to_mb(ROOTFS_SIZE_SEC)}MB)")
    else:
        cf.fail("Failed to create rootfs.ext4")
        return 1

    data_img = output_dir / "data.ext4"
    if create_ext4_image(data_img, None, DATA_SIZE_SEC * SECTOR // (1024*1024), "ARISDATA"):
        cf.ok(f"  data.ext4 ({lba_to_mb(DATA_SIZE_SEC)}MB)")
    else:
        cf.fail("Failed to create data.ext4")
        return 1

    # ── Assemble SD card ──
    cf.blank()
    cf.step("Assembling SD card image")
    sdcard = output_dir / "sdcard.img"

    # Create base image
    subprocess.run(["truncate", "-s", str(TOTAL_LBA * SECTOR), str(sdcard)], check=True)

    # Write U-Boot at Rockchip offsets
    if uboot_idbloader.exists():
        write_at_offset(sdcard, uboot_idbloader, IDBLOADER_LBA * SECTOR)
        cf.ok(f"  idbloader.img @ {lba_to_mb(IDBLOADER_LBA)}MB")
    else:
        cf.warn("  No idbloader.img (BootROM won't find a loader)")

    if uboot_itb.exists():
        write_at_offset(sdcard, uboot_itb, UBOOT_ITB_LBA * SECTOR)
        cf.ok(f"  u-boot.itb @ {lba_to_mb(UBOOT_ITB_LBA)}MB")
    else:
        cf.warn("  No u-boot.itb")

    # Write partition images at offsets
    write_at_offset(sdcard, boot_img, BOOT_START_LBA * SECTOR)
    cf.ok(f"  boot @ {lba_to_mb(BOOT_START_LBA)}MB")
    write_at_offset(sdcard, rootfs_img, ROOTFS_START_LBA * SECTOR)
    cf.ok(f"  rootfs @ {lba_to_mb(ROOTFS_START_LBA)}MB")
    write_at_offset(sdcard, data_img, DATA_START_LBA * SECTOR)
    cf.ok(f"  data @ {lba_to_mb(DATA_START_LBA)}MB")

    # Write GPT partition table
    cf.step("Writing GPT partition table")
    partitions = [
        {"name": "boot",   "type_guid": GPT_TYPE_LINUX,
         "start_lba": BOOT_START_LBA,   "end_lba": ROOTFS_START_LBA - 1},
        {"name": "rootfs", "type_guid": GPT_TYPE_LINUX,
         "start_lba": ROOTFS_START_LBA, "end_lba": DATA_START_LBA - 1},
        {"name": "data",   "type_guid": GPT_TYPE_LINUX,
         "start_lba": DATA_START_LBA,   "end_lba": TOTAL_LBA - 34},
    ]
    write_gpt(sdcard, partitions)
    cf.ok(f"  GPT: {len(partitions)} partitions")

    # Cleanup intermediate files
    shutil.rmtree(boot_dir, ignore_errors=True)
    boot_img.unlink(missing_ok=True)
    rootfs_img.unlink(missing_ok=True)
    data_img.unlink(missing_ok=True)

    # Final report
    size_mb = sdcard.stat().st_size // (1024 * 1024)
    cf.blank()
    cf.ok(f"SD card image: {sdcard} ({size_mb}MB)")

    cf.blank()
    cf.ok("Flash instructions:")
    cf.info(f"  SD card:  dd if={sdcard.name} of=/dev/sdX bs=4M conv=fsync")
    cf.info(f"  Or:       bmaptool copy {sdcard.name} /dev/sdX")
    cf.info(f"  eMMC USB: rkdeveloptool db MiniLoaderAll.bin && rkdeveloptool wl 0 {sdcard.name}")

    cf.blank()
    cf.info("Partition layout:")
    cf.info(f"  {'Part':<8} {'Start':>8} {'Size':>8} {'Type':<8} Name")
    for p in partitions:
        sz = lba_to_mb(p["end_lba"] - p["start_lba"] + 1)
        cf.info(f"  {partitions.index(p)+1:<8} {lba_to_mb(p['start_lba']):>7}MB {sz:>7}MB ext4     {p['name']}")

    return 0


if __name__ == "__main__":
    sys.exit(main())

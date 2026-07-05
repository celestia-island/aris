#!/usr/bin/env python3
"""
Pure-Python FAT16/FAT32 reader for validating installer images.

Reads the BPB (BIOS Parameter Block), walks the directory tree, and extracts
file contents without any external tools (no mtools, no mount, no root).

Only implements read operations — sufficient for validation tests.
"""

import struct
import sys
import os
from datetime import datetime
from dataclasses import dataclass
from typing import Optional


@dataclass
class FatBPB:
    """BIOS Parameter Block — the FAT volume metadata."""
    bytes_per_sector: int
    sectors_per_cluster: int
    reserved_sectors: int
    num_fats: int
    root_entry_count: int  # FAT16 only
    total_sectors16: int   # FAT16 total
    media_type: int
    sectors_per_fat16: int     # FAT16
    sectors_per_track: int
    num_heads: int
    hidden_sectors: int
    total_sectors32: int      # FAT32 total
    fats32: int               # FAT32: sectors per FAT
    root_cluster: int            # FAT32: root dir cluster
    fs_info_sector: int
    backup_boot: int
    fat_type: int               # 16 or 32

    @property
    def fat_offset(self) -> int:
        """Byte offset of the first FAT table."""
        return self.reserved_sectors * self.bytes_per_sector

    @property
    def fat_size(self) -> int:
        """Size of one FAT table in bytes."""
        return self.sectors_per_fat16 * self.bytes_per_sector if self.fat_type == 16 \
            else self.fats32 * self.bytes_per_sector

    @property
    def root_dir_offset(self) -> int:
        """FAT16: offset of root directory area."""
        return self.fat_offset + self.num_fats * self.fat_size

    @property
    def root_dir_sectors(self) -> int:
        """FAT16: number of sectors for root directory."""
        return (self.root_entry_count * 32 + self.bytes_per_sector - 1) // self.bytes_per_sector

    @property
    def data_offset(self) -> int:
        """Offset of the data area (cluster 2)."""
        if self.fat_type == 16:
            return self.root_dir_offset + self.root_dir_sectors * self.bytes_per_sector
        else:
            return self.fat_offset + self.num_fats * self.fat_size

    @property
    def cluster_offset(self) -> int:
        """Bytes per cluster."""
        return self.sectors_per_cluster * self.bytes_per_sector


@dataclass
class DirEntry:
    """A 32-byte FAT directory entry."""
    name: str
    ext: str
    attr: int
    first_cluster: int
    file_size: int
    is_dir: bool
    is_long_name: bool
    is_deleted: bool
    is_volume_label: bool
    long_name_parts: list

    @property
    def full_name(self) -> str:
        if self.ext:
            return f"{self.name}.{self.ext}"
        return self.name


class FatReader:
    """Read-only FAT16/FAT32 filesystem."""

    def __init__(self, path: str):
        with open(path, "rb") as f:
            self.data = f.read()
        self.path = path
        self.bpb = self._parse_bpb()

    def _parse_bpb(self) -> FatBPB:
        d = self.data
        bytes_per_sector = struct.unpack_from("<H", d, 11)[0]
        sectors_per_cluster = d[13]
        reserved_sectors = struct.unpack_from("<H", d, 14)[0]
        num_fats = d[16]
        root_entry_count = struct.unpack_from("<H", d, 17)[0]
        total16 = struct.unpack_from("<H", d, 19)[0]
        media_type = d[21]
        sectors_per_fat16 = struct.unpack_from("<H", d, 22)[0]
        sectors_per_track = struct.unpack_from("<H", d, 24)[0]
        num_heads = struct.unpack_from("<H", d, 26)[0]
        hidden = struct.unpack_from("<I", d, 28)[0]
        total32 = struct.unpack_from("<I", d, 32)[0]

        # Determine FAT type
        total_sectors = total16 if total16 != 0 else total32
        if total_sectors == 0:
            total_sectors = total32

        # Count clusters
        root_dir_sectors = (root_entry_count * 32 + bytes_per_sector - 1) // bytes_per_sector
        data_sectors = total_sectors - reserved_sectors - 0 - root_dir_sectors
        if sectors_per_fat16 != 0:
            fat_sectors = num_fats * sectors_per_fat16
        else:
            fats32 = struct.unpack_from("<I", d, 36)[0]
            fat_sectors = num_fats * fats32
        count_of_clusters = data_sectors // sectors_per_cluster

        fat_type = 32 if count_of_clusters >= 65525 else 16

        fats32 = struct.unpack_from("<I", d, 36)[0] if fat_type == 32 else 0
        root_cluster = struct.unpack_from("<I", d, 44)[0] if fat_type == 32 else 0
        fs_info = struct.unpack_from("<H", d, 48)[0] if fat_type == 32 else 0
        backup = d[50] if fat_type == 32 else 0

        return FatBPB(
            bytes_per_sector=bytes_per_sector,
            sectors_per_cluster=sectors_per_cluster,
            reserved_sectors=reserved_sectors,
            num_fats=num_fats,
            root_entry_count=root_entry_count,
            total_sectors16=total16,
            media_type=media_type,
            sectors_per_fat16=sectors_per_fat16,
            sectors_per_track=sectors_per_track,
            num_heads=num_heads,
            hidden_sectors=hidden,
            total_sectors32=total32,
            fats32=fats32,
            root_cluster=root_cluster,
            fs_info_sector=fs_info,
            backup_boot=backup,
            fat_type=fat_type,
        )

    def _fat_entry(self, cluster: int) -> int:
        """Read a FAT table entry for a cluster."""
        bpb = self.bpb
        if bpb.fat_type == 16:
            offset = bpb.fat_offset + cluster * 2
            return struct.unpack_from("<H", self.data, offset)[0] & 0xFFFF
        else:
            offset = bpb.fat_offset + cluster * 4
            return struct.unpack_from("<I", self.data, offset)[0] & 0x0FFFFFFF

    def _cluster_chain(self, start: int) -> list:
        """Follow the cluster chain starting from `start`."""
        chain = []
        cluster = start
        seen = set()
        while cluster >= 2 and cluster < 0xFFF8 and cluster not in seen:
            seen.add(cluster)
            chain.append(cluster)
            cluster = self._fat_entry(cluster)
        return chain

    def _read_cluster(self, cluster: int) -> bytes:
        """Read the raw bytes of a cluster."""
        bpb = self.bpb
        offset = bpb.data_offset + (cluster - 2) * bpb.cluster_offset
        return self.data[offset:offset + bpb.cluster_offset]

    def _read_chain(self, clusters: list, max_bytes: int = -1) -> bytes:
        """Read all clusters in a chain."""
        result = bytearray()
        for c in clusters:
            result.extend(self._read_cluster(c))
            if max_bytes > 0 and len(result) >= max_bytes:
                return bytes(result[:max_bytes])
        return bytes(result)

    def _parse_dir_entries(self, data: bytes) -> list:
        """Parse 32-byte directory entries from a data block."""
        entries = []
        long_name_parts = []
        for i in range(0, len(data), 32):
            chunk = data[i:i + 32]
            if len(chunk) < 32:
                break
            if chunk[0] == 0x00:  # End of directory
                break
            if chunk[0] == 0xE5:  # Deleted
                long_name_parts = []
                continue

            attr = chunk[11]

            # Long filename entry
            if attr & 0x0F == 0x0F:
                seq = chunk[0]
                if seq & 0x40:  # Last entry marker
                    seq = seq & 0x3F
                # Extract UCS-2 chars from positions 1-10, 14-25, 28-31
                chars = chunk[1:11] + chunk[14:26] + chunk[28:32]
                try:
                    name = chars.decode("utf-16-le").rstrip("\x00")
                    long_name_parts.append((seq, name))
                except Exception:
                    pass
                continue

            # Short name entry
            name_raw = chunk[0:8]
            ext_raw = chunk[8:11]
            name = name_raw.decode("ascii", errors="replace").rstrip()
            ext = ext_raw.decode("ascii", errors="replace").rstrip()

            first_cluster = struct.unpack_from("<H", chunk, 26)[0]
            if bpb_fat32_check := struct.unpack_from("<H", chunk, 20)[0]:
                first_cluster |= (bpb_fat32_check << 16)
            file_size = struct.unpack_from("<I", chunk, 28)[0]

            is_dir = bool(attr & 0x10)
            is_volume = bool(attr & 0x08)

            full_long = ""
            if long_name_parts:
                long_name_parts.sort()
                full_long = "".join(n for _, n in long_name_parts)
                long_name_parts = []

            entries.append(DirEntry(
                name=name,
                ext=ext,
                attr=attr,
                first_cluster=first_cluster,
                file_size=file_size,
                is_dir=is_dir,
                is_long_name=False,
                is_deleted=False,
                is_volume_label=is_volume,
                long_name_parts=[full_long] if full_long else [],
            ))

        return entries

    def list_root(self) -> list:
        """List root directory entries."""
        bpb = self.bpb
        if bpb.fat_type == 16:
            data = self.data[bpb.root_dir_offset:bpb.root_dir_offset + bpb.root_entry_count * 32]
            return self._parse_dir_entries(data)
        else:
            return self.list_dir(bpb.root_cluster)

    def list_dir(self, cluster: int) -> list:
        """List entries in a directory cluster."""
        chain = self._cluster_chain(cluster)
        data = self._read_chain(chain)
        return self._parse_dir_entries(data)

    def read_file(self, first_cluster: int, size: int) -> bytes:
        """Read file data from cluster chain."""
        chain = self._cluster_chain(first_cluster)
        return self._read_chain(chain, max_bytes=size)[:size]

    def find_in_dir(self, entries: list, name: str) -> Optional[DirEntry]:
        """Find an entry by name (case-insensitive)."""
        name_lower = name.lower()
        for e in entries:
            if e.full_name.lower() == name_lower:
                return e
            if e.long_name_parts:
                for ln in e.long_name_parts:
                    if ln.lower() == name_lower:
                        return e
            # Also match short name with extension
            short = e.full_name.lower()
            if short.startswith(name_lower[:6]):
                return e
        return None

    def walk(self, prefix: str = "") -> list:
        """Walk the entire filesystem, returning (path, size, is_dir) tuples."""
        results = []

        def walk_dir(entries, path_prefix):
            for e in entries:
                if e.is_volume_label or e.name in (".", ".."):
                    continue
                full_path = f"{path_prefix}/{e.full_name}" if path_prefix else e.full_name
                # Prefer long name
                if e.long_name_parts and e.long_name_parts[0]:
                    full_path = f"{path_prefix}/{e.long_name_parts[0]}" if path_prefix else e.long_name_parts[0]

                results.append((full_path, e.file_size, e.is_dir))

                if e.is_dir and e.first_cluster >= 2:
                    sub_entries = self.list_dir(e.first_cluster)
                    walk_dir(sub_entries, full_path)

        root = self.list_root()
        walk_dir(root, prefix)
        return results


def main():
    if len(sys.argv) < 2:
        print(f"Usage: {sys.argv[0]} <image.img> [extract <path>]")
        print(f"       {sys.argv[0]} <image.img> --walk")
        return 1

    image = sys.argv[1]
    if not os.path.exists(image):
        print(f"Error: {image} not found")
        return 1

    reader = FatReader(image)
    bpb = reader.bpb
    print(f"FAT{bpb.fat_type} image: {image}")
    print(f"  bytes/sector:    {bpb.bytes_per_sector}")
    print(f"  sectors/cluster: {bpb.sectors_per_cluster}")
    print(f"  reserved:       {bpb.reserved_sectors}")
    print(f"  FATs:         {bpb.num_fats}")
    print(f"  cluster size:  {bpb.cluster_offset} bytes")
    print()

    if len(sys.argv) >= 4 and sys.argv[2] == "extract":
        target = sys.argv[3]
        root = reader.list_root()
        # Try to find file in root, then common/, windows/, etc.
        for prefix in ["", "common/", "windows/", "linux/", "macos/", "android/"]:
            parts = (prefix + target).split("/")
            entries = reader.list_root()
            found = None
            current_entries = entries
            for i, part in enumerate(parts):
                if not part:
                    continue
                entry = reader.find_in_dir(current_entries, part)
                if entry is None:
                    break
                if i == len(parts) - 1:
                    found = entry
                    break
                else:
                    current_entries = reader.list_dir(entry.first_cluster)

            if found:
                data = reader.read_file(found.first_cluster, found.file_size)
                sys.stdout.buffer.write(data)
                return 0

        print(f"File not found: {target}")
        return 1

    if len(sys.argv) >= 3 and sys.argv[2] == "--walk":
        print("Walking filesystem...")
        files = reader.walk()
        for path, size, is_dir in sorted(files):
            type_str = "[DIR]" if is_dir else f"{size:>8}"
            print(f"  {type_str}  {path}")
        print(f"\nTotal: {len(files)} entries")
        return 0

    # Default: list root
    print("Root directory:")
    for e in reader.list_root():
        if e.is_volume_label:
            continue
        type_str = "[DIR]" if e.is_dir else f"{e.file_size:>8}"
        long = e.long_name_parts[0] if e.long_name_parts else ""
        display = long or e.full_name
        print(f"  {type_str}  {display}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

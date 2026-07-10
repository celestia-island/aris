# U-Boot boot script for NanoPi R3S
# Compiled to boot.scr with: mkimage -A arm64 -O linux -T script -C none -d boot.cmd boot.scr

# Detect boot partition (A or B)
if test -n "${boot_part}"; then
    setenv bootpart "${boot_part}"
else
    setenv bootpart "A"
fi

# Load kernel, device tree, and initramfs
# Uses 'load' (filesystem-agnostic) instead of 'fatload' so both
# FAT32 and ext4 boot partitions work.
load mmc 0:${bootpart} ${kernel_addr_r} /Image
load mmc 0:${bootpart} ${fdt_addr_r} /board.dtb
load mmc 0:${bootpart} ${ramdisk_addr_r} /initramfs.cpio.gz 2>/dev/null || true

# Set bootargs for the active rootfs partition (partition 2)
setenv bootargs "console=ttyS2,1500000n8 root=/dev/mmcblk0p2 rootfstype=ext4 ro rootwait quiet"

# Boot Linux
booti ${kernel_addr_r} ${ramdisk_addr_r} ${fdt_addr_r}

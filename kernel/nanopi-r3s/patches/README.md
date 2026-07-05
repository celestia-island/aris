# Kernel patches for NanoPi R3S (RK3566)

This directory contains kernel patches to enable features not yet upstream.

## Applying

Patches are applied automatically by `scripts/build.sh` during the kernel build step.

To add a new patch:

1. Place the `.patch` file in this directory
2. Rebuild with `just build-board nanopi-r3s`

## Current Patches

(none — kernel 6.12 has full RK3566 support out of the box)

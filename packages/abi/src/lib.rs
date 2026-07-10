// SPDX-License-Identifier: BUSL-1.1

//! aris-abi — Linux ABI compatibility layer for the kei kernel.
//!
//! Provides user-space fallbacks for Linux features that kei does not
//! implement natively, allowing standard Linux arm64 binaries to run:
//!
//! - **Syscall fallbacks**: userspace implementations of missing syscalls
//!   (posix_spawn, SYSV shm) using existing kernel syscalls
//! - **fbdev driver**: user-space `/dev/fb0` access via mmap or ioctls
//! - **DRM translation**: translates DRM ioctls to fbdev operations
//! - **procfs/sysfs**: minimal `/proc` and `/sys` emulation via FUSE or
//!   tmpfs overlays
//!
//! ## Architecture
//!
//! ```text
//! Application (standard Linux arm64 binary)
//!   ↓
//! aris-abi (LD_PRELOAD or static link)
//!   ↓
//! kei kernel syscall ABI
//! ```

#![allow(dead_code)]

pub mod syscall_shim;
pub mod fbdev_mmap;
pub mod drm_translate;
pub mod proc_sys;

pub use syscall_shim::SyscallShim;
pub use fbdev_mmap::FbDevMmap;
pub use drm_translate::DrmToFbdev;
pub use proc_sys::ProcSysEmulator;

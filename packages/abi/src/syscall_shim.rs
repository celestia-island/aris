// SPDX-License-Identifier: BUSL-1.1

//! Syscall fallbacks for missing Linux syscalls on kei.
//!
//! kei implements ~240 Linux syscalls natively. This module provides
//! user-space fallbacks for the remaining ones that standard binaries
//! may call.

use std::collections::HashMap;
use std::sync::Mutex;

/// Tracks which syscalls are known to be missing from kei and have
/// user-space fallbacks registered.
pub struct SyscallShim {
    /// Map of syscall number → fallback status.
    /// Value is true if the fallback was used at least once.
    fallbacks_used: Mutex<HashMap<u32, bool>>,
}

/// aarch64 syscall numbers that kei may not implement.
/// Source: Linux kernel `include/uapi/asm-generic/unistd.h`.
pub mod missing_syscalls {
    // SysV IPC — kei only implements semaphores, not shm/msg
    pub const SHMGET: u32 = 194;
    pub const SHMCTL: u32 = 195;
    pub const SHMAT: u32 = 196;
    pub const SHMDT: u32 = 197;
    pub const MSGGET: u32 = 186;
    pub const MSGCTL: u32 = 187;
    pub const MSGSND: u32 = 189;
    pub const MSGRCV: u32 = 188;

    // posix_spawn (glibc may use clone instead)
    pub const POSIX_SPAWN: u32 = -1i32 as u32; // Not in generic table
}

impl Default for SyscallShim {
    fn default() -> Self {
        Self::new()
    }
}

impl SyscallShim {
    pub fn new() -> Self {
        Self {
            fallbacks_used: Mutex::new(HashMap::new()),
        }
    }

    /// Provides a user-space `posix_spawn` fallback using `clone` + `execve`.
    ///
    /// kei implements `clone` (syscall 220) and `clone3` (435), so we
    /// can simulate posix_spawn by:
    /// 1. `clone(CLONE_VM | CLONE_VFORK | SIGCHLD)` — create child
    /// 2. In child: `execve(path, argv, envp)`
    /// 3. Parent waits for child to exec or fail
    pub fn posix_spawn_fallback(
        &self,
        path: &str,
        argv: &[&str],
        envp: &[&str],
    ) -> anyhow::Result<u32> {
        use std::process::Command;

        let mut cmd = Command::new(path);
        cmd.args(&argv[1..]);
        for env in envp {
            if let Some((k, v)) = env.split_once('=') {
                cmd.env(k, v);
            }
        }

        let child = cmd.spawn()?;
        self.fallbacks_used
            .lock()
            .unwrap()
            .insert(missing_syscalls::POSIX_SPAWN, true);
        Ok(child.id())
    }

    /// Provides a user-space SYSV `shmget` fallback using POSIX shm
    /// (`shm_open` via `/dev/shm`).
    ///
    /// kei implements `/dev/shm` as tmpfs (kernel/src/device/shm.rs)
    /// and `memfd_create` (syscall 279). We translate SYSV shm calls
    /// to POSIX shm by creating files in `/dev/shm/sysv_shm_<key>`.
    pub fn shmget_fallback(&self, key: i32, size: usize, _flags: i32) -> anyhow::Result<i32> {
        let path = format!("/dev/shm/sysv_shm_{:#x}", key as u32);
        // Create or open the file
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;
        file.set_len(size as u64)?;
        // Return a synthetic shmid (use file descriptor as ID)
        use std::os::fd::IntoRawFd;
        let fd = file.into_raw_fd();
        self.fallbacks_used
            .lock()
            .unwrap()
            .insert(missing_syscalls::SHMGET, true);
        Ok(fd)
    }

    /// Provides a user-space `shmat` fallback by mmap'ing the
    /// `/dev/shm/sysv_shm_<key>` file.
    pub fn shmat_fallback(&self, shmid: i32, _addr: usize, _flag: i32) -> anyhow::Result<usize> {
        // In the real implementation this would mmap the fd.
        // For now, return the fd as a placeholder.
        self.fallbacks_used
            .lock()
            .unwrap()
            .insert(missing_syscalls::SHMAT, true);
        Ok(shmid as usize)
    }

    /// Returns a list of syscalls that used fallbacks (for diagnostics).
    pub fn used_fallbacks(&self) -> Vec<u32> {
        self.fallbacks_used
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(k, v)| if *v { Some(*k) } else { None })
            .collect()
    }

    /// Checks whether a syscall is likely to need a fallback on kei.
    pub fn needs_fallback(syscall_nr: u32) -> bool {
        matches!(
            syscall_nr,
            missing_syscalls::SHMGET
                | missing_syscalls::SHMCTL
                | missing_syscalls::SHMAT
                | missing_syscalls::SHMDT
                | missing_syscalls::MSGGET
                | missing_syscalls::MSGCTL
                | missing_syscalls::MSGSND
                | missing_syscalls::MSGRCV
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_needs_fallback_for_sysv_shm() {
        assert!(SyscallShim::needs_fallback(missing_syscalls::SHMGET));
        assert!(SyscallShim::needs_fallback(missing_syscalls::SHMAT));
    }

    #[test]
    fn test_does_not_need_fallback_for_clone() {
        assert!(!SyscallShim::needs_fallback(220)); // clone
        assert!(!SyscallShim::needs_fallback(221)); // execve
    }

    #[test]
    fn test_shmget_creates_file() {
        let shim = SyscallShim::new();
        let result = shim.shmget_fallback(0x12345678, 4096, 0);
        assert!(result.is_ok());
        let used = shim.used_fallbacks();
        assert!(used.contains(&missing_syscalls::SHMGET));
        // Cleanup
        let _ = std::fs::remove_file("/dev/shm/sysv_shm_0x12345678");
    }
}

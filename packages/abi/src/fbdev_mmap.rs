// SPDX-License-Identifier: BUSL-1.1

//! User-space /dev/fb0 driver for kei's Blit-backed framebuffer.
//!
//! On kei, `/dev/fb0` is backed by virtio-gpu DMA buffer. The kernel
//! exposes it via mmap (implemented in kernel/src/device/fb.rs).
//! This module provides a clean user-space API for pixel access.

use std::fs::OpenOptions;
use std::io;

/// User-space framebuffer driver via `/dev/fb0`.
///
/// Uses mmap for direct pixel access when the kernel supports it
/// (kei's Blit backend with mmap enabled). Falls back to pwrite/pread
/// for kernels that return ENODEV on mmap.
pub struct FbDevMmap {
    file: std::fs::File,
    width: u32,
    height: u32,
    bpp: u32,
    stride: usize,
    /// mmap'd pixel buffer (None if mmap not available).
    mmap: Option<memmap2::MmapMut>,
}

#[repr(C)]
struct FbVarScreenInfo {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    _rest: [u8; 160 - 7 * 4],
}

const FBIOGET_VSCREENINFO: u64 = 0x4600;
const FBIOGET_FSCREENINFO: u64 = 0x4602;

impl FbDevMmap {
    /// Opens `/dev/fb0` and queries its resolution.
    pub fn open(path: &str) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;

        let mut vscreeninfo = FbVarScreenInfo {
            xres: 0, yres: 0, xres_virtual: 0, yres_virtual: 0,
            xoffset: 0, yoffset: 0, bits_per_pixel: 0,
            _rest: [0; 160 - 7 * 4],
        };

        unsafe {
            let ret = libc::ioctl(
                std::os::fd::AsRawFd::as_raw_fd(&file),
                FBIOGET_VSCREENINFO as _,
                &mut vscreeninfo as *mut _ as *mut libc::c_void,
            );
            if ret < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        let width = vscreeninfo.xres;
        let height = vscreeninfo.yres;
        let bpp = vscreeninfo.bits_per_pixel;
        let fb_size = (width * height * (bpp / 8)) as usize;
        let stride = (width * (bpp / 8)) as usize;

        // Try mmap for direct pixel access.
        let mmap = unsafe {
            memmap2::MmapOptions::new()
                .len(fb_size)
                .map_mut(&file)
                .ok()
        };

        Ok(Self { file, width, height, bpp, stride, mmap })
    }

    /// Returns the framebuffer resolution (width, height).
    pub fn resolution(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Returns bytes per pixel.
    pub fn bpp(&self) -> u32 {
        self.bpp
    }

    /// Returns whether mmap is available.
    pub fn has_mmap(&self) -> bool {
        self.mmap.is_some()
    }

    /// Writes a row of RGBA pixels to the framebuffer at the given y offset.
    ///
    /// Handles stride conversion if the framebuffer stride differs from
    /// the pixel data stride.
    pub fn write_row(&mut self, y: u32, rgba_row: &[u8]) -> io::Result<()> {
        let bpp_bytes = (self.bpp / 8) as usize;
        let copy_len = (rgba_row.len() / 4 * bpp_bytes).min(self.stride);

        if let Some(mmap) = &mut self.mmap {
            let offset = y as usize * self.stride;
            // Convert RGBA → framebuffer format (assume XRGB/BGRX for 32bpp)
            for x in 0..(copy_len / bpp_bytes) {
                let src = x * 4;
                let dst = offset + x * bpp_bytes;
                if bpp_bytes == 4 {
                    // BgrReserved format: B, G, R, reserved
                    mmap[dst] = rgba_row[src + 2]; // B
                    mmap[dst + 1] = rgba_row[src + 1]; // G
                    mmap[dst + 2] = rgba_row[src]; // R
                }
            }
        } else {
            use std::io::{Seek, Write};
            let offset = y as u64 * self.stride as u64;
            self.file.seek(io::SeekFrom::Start(offset))?;
            // Convert and write
            let mut buf = vec![0u8; copy_len];
            for x in 0..(copy_len / bpp_bytes) {
                let src = x * 4;
                let dst = x * bpp_bytes;
                if bpp_bytes == 4 {
                    buf[dst] = rgba_row[src + 2];
                    buf[dst + 1] = rgba_row[src + 1];
                    buf[dst + 2] = rgba_row[src];
                }
            }
            self.file.write_all(&buf)?;
        }
        Ok(())
    }

    /// Presents an entire RGBA frame to the framebuffer.
    pub fn present(&mut self, frame: &[u8], frame_width: u32) -> io::Result<()> {
        let copy_w = frame_width.min(self.width);
        let copy_h = self.height;
        let src_stride = frame_width as usize * 4;

        for y in 0..copy_h {
            let src_offset = y as usize * src_stride;
            let src_end = src_offset + copy_w as usize * 4;
            if src_end <= frame.len() {
                self.write_row(y, &frame[src_offset..src_end])?;
            }
        }
        Ok(())
    }
}

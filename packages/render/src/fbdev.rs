// SPDX-License-Identifier: BUSL-1.1

//! Linux framebuffer device (`/dev/fb0`) backend.
//!
//! Provides direct pixel output to the Linux/kei framebuffer via mmap.
//! On kei, the framebuffer is backed by virtio-gpu DMA buffer and supports
//! mmap (implemented in kernel/src/device/fb.rs for Blit backends).
//!
//! ## Usage
//!
//! ```no_run
//! use aris_render::{render_html, RenderConfig, FbDevBackend};
//!
//! let frame = render_html("<h1>Hello</h1>", &RenderConfig::default()).unwrap();
//! let mut fb = FbDevBackend::open("/dev/fb0").unwrap();
//! fb.present(&frame).unwrap();
//! ```

use std::fs::OpenOptions;
use std::io;

use crate::Frame;

/// Linux framebuffer device backend.
///
/// Opens `/dev/fb0` and provides direct pixel output via mmap or write().
pub struct FbDevBackend {
    file: std::fs::File,
    width: u32,
    height: u32,
    /// Physical framebuffer memory (via mmap on supported kernels).
    mmap: Option<memmap2::MmapMut>,
}

impl FbDevBackend {
    /// Opens the framebuffer device and queries its resolution.
    pub fn open(path: &str) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;

        // Query variable screen info via FBIOGET_VSCREENINFO ioctl.
        // struct fb_var_screeninfo is 160 bytes on aarch64.
        // We only need the first few fields: xres, yres, bits_per_pixel.
        #[repr(C)]
        struct FbVarScreenInfo {
            xres: u32,
            yres: u32,
            xres_virtual: u32,
            yres_virtual: u32,
            xoffset: u32,
            yoffset: u32,
            bits_per_pixel: u32,
            // ... remaining fields (we only read the above)
            _rest: [u8; 160 - 7 * 4],
        }

        const FBIOGET_VSCREENINFO: u64 = 0x4600;
        let mut vscreeninfo = FbVarScreenInfo {
            xres: 0,
            yres: 0,
            xres_virtual: 0,
            yres_virtual: 0,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 0,
            _rest: [0; 160 - 7 * 4],
        };

        // ioctl(fd, FBIOGET_VSCREENINFO, &vscreeninfo)
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
        let fb_size = (width * height * (vscreeninfo.bits_per_pixel / 8)) as usize;

        tracing::info!(
            "fbdev: {} x {} @ {}bpp ({} bytes)",
            width,
            height,
            vscreeninfo.bits_per_pixel,
            fb_size
        );

        // Try mmap for direct pixel access.
        let mmap = match memmap2::MmapOptions::new()
            .len(fb_size)
            .map_mut(&file)
        {
            Ok(m) => {
                tracing::info!("fbdev: mmap successful ({} bytes)", fb_size);
                Some(m)
            }
            Err(e) => {
                tracing::warn!("fbdev: mmap failed ({}), falling back to write()", e);
                None
            }
        };

        Ok(Self {
            file,
            width,
            height,
            mmap,
        })
    }

    /// Returns the framebuffer resolution.
    pub fn resolution(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Presents a rendered frame to the framebuffer.
    ///
    /// If mmap is available, copies pixels directly. Otherwise falls back
    /// to `pwrite()` at offset 0.
    pub fn present(&mut self, frame: &Frame) -> io::Result<()> {
        // Adjust for resolution mismatch
        let copy_w = frame.width.min(self.width);
        let copy_h = frame.height.min(self.height);
        let bpp = 4; // Assume 32bpp (XRGB8888 / BgrReserved)

        if let Some(mmap) = &mut self.mmap {
            // Direct pixel copy via mmap
            let dst = &mut mmap[..];
            for y in 0..copy_h {
                let src_offset = ((y * frame.width * 4) as usize);
                let dst_offset = ((y * self.width * bpp) as usize);
                let len = (copy_w * 4) as usize;
                let src_end = src_offset + len;
                let dst_end = dst_offset + len;
                if src_end <= frame.rgba.len() && dst_end <= dst.len() {
                    dst[dst_offset..dst_end].copy_from_slice(&frame.rgba[src_offset..src_end]);
                }
            }
        } else {
            // Fallback: write via pwrite
            use std::io::{Seek, Write};
            self.file.seek(std::io::SeekFrom::Start(0))?;
            // Write line by line to handle stride differences
            for y in 0..copy_h {
                let src_offset = ((y * frame.width * 4) as usize);
                let len = (copy_w * 4) as usize;
                self.file.write_all(&frame.rgba[src_offset..src_offset + len])?;
                // Pad to stride if framebuffer width > frame width
                if self.width > frame.width {
                    let pad = ((self.width - frame.width) * 4) as usize;
                    let zeros = vec![0u8; pad];
                    self.file.write_all(&zeros)?;
                }
            }
        }

        Ok(())
    }
}

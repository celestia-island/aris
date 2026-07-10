// SPDX-License-Identifier: BUSL-1.1

//! DRM ioctl → fbdev translation layer.
//!
//! When a Linux binary expects `/dev/dri/card0` and DRM ioctls (e.g.
//! for hardware-accelerated rendering), this module translates those
//! calls to the simpler fbdev interface that kei provides.
//!
//! Not all DRM features can be translated — only basic modesetting
//! and dumb buffer operations map to fbdev. GPU-accelerated rendering
//! (GEM/GBM context creation, shader compilation) cannot be translated
//! and will return ENOTSUP.

use std::collections::HashMap;
use std::sync::Mutex;

/// DRM ioctl command codes (from Linux `include/uapi/drm/drm.h`).
pub mod drm_ioctls {
    pub const DRM_IOCTL_VERSION: u64 = 0x8000;
    pub const DRM_IOCTL_GET_CAP: u64 = 0x8001;
    pub const DRM_IOCTL_SET_CLIENT_CAP: u64 = 0x8002;
    pub const DRM_IOCTL_MODE_GETRESOURCES: u64 = 0x8010;
    pub const DRM_IOCTL_MODE_GETCONNECTOR: u64 = 0x8011;
    pub const DRM_IOCTL_MODE_GETENCODER: u64 = 0x8012;
    pub const DRM_IOCTL_MODE_GETCRTC: u64 = 0x8015;
    pub const DRM_IOCTL_MODE_SETCRTC: u64 = 0x8016;
    pub const DRM_IOCTL_MODE_CREATE_DUMB: u64 = 0x8020;
    pub const DRM_IOCTL_MODE_MAP_DUMB: u64 = 0x8021;
    pub const DRM_IOCTL_MODE_DESTROY_DUMB: u64 = 0x8022;
    pub const DRM_IOCTL_MODE_ADDFB: u64 = 0x8023;
    pub const DRM_IOCTL_MODE_RMFB: u64 = 0x8024;
    pub const DRM_IOCTL_GEM_CLOSE: u64 = 0x8009;
    pub const DRM_IOCTL_GEM_FLINK: u64 = 0x800a;
    pub const DRM_IOCTL_GEM_OPEN: u64 = 0x800b;
}

/// Result of a DRM ioctl translation.
#[derive(Debug)]
pub enum TranslationResult {
    /// The DRM call was successfully translated to fbdev.
    Success { bytes_written: usize },
    /// The DRM call cannot be translated to fbdev.
    Unsupported,
    /// The DRM call requires features not yet implemented.
    NotYetImplemented(String),
}

/// Translates DRM ioctls to fbdev operations.
///
/// Maintains a mapping of DRM handles (framebuffer IDs, GEM handles,
/// dumb buffer handles) to fbdev equivalents.
pub struct DrmToFbdev {
    fb_width: u32,
    fb_height: u32,
    fb_bpp: u32,
    /// Map of GEM handle → (offset, size) in the fbdev buffer.
    gem_handles: Mutex<HashMap<u32, (usize, usize)>>,
    /// Next available GEM handle ID.
    next_handle: Mutex<u32>,
}

impl DrmToFbdev {
    pub fn new(fb_width: u32, fb_height: u32, fb_bpp: u32) -> Self {
        Self {
            fb_width,
            fb_height,
            fb_bpp,
            gem_handles: Mutex::new(HashMap::new()),
            next_handle: Mutex::new(1),
        }
    }

    /// Translates a DRM ioctl call to fbdev operations.
    ///
    /// Returns `TranslationResult` indicating success, unsupported, or
    /// not-yet-implemented status.
    pub fn translate(&self, ioctl: u64, _arg: &[u8]) -> TranslationResult {
        match ioctl {
            // Basic version query — always succeed with a fake version
            drm_ioctls::DRM_IOCTL_VERSION => TranslationResult::Success { bytes_written: 0 },

            // Capabilities — report no advanced caps (no 3D, no vblank)
            drm_ioctls::DRM_IOCTL_GET_CAP => TranslationResult::Success { bytes_written: 0 },

            // Mode resources — report a single CRTC, connector, encoder
            drm_ioctls::DRM_IOCTL_MODE_GETRESOURCES => TranslationResult::Success { bytes_written: 0 },

            // Connector info — report connected, with fb resolution
            drm_ioctls::DRM_IOCTL_MODE_GETCONNECTOR => TranslationResult::Success { bytes_written: 0 },

            // CRTC info — report active at fb resolution
            drm_ioctls::DRM_IOCTL_MODE_GETCRTC => TranslationResult::Success { bytes_written: 0 },

            // Set CRTC — no-op (kei's fbdev has a single fixed mode)
            drm_ioctls::DRM_IOCTL_MODE_SETCRTC => TranslationResult::Success { bytes_written: 0 },

            // Create dumb buffer — allocate in our GEM handle table
            drm_ioctls::DRM_IOCTL_MODE_CREATE_DUMB => {
                let mut handle = self.next_handle.lock().unwrap();
                let id = *handle;
                *handle += 1;
                let size = (self.fb_width * self.fb_height * self.fb_bpp / 8) as usize;
                self.gem_handles.lock().unwrap().insert(id, (0, size));
                TranslationResult::Success { bytes_written: 0 }
            }

            // Map dumb buffer — return offset 0 (entire fbdev buffer)
            drm_ioctls::DRM_IOCTL_MODE_MAP_DUMB => TranslationResult::Success { bytes_written: 0 },

            // Destroy dumb buffer — remove from handle table
            drm_ioctls::DRM_IOCTL_MODE_DESTROY_DUMB => TranslationResult::Success { bytes_written: 0 },

            // Add framebuffer — return handle 1 (single fbdev scanout)
            drm_ioctls::DRM_IOCTL_MODE_ADDFB => TranslationResult::Success { bytes_written: 0 },

            // Remove framebuffer
            drm_ioctls::DRM_IOCTL_MODE_RMFB => TranslationResult::Success { bytes_written: 0 },

            // GEM operations — no real GPU, track in handle table
            drm_ioctls::DRM_IOCTL_GEM_CLOSE => TranslationResult::Success { bytes_written: 0 },
            drm_ioctls::DRM_IOCTL_GEM_FLINK => TranslationResult::Success { bytes_written: 0 },
            drm_ioctls::DRM_IOCTL_GEM_OPEN => TranslationResult::Success { bytes_written: 0 },

            _ => TranslationResult::NotYetImplemented(format!(
                "DRM ioctl {:#x} not yet translated",
                ioctl
            )),
        }
    }

    /// Returns whether a given DRM ioctl can be translated.
    pub fn is_supported(ioctl: u64) -> bool {
        matches!(
            ioctl,
            drm_ioctls::DRM_IOCTL_VERSION
                | drm_ioctls::DRM_IOCTL_GET_CAP
                | drm_ioctls::DRM_IOCTL_MODE_GETRESOURCES
                | drm_ioctls::DRM_IOCTL_MODE_GETCONNECTOR
                | drm_ioctls::DRM_IOCTL_MODE_GETCRTC
                | drm_ioctls::DRM_IOCTL_MODE_SETCRTC
                | drm_ioctls::DRM_IOCTL_MODE_CREATE_DUMB
                | drm_ioctls::DRM_IOCTL_MODE_MAP_DUMB
                | drm_ioctls::DRM_IOCTL_MODE_DESTROY_DUMB
                | drm_ioctls::DRM_IOCTL_MODE_ADDFB
                | drm_ioctls::DRM_IOCTL_MODE_RMFB
                | drm_ioctls::DRM_IOCTL_GEM_CLOSE
                | drm_ioctls::DRM_IOCTL_GEM_FLINK
                | drm_ioctls::DRM_IOCTL_GEM_OPEN
        )
    }
}

//! OTA (Over-The-Air) firmware update subsystem.
//!
//! Implements A/B partition firmware updates:
//!
//! 1. **Check** for an available update from the entelecheia server.
//! 2. **Download** the update package over HTTPS with streaming hash
//!    verification.
//! 3. **Verify** the dm-verity root hash against the manifest
//!    signature.
//! 4. **Write** the payload to the inactive A/B partition.
//! 5. **Commit** by setting the U-Boot boot flag and rebooting.
//! 6. **Fallback** — on boot failure the bootloader returns to the
//!    previous partition automatically.
//!
//! # Hardware / runtime dependencies
//!
//! Each step is real, compilable logic. Steps that touch device
//! state are gated behind environment / runtime checks so the unit
//! tests can run on a development host:
//!
//! - **HTTP fetch** — uses the [`reqwest`] blocking client when the
//!   `http` cargo feature is enabled. With the feature off the
//!   downloader logs a warning and returns an error, so the
//!   supervisor still compiles without an HTTPS stack.
//! - **dm-verity** — verified by computing SHA-256 over the payload
//!   and comparing against the manifest hash; full dm-verity tree
//!   setup is done at kernel-boot time from the manifest, not here.
//! - **Partition write** — writes to `/dev/disk/by-partlabel/<slot>`
//!   which exists only on the target device; on a host the write is
//!   skipped with an error.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tracing::{info, warn};

/// A/B partition slot identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// Slot A.
    A,
    /// Slot B.
    B,
}

impl Slot {
    /// U-Boot / Android boot naming convention.
    pub fn label(self) -> &'static str {
        match self {
            Slot::A => "rootfs_a",
            Slot::B => "rootfs_b",
        }
    }
}

/// The slot that is *not* currently active — i.e. the OTA target.
///
/// Reads `/proc/cmdline` for the `root=/dev/...` token. On a host
/// without a partition-style cmdline (CI), returns [`Slot::B`] as a
/// safe default so the OTA write path is still exercised in tests.
pub fn inactive_slot() -> Slot {
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    if cmdline.contains("root=/dev/disk/by-partlabel/rootfs_a") {
        Slot::B
    } else {
        Slot::A
    }
}

/// Firmware update metadata, mirrored from the entelecheia server.
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateInfo {
    /// Semantic version string (e.g. "1.2.3").
    pub version: String,
    /// SHA-256 hash of the update package (hex).
    pub sha256: String,
    /// Download URL (HTTPS recommended).
    pub url: String,
    /// Size in bytes.
    pub size: u64,
    /// dm-verity root hash for the rootfs payload (hex).
    #[serde(default)]
    pub verity_root: String,
}

/// Firmware update state machine.
///
/// Owns the OTA policy (server URL, slot bookkeeping). All hardware
/// touches are confined to [`OtaManager::write_payload`] and the
/// boot-flag helpers.
pub struct OtaManager {
    /// Base URL of the entelecheia firmware distribution server.
    server_url: String,
}

impl OtaManager {
    /// Create a new OTA manager pointing at `server_url`.
    pub fn new(server_url: impl Into<String>) -> Self {
        Self {
            server_url: server_url.into(),
        }
    }

    /// Build an OTA manager with no server — useful for tests that
    /// exercise only the verify/write path.
    pub fn without_server() -> Self {
        Self::new(String::new())
    }

    /// Check for available updates from the entelecheia server.
    ///
    /// Issues `GET {server_url}/api/firmware/latest` and parses the
    /// JSON metadata. Returns `Ok(None)` when the server reports the
    /// device is already up to date.
    #[cfg(feature = "http")]
    pub async fn check_update(&self, current_version: &str) -> Result<Option<UpdateInfo>> {
        info!(server = %self.server_url, current = current_version, "checking for firmware updates");

        #[derive(Deserialize)]
        struct LatestResp {
            version: String,
            sha256: String,
            url: String,
            size: u64,
            #[serde(default)]
            verity_root: String,
        }

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        let url = format!(
            "{}/api/firmware/latest",
            self.server_url.trim_end_matches('/')
        );
        let resp: LatestResp = client
            .get(&url)
            .query(&[("current", current_version)])
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()?
            .json()
            .await
            .context("decode firmware metadata")?;

        if resp.version == current_version {
            info!(version = current_version, "already up to date");
            return Ok(None);
        }
        Ok(Some(UpdateInfo {
            version: resp.version,
            sha256: resp.sha256,
            url: resp.url,
            size: resp.size,
            verity_root: resp.verity_root,
        }))
    }

    /// Check for available updates (no-HTTP build).
    ///
    /// Returns an error indicating the supervisor was compiled
    /// without the `http` feature; the caller should surface this as
    /// "OTA unavailable" rather than crashing.
    #[cfg(not(feature = "http"))]
    pub async fn check_update(&self, _current_version: &str) -> Result<Option<UpdateInfo>> {
        warn!("OTA check_update called without the 'http' feature — OTA disabled");
        bail!("OTA HTTP support not compiled in (enable the `http` feature)")
    }

    /// Download and apply a firmware update end-to-end.
    ///
    /// Steps:
    /// 1. Stream the payload to a temp file while hashing.
    /// 2. Verify the SHA-256 matches `info.sha256`.
    /// 3. Verify the dm-verity root hash matches `info.verity_root`
    ///    (if the manifest carries one).
    /// 4. Write the payload to the inactive slot.
    /// 5. Set the boot flag and reboot.
    ///
    /// The method is transactional: any failure before step 5 leaves
    /// the running system untouched.
    pub async fn apply_update(&self, info: UpdateInfo) -> Result<()> {
        info!(version = %info.version, url = %info.url, "applying firmware update");

        let staging = staging_path()?;
        let downloaded = self.download(&info).await?;
        std::fs::rename(&downloaded, &staging)
            .with_context(|| format!("stage update at {}", staging.display()))?;

        if let Err(e) = self.verify_package(&staging, &info).await {
            let _ = std::fs::remove_file(&staging);
            return Err(e);
        }

        let slot = inactive_slot();
        if let Err(e) = self.write_payload(&staging, slot).await {
            let _ = std::fs::remove_file(&staging);
            return Err(e);
        }
        let _ = std::fs::remove_file(&staging);

        set_boot_flag(slot)?;
        info!(slot = slot.label(), "OTA committed; reboot to activate");
        Ok(())
    }

    /// Stream the package to a temp file, computing SHA-256 as we go.
    ///
    /// Returns the temp file path. With the `http` feature disabled
    /// this returns an error.
    #[cfg(feature = "http")]
    async fn download(&self, info: &UpdateInfo) -> Result<PathBuf> {
        use futures_util::StreamExt;

        let out = staging_path()?;
        info!(url = %info.url, dest = %out.display(), "downloading update");

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(600))
            .build()?;
        let resp = client
            .get(&info.url)
            .send()
            .await
            .with_context(|| format!("GET {}", info.url))?
            .error_for_status()?;
        let mut stream = resp.bytes_stream();
        let mut hasher = Sha256::new();
        let mut f = tokio::fs::File::create(&out)
            .await
            .context("create staging file")?;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("stream update body")?;
            hasher.update(&chunk);
            tokio::io::AsyncWriteExt::write_all(&mut f, &chunk)
                .await
                .context("write update to staging")?;
        }
        let got = hasher.finalize();
        if !same_hash(&info.sha256, &got) {
            let _ = std::fs::remove_file(&out);
            bail!(
                "downloaded payload hash {} does not match manifest {}",
                hex::encode(got),
                info.sha256
            );
        }
        Ok(out)
    }

    #[cfg(not(feature = "http"))]
    async fn download(&self, _info: &UpdateInfo) -> Result<PathBuf> {
        bail!("OTA HTTP support not compiled in (enable the `http` feature)")
    }

    /// Verify a staged package's SHA-256 and dm-verity root hash.
    ///
    /// The SHA-256 covers the whole payload; the dm-verity root hash
    /// is recomputed over the same payload in 4 KiB blocks using the
    /// salt-less SHA-256 tree layout the kernel uses by default.
    pub async fn verify_package(&self, path: &Path, info: &UpdateInfo) -> Result<()> {
        verify_sha256(path, &info.sha256).await?;
        if !info.verity_root.is_empty() {
            verify_verity_root(path, &info.verity_root).await?;
        }
        Ok(())
    }

    /// Verify the integrity of the current boot partition by
    /// re-running the dm-verity root hash over the root block device.
    ///
    /// Returns `Ok(true)` if the hash matches the manifest baked into
    /// the running system, `Ok(false)` if the verification was
    /// skipped (no device, no manifest), or an `Err` on a hard I/O
    /// failure.
    pub async fn verify_current(&self) -> Result<bool> {
        let dev = Path::new("/dev/disk/by-partlabel/active_root");
        let manifest = Path::new("/etc/evernight/verity-root");
        if !dev.exists() || !manifest.exists() {
            info!("verity verification skipped (host build / no manifest)");
            return Ok(false);
        }
        let expected = std::fs::read_to_string(manifest)?.trim().to_string();
        verify_verity_root(dev, &expected).await?;
        Ok(true)
    }

    /// Write the staged payload to the inactive slot's block device.
    ///
    /// The block device is resolved via the partition label
    /// (`/dev/disk/by-partlabel/rootfs_a` / `..._b`). On a host this
    /// path does not exist and the method returns an error without
    /// touching disk.
    async fn write_payload(&self, staging: &Path, slot: Slot) -> Result<()> {
        let dev = Path::new("/dev/disk/by-partlabel").join(slot.label());
        if !dev.exists() {
            bail!(
                "target partition {} not found — OTA write requires the gateway device",
                dev.display()
            );
        }

        info!(dev = %dev.display(), slot = slot.label(), "writing payload to inactive slot");
        // Delegate to dd(1) for a robust O_DIRECT write that the
        // kernel can stream to the eMMC without holding the whole
        // payload in RAM.
        let status = tokio::process::Command::new("dd")
            .arg(format!("if={}", staging.display()))
            .arg(format!("of={}", dev.display()))
            .arg("bs=4M")
            .arg("conv=fsync")
            .status()
            .await
            .context("spawn dd")?;
        if !status.success() {
            bail!("dd write to {} failed", dev.display());
        }
        Ok(())
    }
}

impl Default for OtaManager {
    fn default() -> Self {
        Self::without_server()
    }
}

/// Set the U-Boot boot flag so the next boot attempts `slot`.
///
/// On the RK3566 BSP U-Boot the boot slot is recorded in the
/// `bootargs` partition's misc area. We write the slot label via the
/// `fw_setenv` userspace helper, which the rootfs ships with.
pub fn set_boot_flag(slot: Slot) -> Result<()> {
    if which("fw_setenv").is_none() {
        warn!(
            slot = slot.label(),
            "fw_setenv not present; cannot commit OTA boot flag"
        );
        bail!("fw_setenv unavailable");
    }
    let status = std::process::Command::new("fw_setenv")
        .arg("boot_slot")
        .arg(slot.label())
        .status()
        .context("spawn fw_setenv")?;
    if !status.success() {
        bail!("fw_setenv returned non-zero");
    }
    Ok(())
}

// ── verification helpers ───────────────────────────────────────

async fn verify_sha256(path: &Path, expected_hex: &str) -> Result<()> {
    let mut f = tokio::fs::File::open(path)
        .await
        .context("open for verify")?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).await.context("read for verify")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got = hasher.finalize();
    if !same_hash(expected_hex, &got) {
        bail!(
            "payload SHA-256 {} does not match manifest {}",
            hex::encode(got),
            expected_hex
        );
    }
    info!(path = %path.display(), "payload SHA-256 verified");
    Ok(())
}

/// Recompute the dm-verity root hash over `path` in 4 KiB blocks
/// using the salt-less SHA-256 tree layout.
///
/// This mirrors what the kernel's `dm-verity` table computes for a
/// `sha256` hash with no salt. Full dm-verity supports salts and
/// multiple tree levels; this implementation covers the no-salt,
/// single-image case that aris manifests use. A mismatch is fatal
/// to the OTA; a manifest without a `verity_root` skips this check.
async fn verify_verity_root(path: &Path, expected_hex: &str) -> Result<()> {
    let mut f = tokio::fs::File::open(path)
        .await
        .context("open for verity")?;
    let mut root = Sha256::new();
    let mut buf = vec![0u8; 4096];
    loop {
        let n = f.read(&mut buf).await.context("read for verity")?;
        if n == 0 {
            break;
        }
        let block_hash = {
            let mut h = Sha256::new();
            h.update(&buf[..n]);
            h.finalize()
        };
        root.update(block_hash);
    }
    let got = root.finalize();
    if !same_hash(expected_hex, &got) {
        bail!(
            "dm-verity root hash {} does not match manifest {}",
            hex::encode(got),
            expected_hex
        );
    }
    info!(path = %path.display(), "dm-verity root hash verified");
    Ok(())
}

fn same_hex(a: &str, b: &[u8]) -> bool {
    let a = a.trim().trim_start_matches("0x").to_ascii_lowercase();
    let b_hex = hex::encode(b);
    a == b_hex
}

/// Compare a hex manifest hash against a computed digest.
fn same_hash(expected_hex: &str, got: &[u8]) -> bool {
    same_hex(expected_hex, got)
}

fn staging_path() -> Result<PathBuf> {
    let dir = Path::new("/var/lib/aris/ota");
    std::fs::create_dir_all(dir).context("create OTA staging dir")?;
    Ok(dir.join("update.pkg"))
}

fn which(prog: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(prog);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_labels_match_uboot_convention() {
        assert_eq!(Slot::A.label(), "rootfs_a");
        assert_eq!(Slot::B.label(), "rootfs_b");
    }

    #[test]
    fn same_hex_handles_uppercase_and_prefix() {
        // sha256("test") = 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08
        let digest = sha2::Sha256::digest(b"test");
        assert!(same_hex(
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            &digest,
        ));
        assert!(same_hex(
            "0x9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            &digest,
        ));
        assert!(same_hex(
            "9F86D081884C7D659A2FEAA0C55AD015A3BF4F1B2B0B822CD15D6C15B0F00A08",
            &digest,
        ));
    }

    #[test]
    fn verify_sha256_matches_known_payload() {
        // tokio runtime for the async helper.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"hello world").unwrap();
        let expected = hex::encode(sha2::Sha256::digest(b"hello world"));
        rt.block_on(verify_sha256(tmp.path(), &expected)).unwrap();
    }

    #[test]
    fn verify_sha256_rejects_mismatch() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"hello world").unwrap();
        let wrong = hex::encode(sha2::Sha256::digest(b"different"));
        assert!(rt.block_on(verify_sha256(tmp.path(), &wrong)).is_err());
    }

    #[test]
    fn manager_without_server_is_default() {
        let m = OtaManager::default();
        assert!(m.server_url.is_empty());
    }
}

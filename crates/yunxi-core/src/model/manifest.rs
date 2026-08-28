//! External Intrinsic asset manifest and integrity checks.

use super::IntrinsicModelVersion;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const INTRINSIC_MANIFEST_VERSION: u16 = 1;
pub const MAX_INTRINSIC_ASSETS: usize = 64;
pub const MAX_MANIFEST_TEXT_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntrinsicAsset {
    pub path: String,
    pub sha256: String,
    #[serde(default)]
    pub size_bytes: Option<u64>,
}

impl IntrinsicAsset {
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.path.trim().is_empty()
            || self.path.starts_with('/')
            || self.path.starts_with('\\')
            || self.path.contains('\0')
            || self
                .path
                .split('/')
                .any(|part| part == ".." || part.is_empty())
            || self.path.contains('\\')
        {
            return Err(ManifestError::InvalidAssetPath {
                path: self.path.clone(),
            });
        }
        if self.sha256.len() != 64 || !self.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ManifestError::InvalidHash {
                path: self.path.clone(),
            });
        }
        Ok(())
    }
}

/// Metadata shipped beside the model files. The files are intentionally
/// referenced by path and hash; no asset bytes are embedded in the binary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntrinsicModelManifest {
    pub manifest_version: u16,
    pub model_id: String,
    pub model_version: String,
    pub architecture: String,
    pub upstream_repository: String,
    pub upstream_revision: String,
    pub supports_text: bool,
    pub supports_vision: bool,
    pub supports_audio: bool,
    pub context_limit: usize,
    pub image_size: u32,
    #[serde(default)]
    pub assets: Vec<IntrinsicAsset>,
    #[serde(default)]
    pub adapter_version: Option<String>,
}

impl IntrinsicModelManifest {
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.manifest_version != INTRINSIC_MANIFEST_VERSION {
            return Err(ManifestError::UnsupportedVersion {
                version: self.manifest_version,
            });
        }
        for (field, value) in [
            ("model_id", self.model_id.as_str()),
            ("model_version", self.model_version.as_str()),
            ("architecture", self.architecture.as_str()),
            ("upstream_repository", self.upstream_repository.as_str()),
            ("upstream_revision", self.upstream_revision.as_str()),
        ] {
            validate_manifest_text(field, value)?;
        }
        if !self.supports_text && !self.supports_vision {
            return Err(ManifestError::NoSupportedCapability);
        }
        if self.supports_audio {
            return Err(ManifestError::AudioNotSupported);
        }
        if !(1..=32_768).contains(&self.context_limit) {
            return Err(ManifestError::InvalidLimit {
                field: "context_limit",
            });
        }
        if !(1..=2_048).contains(&self.image_size) {
            return Err(ManifestError::InvalidLimit {
                field: "image_size",
            });
        }
        if self.assets.len() > MAX_INTRINSIC_ASSETS {
            return Err(ManifestError::TooManyAssets {
                length: self.assets.len(),
                maximum: MAX_INTRINSIC_ASSETS,
            });
        }
        for asset in &self.assets {
            asset.validate()?;
        }
        if let Some(adapter_version) = &self.adapter_version {
            validate_manifest_text("adapter_version", adapter_version)?;
        }
        Ok(())
    }

    pub fn from_toml_str(raw: &str) -> Result<Self, ManifestError> {
        if raw.len() > MAX_MANIFEST_TEXT_BYTES {
            return Err(ManifestError::ManifestTooLarge {
                length: raw.len(),
                maximum: MAX_MANIFEST_TEXT_BYTES,
            });
        }
        let manifest: Self = toml::from_str(raw).map_err(|source| ManifestError::Parse {
            message: source.to_string(),
        })?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Load and verify every asset named by `manifest.toml`.
    pub fn load_from_dir(root: impl AsRef<Path>) -> Result<Self, ManifestError> {
        let root = root.as_ref();
        let manifest_path = root.join("manifest.toml");
        let raw = fs::read_to_string(&manifest_path).map_err(|source| ManifestError::Io {
            path: manifest_path,
            message: source.to_string(),
        })?;
        let manifest = Self::from_toml_str(&raw)?;
        manifest.verify_assets(root)?;
        Ok(manifest)
    }

    pub fn verify_assets(&self, root: impl AsRef<Path>) -> Result<(), ManifestError> {
        self.validate()?;
        let root = root.as_ref();
        let canonical_root = fs::canonicalize(root).map_err(|source| ManifestError::Io {
            path: root.to_path_buf(),
            message: source.to_string(),
        })?;
        for asset in &self.assets {
            let path = root.join(&asset.path);
            let canonical = fs::canonicalize(&path).map_err(|source| ManifestError::Io {
                path: path.clone(),
                message: source.to_string(),
            })?;
            if !canonical.starts_with(&canonical_root) {
                return Err(ManifestError::AssetEscapesRoot { path });
            }
            let metadata = fs::metadata(&canonical).map_err(|source| ManifestError::Io {
                path: canonical.clone(),
                message: source.to_string(),
            })?;
            if let Some(expected) = asset.size_bytes
                && metadata.len() != expected
            {
                return Err(ManifestError::SizeMismatch {
                    path: asset.path.clone(),
                    expected,
                    actual: metadata.len(),
                });
            }
            let actual = sha256_file(&canonical).map_err(|source| ManifestError::Io {
                path: canonical,
                message: source.to_string(),
            })?;
            if !actual.eq_ignore_ascii_case(&asset.sha256) {
                return Err(ManifestError::HashMismatch {
                    path: asset.path.clone(),
                    expected: asset.sha256.clone(),
                    actual,
                });
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn manifest_hash(&self) -> String {
        let serialized = toml::to_string(self).unwrap_or_default();
        sha256_bytes(serialized.as_bytes())
    }

    #[must_use]
    pub fn version(&self) -> IntrinsicModelVersion {
        IntrinsicModelVersion {
            model_id: self.model_id.clone(),
            base_version: self.model_version.clone(),
            adapter_version: self.adapter_version.clone(),
            manifest_hash: self.manifest_hash(),
        }
    }

    #[must_use]
    pub fn asset_path(&self, root: impl AsRef<Path>, asset: &IntrinsicAsset) -> PathBuf {
        root.as_ref().join(&asset.path)
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ManifestError {
    #[error("manifest schema version {version} is unsupported")]
    UnsupportedVersion { version: u16 },
    #[error("manifest field `{field}` is empty, too long, or contains control characters")]
    InvalidText { field: &'static str },
    #[error("manifest must expose text or vision")]
    NoSupportedCapability,
    #[error("Intrinsic v1 must not enable audio")]
    AudioNotSupported,
    #[error("manifest limit `{field}` is invalid")]
    InvalidLimit { field: &'static str },
    #[error("manifest contains {length} assets, maximum {maximum}")]
    TooManyAssets { length: usize, maximum: usize },
    #[error("manifest is {length} bytes, maximum {maximum}")]
    ManifestTooLarge { length: usize, maximum: usize },
    #[error("manifest parse failed: {message}")]
    Parse { message: String },
    #[error("asset path is unsafe: {path}")]
    InvalidAssetPath { path: String },
    #[error("asset `{path}` has an invalid sha256")]
    InvalidHash { path: String },
    #[error("asset path escapes the model root: {path}")]
    AssetEscapesRoot { path: PathBuf },
    #[error("asset `{path}` has size {actual}, expected {expected}")]
    SizeMismatch {
        path: String,
        expected: u64,
        actual: u64,
    },
    #[error("asset `{path}` hash mismatch: expected {expected}, got {actual}")]
    HashMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("I/O for `{path}` failed: {message}")]
    Io { path: PathBuf, message: String },
}

fn validate_manifest_text(field: &'static str, value: &str) -> Result<(), ManifestError> {
    if value.trim().is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        return Err(ManifestError::InvalidText { field });
    }
    Ok(())
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_digest(&hasher.finalize()))
}

fn sha256_bytes(value: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value);
    hex_digest(&hasher.finalize())
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

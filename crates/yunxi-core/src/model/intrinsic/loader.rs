//! Verified asset loading boundary for the bundled intrinsic model runtime.

use super::config::IntrinsicRuntimeConfig;
use super::minimind::MiniMindEngine;
use super::runtime::{IntrinsicModelRuntime, IntrinsicRuntimeError};
use crate::model::{IntrinsicModelManifest, IntrinsicModelVersion, ManifestError, ModelHealth};
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntrinsicAssetLoadReport {
    pub health: ModelHealth,
    pub version: Option<IntrinsicModelVersion>,
    pub asset_count: usize,
    pub supports_text: bool,
    pub supports_vision: bool,
    pub error: Option<String>,
}

/// Runtime plus the bounded asset metadata used by host status surfaces.
#[derive(Debug)]
pub struct IntrinsicAssetRuntime {
    pub runtime: Arc<IntrinsicModelRuntime>,
    pub report: IntrinsicAssetLoadReport,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct IntrinsicAssetLoader;

impl IntrinsicAssetLoader {
    pub fn validate_directory(
        &self,
        root: impl AsRef<Path>,
    ) -> Result<IntrinsicAssetLoadReport, ManifestError> {
        let manifest = IntrinsicModelManifest::load_from_dir(root)?;
        Ok(IntrinsicAssetLoadReport {
            // The current Rust-native compatibility engine is deliberately
            // degraded even when the bundle verifies; a future tensor engine
            // may report Healthy after it has actually loaded its weights.
            health: ModelHealth::Degraded,
            version: Some(manifest.version()),
            asset_count: manifest.assets.len(),
            supports_text: manifest.supports_text,
            supports_vision: manifest.supports_vision,
            error: None,
        })
    }

    /// Load a verified bundle when present, otherwise keep the host alive with
    /// the builtin deterministic compatibility runtime. A present but invalid
    /// manifest is treated differently: it yields an unavailable runtime so a
    /// damaged bundle can never be mistaken for a usable model.
    pub fn load_or_builtin(
        &self,
        root: impl AsRef<Path>,
        config: IntrinsicRuntimeConfig,
    ) -> Result<IntrinsicAssetRuntime, IntrinsicRuntimeError> {
        let root = root.as_ref();
        let manifest_path = root.join("manifest.toml");
        if !manifest_path.exists() {
            let runtime = Arc::new(IntrinsicModelRuntime::builtin(config)?);
            let report = IntrinsicAssetLoadReport {
                health: runtime.health(),
                version: Some(runtime.version()),
                asset_count: 0,
                supports_text: true,
                // The compatibility engine has no image encoder. It must not
                // claim vision support merely because it can return text.
                supports_vision: false,
                error: None,
            };
            return Ok(IntrinsicAssetRuntime { runtime, report });
        }

        match IntrinsicModelManifest::load_from_dir(root) {
            Ok(manifest) => {
                let version = manifest.version();
                let engine = match MiniMindEngine::load_from_dir(
                    root,
                    version.clone(),
                    manifest.context_limit,
                ) {
                    Ok(engine) => engine,
                    Err(error) => {
                        let runtime =
                            Arc::new(IntrinsicModelRuntime::unavailable(config, version.clone())?);
                        let report = IntrinsicAssetLoadReport {
                            health: ModelHealth::Unavailable,
                            version: Some(version),
                            asset_count: manifest.assets.len(),
                            supports_text: false,
                            supports_vision: false,
                            error: Some(format!("MiniMind weight load failed: {error}")),
                        };
                        return Ok(IntrinsicAssetRuntime { runtime, report });
                    }
                };
                let supports_text = manifest.supports_text;
                let supports_vision = manifest.supports_vision && engine.supports_vision();
                if manifest.supports_vision && !engine.supports_vision() {
                    let runtime =
                        Arc::new(IntrinsicModelRuntime::unavailable(config, version.clone())?);
                    let report = IntrinsicAssetLoadReport {
                        health: ModelHealth::Unavailable,
                        version: Some(version),
                        asset_count: manifest.assets.len(),
                        supports_text: false,
                        supports_vision: false,
                        error: Some(
                            "manifest declares vision support but the SigLIP assets were not loaded"
                                .to_owned(),
                        ),
                    };
                    return Ok(IntrinsicAssetRuntime { runtime, report });
                }
                let runtime = Arc::new(IntrinsicModelRuntime::new(Arc::new(engine), config)?);
                let report = IntrinsicAssetLoadReport {
                    health: runtime.health(),
                    version: Some(version),
                    asset_count: manifest.assets.len(),
                    supports_text,
                    supports_vision,
                    error: None,
                };
                Ok(IntrinsicAssetRuntime { runtime, report })
            }
            Err(error) => {
                let version = fallback_invalid_version();
                let runtime = Arc::new(IntrinsicModelRuntime::unavailable(config, version)?);
                let report = IntrinsicAssetLoadReport {
                    health: ModelHealth::Unavailable,
                    version: Some(runtime.version()),
                    asset_count: 0,
                    supports_text: false,
                    supports_vision: false,
                    error: Some(error.to_string()),
                };
                Ok(IntrinsicAssetRuntime { runtime, report })
            }
        }
    }
}

fn fallback_invalid_version() -> IntrinsicModelVersion {
    IntrinsicModelVersion {
        model_id: "yunxi-intrinsic-invalid-bundle".to_owned(),
        base_version: "unavailable".to_owned(),
        adapter_version: None,
        manifest_hash: "invalid".to_owned(),
    }
}

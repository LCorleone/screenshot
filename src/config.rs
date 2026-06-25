//! Application settings: load/save a small JSON file under the OS config dir.

use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

const APP_QUALIFIER: &str = "ai";
const APP_ORG: &str = "dai";
const APP_NAME: &str = "screenshot-dai";
const SETTINGS_FILE: &str = "settings.json";

/// User-configurable settings, persisted as `settings.json` in the OS config dir.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Base URL for the OpenAI-compatible API (no trailing slash).
    pub openai_base_url: String,
    /// API key (kept in plaintext on disk for now; Phase 0 simplicity).
    pub openai_api_key: String,
    /// Model id, e.g. `gpt-4o`.
    pub openai_model: String,
    /// Optional OCR endpoint. Empty means "use the LLM for OCR".
    pub ocr_endpoint: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            openai_base_url: "https://api.openai.com/v1".to_string(),
            openai_api_key: String::new(),
            openai_model: "gpt-4o".to_string(),
            ocr_endpoint: String::new(),
        }
    }
}

impl Settings {
    /// Load settings from disk; returns `Default` if the file is missing.
    pub fn load() -> anyhow::Result<Self> {
        let path = match settings_path() {
            Some(p) => p,
            None => {
                tracing::warn!("no config dir available; using default settings");
                return Ok(Self::default());
            }
        };
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let settings: Self = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(settings)
    }

    /// Persist settings to disk.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = match settings_path() {
            Some(p) => p,
            None => anyhow::bail!("no config dir available to write settings"),
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let raw = serde_json::to_string_pretty(self).context("failed to serialize settings")?;
        std::fs::write(&path, raw)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }
}

/// Resolve the settings file path under the OS config dir.
fn settings_path() -> Option<PathBuf> {
    let pd = directories::ProjectDirs::from(APP_QUALIFIER, APP_ORG, APP_NAME)?;
    Some(pd.config_dir().join(SETTINGS_FILE))
}

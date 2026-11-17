//! Persistent configuration for po.
//!
//! The config file is JSON next to the session data. Secrets are never
//! written to it: the API key is read from the environment (`PO_API_KEY`) or
//! an untracked local file.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::Provider;
use crate::paths;

/// Errors produced while loading or saving configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Filesystem failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Malformed config file.
    #[error("invalid config: {0}")]
    Parse(#[from] serde_json::Error),
}

/// Which colour theme is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    /// Light bumblebee palette.
    Light,
    /// Dark bumblebee palette.
    Dark,
}

impl ThemeMode {
    /// The opposite mode.
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::Light => Self::Dark,
            Self::Dark => Self::Light,
        }
    }

    /// Parse a user-supplied mode name.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }
}

/// po's configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Active provider.
    pub provider: Provider,
    /// Active theme.
    pub theme: ThemeMode,
    /// Font family used across the UI.
    pub font_family: String,
    /// Maximum number of tokens generated per turn.
    pub max_new_tokens: usize,
    /// Whether tool calls require explicit approval.
    pub approve_tools: bool,
    /// Directory where sessions and logs are stored.
    pub data_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: Provider::LocalMicroAi {
                model_path: default_model_path(),
            },
            theme: ThemeMode::Dark,
            font_family: "IBM3270".into(),
            max_new_tokens: 512,
            approve_tools: true,
            data_dir: paths::po_data_dir(),
        }
    }
}

/// Default model path, relative to the repository's `models/` directory.
///
/// The file itself is gitignored; `models/README.md` explains how to link a
/// local nano-ai model checkout.
#[must_use]
pub fn default_model_path() -> String {
    "models/minicpm-v-4.7-1b-tern4.bin".into()
}

impl Config {
    /// Load configuration from `path`, falling back to defaults when absent.
    ///
    /// # Errors
    /// Returns an error when the file exists but cannot be read or parsed.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Load configuration from the default location.
    ///
    /// # Errors
    /// Returns an error when the file exists but cannot be read or parsed.
    pub fn load_default() -> Result<Self, ConfigError> {
        Self::load(paths::po_config_path())
    }

    /// Persist the configuration to `path`.
    ///
    /// # Errors
    /// Returns an error when the directory or file cannot be written.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        fs::write(path, text)?;
        Ok(())
    }

    /// Persist the configuration to the default location.
    ///
    /// # Errors
    /// Returns an error when the file cannot be written.
    pub fn save_default(&self) -> Result<(), ConfigError> {
        self.save(paths::po_config_path())
    }

    /// Directory holding session logs.
    #[must_use]
    pub fn sessions_dir(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }

    /// Resolve the API key for remote providers from the environment.
    #[must_use]
    pub fn api_key_from_env(&self) -> Option<String> {
        std::env::var("PO_API_KEY").ok().filter(|k| !k.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.json");
        let mut config = Config::default();
        let _ = &mut config;
        config.theme = ThemeMode::Light;
        config.max_new_tokens = 128;
        config.save(&path).expect("save");

        let loaded = Config::load(&path).expect("load");
        assert_eq!(loaded.theme, ThemeMode::Light);
        assert_eq!(loaded.max_new_tokens, 128);
        assert_eq!(loaded.font_family, "IBM3270");
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = Config::load(dir.path().join("absent.json")).expect("load");
        assert_eq!(config, Config::default());
    }

    #[test]
    fn theme_mode_toggles_and_parses() {
        assert_eq!(ThemeMode::Light.toggled(), ThemeMode::Dark);
        assert_eq!(ThemeMode::parse("DARK"), Some(ThemeMode::Dark));
        assert_eq!(ThemeMode::parse("sepia"), None);
    }
}

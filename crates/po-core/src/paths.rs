//! Platform paths, derived from the standard library only.
//!
//! po deliberately avoids a platform-detection dependency: the few paths it
//! needs are resolved from environment variables, which keeps the dependency
//! surface small and the behaviour explicit.

use std::path::PathBuf;

/// The user's home directory.
#[must_use]
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Per-user data directory (where sessions and config live).
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        home_dir().map(|home| home.join("Library").join("Application Support"))
    } else if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| home_dir().map(|home| home.join(".local").join("share")))
    }
}

/// Per-user configuration directory.
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        home_dir().map(|home| home.join("Library").join("Application Support"))
    } else if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home_dir().map(|home| home.join(".config")))
    }
}

/// Root directory for po's own data (`…/po`).
#[must_use]
pub fn po_data_dir() -> PathBuf {
    data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("po")
}

/// Directory holding po's session logs.
#[must_use]
pub fn po_sessions_dir() -> PathBuf {
    po_data_dir().join("sessions")
}

/// Path of po's configuration file.
#[must_use]
pub fn po_config_path() -> PathBuf {
    config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("po")
        .join("config.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn po_paths_end_with_po() {
        assert!(po_data_dir().ends_with("po"));
        assert!(po_sessions_dir().ends_with("sessions"));
        assert!(po_config_path().ends_with("config.json"));
    }

    #[test]
    fn home_dir_is_resolvable_in_a_normal_environment() {
        // Tests run with a home directory in CI and on developer machines.
        assert!(home_dir().is_some());
    }
}

// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! The bridge configuration file shared by the host and the driver.
//!
//! The file sits next to the `cua-driver` binary ([`CONFIG_FILE_NAME`]) and
//! is written by the installer. Chrome starts the host with an environment
//! the driver does not control, so the host takes nothing from its
//! environment or from the Chrome side except the caller origin it then
//! checks against this file. A missing file means the bridge is off and
//! the driver behaves exactly as upstream.
//!
//! No deployment-specific value (extension id, socket path, browser path)
//! is compiled in: this fork is public and every deployment supplies its
//! own file.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// File name looked up in the directory of the running executable.
pub const CONFIG_FILE_NAME: &str = "cua-driver-ext-bridge.json";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BridgeConfig {
    /// Chrome extension id (32 characters `a`–`p`). The host accepts only
    /// the origin `chrome-extension://<id>/`.
    pub extension_id: String,
    /// Absolute path of the driver's Unix socket. Its parent directory is
    /// created with mode 0700.
    pub socket_path: PathBuf,
    /// Upload roots sent to the extension in `driver_hello.config`.
    #[serde(default)]
    pub upload_roots: Vec<String>,
    /// Absolute paths of the browser executables a host's parent process
    /// may run (compared by file identity, so symlinks to the same binary
    /// match).
    #[serde(default)]
    pub browser_executables: Vec<PathBuf>,
    /// Navigation schemes sent in `driver_hello.config` when set; absent
    /// means the extension's own default applies.
    #[serde(default)]
    pub nav_schemes: Option<Vec<String>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("read bridge config {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parse bridge config {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("bridge config {path}: {reason}")]
    Invalid { path: PathBuf, reason: String },
}

impl BridgeConfig {
    /// The only caller origin the host accepts.
    pub fn expected_origin(&self) -> String {
        format!("chrome-extension://{}/", self.extension_id)
    }

    /// The `config` object of `driver_hello` (spec §5); the host forwards it
    /// into `welcome` unchanged.
    pub fn driver_hello_config(&self) -> serde_json::Value {
        let mut config = serde_json::json!({ "upload_roots": self.upload_roots });
        if let Some(schemes) = &self.nav_schemes {
            config["nav_schemes"] = serde_json::json!(schemes);
        }
        config
    }

    fn validate(&self) -> Result<(), String> {
        let id = &self.extension_id;
        if id.len() != 32 || !id.bytes().all(|b| (b'a'..=b'p').contains(&b)) {
            return Err("extension_id must be 32 characters a-p".into());
        }
        if !self.socket_path.is_absolute() {
            return Err("socket_path must be absolute".into());
        }
        if self
            .socket_path
            .parent()
            .is_none_or(|p| p == Path::new("/"))
        {
            return Err("socket_path must be inside a dedicated directory".into());
        }
        if let Some(path) = self.browser_executables.iter().find(|p| !p.is_absolute()) {
            return Err(format!(
                "browser_executables entry {} is not absolute",
                path.display()
            ));
        }
        Ok(())
    }
}

/// `<directory of the running executable>/cua-driver-ext-bridge.json`.
pub fn default_config_path() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe
        .parent()
        .ok_or_else(|| std::io::Error::other("executable has no parent directory"))?;
    Ok(dir.join(CONFIG_FILE_NAME))
}

/// Loads and validates the file. `Ok(None)` when it does not exist (bridge
/// off); a file that exists but is unreadable or invalid is an error, never
/// "off".
pub fn load(path: &Path) -> Result<Option<BridgeConfig>, ConfigError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    let config: BridgeConfig =
        serde_json::from_slice(&bytes).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    config.validate().map_err(|reason| ConfigError::Invalid {
        path: path.to_path_buf(),
        reason,
    })?;
    Ok(Some(config))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// A syntactically valid id that belongs to no extension.
    pub(crate) fn test_extension_id() -> String {
        "a".repeat(32)
    }

    fn write(dir: &Path, value: serde_json::Value) -> PathBuf {
        let path = dir.join(CONFIG_FILE_NAME);
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        path
    }

    #[test]
    fn missing_file_means_off() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(&dir.path().join(CONFIG_FILE_NAME)).unwrap().is_none());
    }

    #[test]
    fn valid_file_loads_and_builds_handshake_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            json!({
                "extension_id": test_extension_id(),
                "socket_path": "/run/test/bridge/driver.sock",
                "upload_roots": ["/tmp/up"],
                "browser_executables": ["/opt/browser/browser"],
                "future_field": true
            }),
        );
        let config = load(&path).unwrap().unwrap();
        assert_eq!(
            config.expected_origin(),
            format!("chrome-extension://{}/", test_extension_id())
        );
        assert_eq!(
            config.driver_hello_config(),
            json!({"upload_roots": ["/tmp/up"]})
        );
        let with_schemes = BridgeConfig {
            nav_schemes: Some(vec!["https".into()]),
            ..config
        };
        assert_eq!(
            with_schemes.driver_hello_config(),
            json!({"upload_roots": ["/tmp/up"], "nav_schemes": ["https"]})
        );
    }

    #[test]
    fn invalid_files_are_errors_not_off() {
        let dir = tempfile::tempdir().unwrap();
        for value in [
            json!({"extension_id": "short", "socket_path": "/run/x/s.sock"}),
            json!({"extension_id": "z".repeat(32), "socket_path": "/run/x/s.sock"}),
            json!({"extension_id": test_extension_id(), "socket_path": "rel/s.sock"}),
            json!({"extension_id": test_extension_id(), "socket_path": "/s.sock"}),
            json!({"extension_id": test_extension_id(), "socket_path": "/run/x/s.sock",
                   "browser_executables": ["chrome"]}),
        ] {
            let path = write(dir.path(), value.clone());
            assert!(
                matches!(load(&path), Err(ConfigError::Invalid { .. })),
                "{value}"
            );
        }
        std::fs::write(dir.path().join(CONFIG_FILE_NAME), b"{").unwrap();
        assert!(matches!(
            load(&dir.path().join(CONFIG_FILE_NAME)),
            Err(ConfigError::Parse { .. })
        ));
    }
}

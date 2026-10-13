//! The global `config.toml`.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::backend::BackendKind;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Container engine to use when `--backend` is not given.
    pub backend: Option<BackendKind>,
    /// Profile to use when `--profile` is not given. Falls back to a profile
    /// named `default` if one exists.
    pub default_profile: Option<String>,
    /// Use this image instead of building the bundled base image.
    pub base_image: Option<String>,
}

impl Config {
    /// Loads `path`, or returns the defaults if it does not exist.
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_default() {
        let cfg = Config::load(Path::new("/nonexistent/podbay/config.toml")).unwrap();
        assert!(cfg.backend.is_none());
    }

    #[test]
    fn parses_fields() {
        let cfg: Config =
            toml::from_str("backend = \"docker\"\ndefault_profile = \"ai\"\n").unwrap();
        assert_eq!(cfg.backend, Some(BackendKind::Docker));
        assert_eq!(cfg.default_profile.as_deref(), Some("ai"));
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<Config>("bakend = \"docker\"").is_err());
    }
}

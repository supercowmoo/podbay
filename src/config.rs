//! The global `config.toml`.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result, bail};
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
    /// Resource limits for every session; profiles can override them.
    #[serde(default)]
    pub limits: Limits,
}

/// Default process (and thread) limit: enough for parallel builds, low
/// enough to stop a fork bomb.
pub const DEFAULT_PIDS: u32 = 4096;

/// Resource limits, from `[limits]` in config.toml or a profile. Unset
/// fields inherit; set ones override.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Processes and threads. 0 means no limit.
    pub pids: Option<u32>,
    /// Memory, in the engine's syntax: "8g", "512m".
    pub memory: Option<String>,
    /// CPUs, such as 2 or 1.5.
    pub cpus: Option<f64>,
}

impl Limits {
    /// Applies `other`'s set fields on top of these.
    pub fn merge(&mut self, other: &Limits) {
        if other.pids.is_some() {
            self.pids = other.pids;
        }
        if other.memory.is_some() {
            self.memory.clone_from(&other.memory);
        }
        if other.cpus.is_some() {
            self.cpus = other.cpus;
        }
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(memory) = &self.memory {
            let digits = memory.trim_end_matches(['b', 'k', 'm', 'g', 'B', 'K', 'M', 'G']);
            if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
                bail!(
                    "invalid memory limit {memory:?}: use a number with b, k, m or g, such as \"8g\""
                );
            }
        }
        if let Some(cpus) = self.cpus
            && !(cpus > 0.0 && cpus.is_finite())
        {
            bail!("invalid cpus limit {cpus}: must be greater than 0");
        }
        Ok(())
    }
}

impl Config {
    /// Loads `path`, or returns the defaults if it does not exist.
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => {
                let config: Self =
                    toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
                config
                    .limits
                    .validate()
                    .with_context(|| format!("invalid {}", path.display()))?;
                Ok(config)
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
    fn limits_parse_merge_and_validate() {
        let cfg: Config = toml::from_str("[limits]\npids = 100\nmemory = \"8g\"\n").unwrap();
        cfg.limits.validate().unwrap();
        let mut limits = cfg.limits;
        limits.merge(&Limits {
            pids: Some(0),
            cpus: Some(1.5),
            ..Default::default()
        });
        assert_eq!(limits.pids, Some(0));
        assert_eq!(limits.memory.as_deref(), Some("8g"));
        assert_eq!(limits.cpus, Some(1.5));

        for bad in [
            "memory = \"lots\"",
            "memory = \"\"",
            "cpus = 0",
            "cpus = -1",
        ] {
            let l: Limits = toml::from_str(bad).unwrap();
            assert!(l.validate().is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<Config>("bakend = \"docker\"").is_err());
    }
}

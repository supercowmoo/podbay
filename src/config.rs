//! The global `config.toml`.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::Deserialize;

use crate::backend::BackendKind;
use crate::egress::HostPattern;

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
    /// Network access for every session; profiles and `--network` can
    /// override it.
    pub network: Option<NetworkSetting>,
    /// Resource limits for every session; profiles can override them.
    #[serde(default)]
    pub limits: Limits,
}

/// A `network` setting from config.toml or a profile: `"none"`, `"full"`,
/// or a list of hosts the session may connect to.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawNetwork")]
pub enum NetworkSetting {
    Mode(NetworkMode),
    Allow(Vec<HostPattern>),
}

#[derive(Deserialize)]
#[serde(
    untagged,
    expecting = "\"none\", \"full\", or a list of hosts such as [\"api.example.com\"]"
)]
enum RawNetwork {
    Mode(NetworkMode),
    Allow(Vec<String>),
}

impl TryFrom<RawNetwork> for NetworkSetting {
    type Error = anyhow::Error;
    fn try_from(raw: RawNetwork) -> Result<Self> {
        Ok(match raw {
            RawNetwork::Mode(mode) => Self::Mode(mode),
            RawNetwork::Allow(hosts) => Self::Allow(
                hosts
                    .iter()
                    .map(|h| HostPattern::parse(h))
                    .collect::<Result<_>>()?,
            ),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    /// Loopback only.
    None,
    /// The engine's default network: the internet, the LAN, and depending
    /// on the engine, the host.
    Full,
}

/// What a session can reach, once config.toml, the profile chain and
/// `--network` are combined. Tool installs happen in a separate
/// provisioning step, so sessions need no network by default.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum Network {
    #[default]
    None,
    Full,
    /// HTTPS to these hosts only, through podbay's proxy.
    Allow(Vec<HostPattern>),
}

impl Network {
    /// Applies the next setting in order. A mode replaces what came before;
    /// a list of hosts adds to an earlier list, or replaces a mode.
    pub fn apply(&mut self, setting: &NetworkSetting) {
        match (setting, &mut *self) {
            (NetworkSetting::Mode(NetworkMode::None), _) => *self = Self::None,
            (NetworkSetting::Mode(NetworkMode::Full), _) => *self = Self::Full,
            (NetworkSetting::Allow(hosts), Self::Allow(have)) => {
                for host in hosts {
                    if !have.contains(host) {
                        have.push(host.clone());
                    }
                }
            }
            (NetworkSetting::Allow(hosts), _) => *self = Self::Allow(hosts.clone()),
        }
    }

    /// `none`, `full`, or the allowed hosts.
    pub fn describe(&self) -> String {
        match self {
            Self::None => "none".into(),
            Self::Full => "full".into(),
            Self::Allow(hosts) => {
                let hosts: Vec<String> = hosts.iter().map(ToString::to_string).collect();
                format!("allow {}", hosts.join(", "))
            }
        }
    }
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
        assert_eq!(cfg.network, None);
    }

    #[test]
    fn parses_network() {
        let cfg: Config = toml::from_str("network = \"full\"\n").unwrap();
        assert_eq!(cfg.network, Some(NetworkSetting::Mode(NetworkMode::Full)));
        let cfg: Config = toml::from_str("network = [\"a.example.com\"]\n").unwrap();
        assert_eq!(
            cfg.network,
            Some(NetworkSetting::Allow(vec![
                HostPattern::parse("a.example.com").unwrap()
            ]))
        );
        assert!(toml::from_str::<Config>("network = \"some\"\n").is_err());
        let err = toml::from_str::<Config>("network = [\"10.0.0.1\"]\n").unwrap_err();
        assert!(err.to_string().contains("IP addresses"), "{err}");
    }

    #[test]
    fn network_settings_combine() {
        let allow = |hosts: &[&str]| {
            NetworkSetting::Allow(
                hosts
                    .iter()
                    .map(|h| HostPattern::parse(h).unwrap())
                    .collect(),
            )
        };
        let mut net = Network::default();
        net.apply(&allow(&["a.com"]));
        net.apply(&allow(&["b.com", "a.com"]));
        assert_eq!(net.describe(), "allow a.com, b.com");
        net.apply(&NetworkSetting::Mode(NetworkMode::Full));
        assert_eq!(net, Network::Full);
        net.apply(&allow(&["c.com:8443"]));
        assert_eq!(net.describe(), "allow c.com:8443");
        net.apply(&NetworkSetting::Mode(NetworkMode::None));
        assert_eq!(net, Network::None);
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

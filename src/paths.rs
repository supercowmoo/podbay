//! Where podbay keeps its configuration and caches on the host.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
    /// `$PODBAY_CONFIG_DIR`, else `$XDG_CONFIG_HOME/podbay`
    pub config_dir: PathBuf,
    /// `$PODBAY_CACHE_DIR`, else `$XDG_CACHE_HOME/podbay`
    pub cache_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Self> {
        let home = env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .context("HOME is not set")?;
        if !home.is_absolute() {
            bail!("HOME must be an absolute path, got {}", home.display());
        }
        let absolute = |var: &str| {
            env::var_os(var)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
        };
        // The PODBAY_ variables move podbay's own files without changing
        // HOME or XDG_*, which rootless podman reads to find its storage.
        let dir = |own: &str, xdg: &str, fallback: &str| {
            absolute(own).unwrap_or_else(|| {
                absolute(xdg)
                    .unwrap_or_else(|| home.join(fallback))
                    .join("podbay")
            })
        };
        Ok(Self {
            config_dir: dir("PODBAY_CONFIG_DIR", "XDG_CONFIG_HOME", ".config"),
            cache_dir: dir("PODBAY_CACHE_DIR", "XDG_CACHE_HOME", ".cache"),
            home,
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn profiles_dir(&self) -> PathBuf {
        self.config_dir.join("profiles")
    }

    pub fn profile_file(&self, name: &str) -> PathBuf {
        self.profiles_dir().join(format!("{name}.toml"))
    }

    /// Expands a leading `~` or `~/` to the home directory.
    pub fn expand_tilde(&self, path: &str) -> PathBuf {
        if path == "~" {
            self.home.clone()
        } else if let Some(rest) = path.strip_prefix("~/") {
            self.home.join(rest)
        } else {
            PathBuf::from(path)
        }
    }

    /// Resolves `path` relative to `base` unless it is already absolute,
    /// after tilde expansion.
    pub fn resolve_relative(&self, base: &Path, path: &str) -> PathBuf {
        let expanded = self.expand_tilde(path);
        if expanded.is_absolute() {
            expanded
        } else {
            base.join(expanded)
        }
    }
}

#[cfg(test)]
pub fn test_paths(root: &Path) -> Paths {
    Paths {
        home: root.join("home"),
        config_dir: root.join("config/podbay"),
        cache_dir: root.join("cache/podbay"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expansion() {
        let p = test_paths(Path::new("/r"));
        assert_eq!(p.expand_tilde("~"), PathBuf::from("/r/home"));
        assert_eq!(p.expand_tilde("~/.zshrc"), PathBuf::from("/r/home/.zshrc"));
        assert_eq!(p.expand_tilde("~other/x"), PathBuf::from("~other/x"));
        assert_eq!(p.expand_tilde("/abs"), PathBuf::from("/abs"));
    }

    #[test]
    fn relative_resolution() {
        let p = test_paths(Path::new("/r"));
        assert_eq!(
            p.resolve_relative(Path::new("/base"), "ext.sh"),
            PathBuf::from("/base/ext.sh")
        );
        assert_eq!(
            p.resolve_relative(Path::new("/base"), "/abs.sh"),
            PathBuf::from("/abs.sh")
        );
    }
}

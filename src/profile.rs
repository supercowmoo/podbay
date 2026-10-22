//! Profiles: one TOML file per profile under `profiles/`, composable through
//! `extends`.
//!
//! ```toml
//! extends = ["base"]
//! extension = "ai.sh"   # optional root build script, relative to profiles/
//!
//! [[mount]]
//! host = "~/.gitconfig"
//! container = "/home/dev/.gitconfig"
//! mode = "ro"           # default
//!
//! [[volume]]
//! name = "opencode-config"
//! container = "/home/dev/.config/opencode"
//!
//! [tools]
//! opencode = "latest"
//! ```

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::Limits;
use crate::paths::Paths;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileFile {
    #[serde(default)]
    extends: Vec<String>,
    extension: Option<String>,
    #[serde(default, rename = "mount")]
    mounts: Vec<MountEntry>,
    #[serde(default, rename = "volume")]
    volumes: Vec<VolumeEntry>,
    #[serde(default)]
    tools: toml::Table,
    /// Capabilities the session keeps, such as "SYS_PTRACE".
    #[serde(default)]
    cap_add: Vec<String>,
    #[serde(default)]
    limits: Limits,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MountEntry {
    host: String,
    container: String,
    #[serde(default)]
    mode: Mode,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    #[default]
    Ro,
    Rw,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VolumeEntry {
    name: String,
    container: String,
}

/// A bind mount from the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub host: PathBuf,
    pub container: String,
    pub readonly: bool,
    /// Profile that declared it.
    pub profile: String,
}

/// A named engine volume, namespaced by the profile that declared it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    pub name: String,
    pub container: String,
    pub profile: String,
}

/// A root build script layered onto the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extension {
    pub profile: String,
    pub script: PathBuf,
}

/// A profile with its `extends` chain flattened.
#[derive(Debug, Default)]
pub struct ResolvedProfile {
    /// Profiles in the order they were applied (ancestors first).
    pub chain: Vec<String>,
    pub mounts: Vec<Mount>,
    pub volumes: Vec<Volume>,
    pub tools: toml::Table,
    pub extensions: Vec<Extension>,
    /// Capabilities added back for the session, in the order declared.
    pub caps: Vec<Capability>,
    /// Limits set by the chain; unset fields fall back to config.toml.
    pub limits: Limits,
    /// Non-fatal problems, such as a mount whose host path is missing.
    pub warnings: Vec<String>,
}

/// A Linux capability a profile adds back, by its name without `CAP_`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    pub name: String,
    pub profile: String,
}

const CAPABILITIES: &[&str] = &[
    "AUDIT_CONTROL",
    "AUDIT_READ",
    "AUDIT_WRITE",
    "BLOCK_SUSPEND",
    "BPF",
    "CHECKPOINT_RESTORE",
    "CHOWN",
    "DAC_OVERRIDE",
    "DAC_READ_SEARCH",
    "FOWNER",
    "FSETID",
    "IPC_LOCK",
    "IPC_OWNER",
    "KILL",
    "LEASE",
    "LINUX_IMMUTABLE",
    "MAC_ADMIN",
    "MAC_OVERRIDE",
    "MKNOD",
    "NET_ADMIN",
    "NET_BIND_SERVICE",
    "NET_BROADCAST",
    "NET_RAW",
    "PERFMON",
    "SETFCAP",
    "SETGID",
    "SETPCAP",
    "SETUID",
    "SYS_ADMIN",
    "SYS_BOOT",
    "SYS_CHROOT",
    "SYS_MODULE",
    "SYS_NICE",
    "SYS_PACCT",
    "SYS_PTRACE",
    "SYS_RAWIO",
    "SYS_RESOURCE",
    "SYS_TIME",
    "SYS_TTY_CONFIG",
    "SYSLOG",
    "WAKE_ALARM",
];

/// Normalizes `sys_ptrace` or `CAP_SYS_PTRACE` to `SYS_PTRACE`.
fn capability_name(name: &str) -> Result<String> {
    let upper = name.to_ascii_uppercase();
    let bare = upper.strip_prefix("CAP_").unwrap_or(&upper);
    if !CAPABILITIES.contains(&bare) {
        bail!("unknown capability {name:?} in cap_add");
    }
    Ok(bare.to_owned())
}

/// Profile names end up in file names, image tags and volume names, so they
/// are restricted to a conservative character set.
pub fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !valid {
        bail!(
            "invalid profile name {name:?}: use lowercase letters, digits, '-' and '_', \
             starting with a letter or digit"
        );
    }
    Ok(())
}

fn validate_volume_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !valid {
        bail!("invalid volume name {name:?}: use letters, digits, '-', '_' and '.'");
    }
    Ok(())
}

/// Container paths are passed to the engine in comma-separated `--mount`
/// specs and a colon-separated environment variable, so neither character is
/// allowed.
pub fn validate_container_path(path: &str) -> Result<()> {
    if !path.starts_with('/') {
        bail!("container path {path:?} must be absolute");
    }
    if path.contains([',', ':', '\n']) {
        bail!("container path {path:?} must not contain ',', ':' or newlines");
    }
    if path.split('/').any(|seg| seg == "..") {
        bail!("container path {path:?} must not contain '..'");
    }
    Ok(())
}

/// Names of all profiles, sorted.
pub fn list(paths: &Paths) -> Result<Vec<String>> {
    let dir = paths.profiles_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut names = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "toml")
            && let Some(name) = path.file_stem().and_then(|s| s.to_str())
            && validate_name(name).is_ok()
        {
            names.push(name.to_owned());
        }
    }
    names.sort();
    Ok(names)
}

/// Loads `name` and everything it extends.
pub fn resolve(paths: &Paths, name: &str) -> Result<ResolvedProfile> {
    let mut resolved = ResolvedProfile::default();
    let mut done = HashSet::new();
    let mut stack = Vec::new();
    visit(paths, name, &mut stack, &mut done, &mut resolved)?;
    Ok(resolved)
}

fn visit(
    paths: &Paths,
    name: &str,
    stack: &mut Vec<String>,
    done: &mut HashSet<String>,
    out: &mut ResolvedProfile,
) -> Result<()> {
    validate_name(name)?;
    if stack.iter().any(|n| n == name) {
        bail!("profile cycle: {} -> {name}", stack.join(" -> "));
    }
    // Diamonds (two parents extending the same profile) apply it once.
    if done.contains(name) {
        return Ok(());
    }

    let file = paths.profile_file(name);
    let text = match fs::read_to_string(&file) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let mut msg = format!("profile {name:?} not found at {}", file.display());
            if let Some(child) = stack.last() {
                msg.push_str(&format!(" (extended by {child:?})"));
            }
            bail!(msg);
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
    };
    let profile: ProfileFile =
        toml::from_str(&text).with_context(|| format!("invalid profile {}", file.display()))?;

    stack.push(name.to_owned());
    for parent in &profile.extends {
        visit(paths, parent, stack, done, out)?;
    }
    stack.pop();

    apply(paths, name, profile, out).with_context(|| format!("in profile {}", file.display()))?;
    done.insert(name.to_owned());
    out.chain.push(name.to_owned());
    Ok(())
}

/// Merges one profile on top of what its ancestors produced. Entries that
/// target the same container path replace the inherited one.
fn apply(paths: &Paths, name: &str, profile: ProfileFile, out: &mut ResolvedProfile) -> Result<()> {
    if let Some(script) = profile.extension {
        let script = paths.resolve_relative(&paths.profiles_dir(), &script);
        if !script.is_file() {
            bail!("extension script not found: {}", script.display());
        }
        out.extensions.push(Extension {
            profile: name.to_owned(),
            script,
        });
    }

    for entry in profile.mounts {
        validate_container_path(&entry.container)?;
        let host = paths.expand_tilde(&entry.host);
        if !host.is_absolute() {
            bail!(
                "mount host path {:?} must be absolute or start with ~",
                entry.host
            );
        }
        if host.to_string_lossy().contains([',', '\n']) {
            bail!(
                "mount host path {:?} must not contain ',' or newlines",
                entry.host
            );
        }
        out.mounts.retain(|m| m.container != entry.container);
        out.volumes.retain(|v| v.container != entry.container);
        if !host.exists() {
            out.warnings.push(format!(
                "{name}: mount source does not exist, skipping: {}",
                host.display()
            ));
            continue;
        }
        out.mounts.push(Mount {
            host,
            container: entry.container,
            readonly: entry.mode == Mode::Ro,
            profile: name.to_owned(),
        });
    }

    for entry in profile.volumes {
        validate_container_path(&entry.container)?;
        validate_volume_name(&entry.name)?;
        out.mounts.retain(|m| m.container != entry.container);
        out.volumes.retain(|v| v.container != entry.container);
        out.volumes.push(Volume {
            name: format!("podbay-{name}-{}", entry.name),
            container: entry.container,
            profile: name.to_owned(),
        });
    }

    for (tool, version) in profile.tools {
        out.tools.insert(tool, version);
    }

    for cap in &profile.cap_add {
        let cap = capability_name(cap)?;
        if !out.caps.iter().any(|c| c.name == cap) {
            out.caps.push(Capability {
                name: cap,
                profile: name.to_owned(),
            });
        }
    }

    profile.limits.validate()?;
    out.limits.merge(&profile.limits);
    Ok(())
}

/// Contents for `podbay profile init`.
pub fn template(name: &str, extends: &[String]) -> String {
    let extends = if extends.is_empty() {
        "# extends = [\"base\"]".to_owned()
    } else {
        let quoted: Vec<String> = extends.iter().map(|e| format!("{e:?}")).collect();
        format!("extends = [{}]", quoted.join(", "))
    };
    include_str!("templates/profile.toml")
        .replace("{name}", name)
        .replace("{extends}", &extends)
}

/// Writes a new profile file, refusing to overwrite an existing one.
pub fn init(paths: &Paths, name: &str, extends: &[String]) -> Result<PathBuf> {
    validate_name(name)?;
    for parent in extends {
        validate_name(parent)?;
    }
    let dir = paths.profiles_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let file = paths.profile_file(name);
    write_new(&file, &template(name, extends))?;
    Ok(file)
}

fn write_new(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| match path.exists() {
            true => format!("{} already exists, refusing to overwrite", path.display()),
            false => format!("creating {}", path.display()),
        })?;
    f.write_all(contents.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::test_paths;

    struct Fixture {
        _dir: tempfile::TempDir,
        paths: Paths,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let paths = test_paths(dir.path());
            fs::create_dir_all(paths.profiles_dir()).unwrap();
            fs::create_dir_all(&paths.home).unwrap();
            Self { _dir: dir, paths }
        }

        fn profile(&self, name: &str, body: &str) -> &Self {
            fs::write(self.paths.profile_file(name), body).unwrap();
            self
        }

        fn home_file(&self, rel: &str) -> PathBuf {
            let p = self.paths.home.join(rel);
            fs::write(&p, "").unwrap();
            p
        }
    }

    #[test]
    fn names() {
        for ok in ["ai", "web-dev", "a_b", "0x"] {
            validate_name(ok).unwrap();
        }
        for bad in ["", "AI", "../x", "-x", "a b", "a.b", &"x".repeat(65)] {
            assert!(validate_name(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn container_paths() {
        validate_container_path("/home/dev/.config/x").unwrap();
        for bad in ["relative", "/a,b", "/a:b", "/a/../b"] {
            assert!(validate_container_path(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn parses_full_profile() {
        let f = Fixture::new();
        let gitconfig = f.home_file(".gitconfig");
        fs::write(f.paths.profiles_dir().join("ai.sh"), "true").unwrap();
        f.profile(
            "ai",
            r#"
            extension = "ai.sh"
            [[mount]]
            host = "~/.gitconfig"   # comment
            container = "/home/dev/.gitconfig"

            [[mount]]
            host = "~/.gitconfig"
            container = "/home/dev/#notacomment"
            mode = 'rw'

            [[volume]]
            name = "oc"
            container = "/home/dev/.config/opencode"

            [tools]
            opencode = "latest"
            node = { version = "22" }
            "#,
        );
        let r = resolve(&f.paths, "ai").unwrap();
        assert_eq!(r.chain, ["ai"]);
        assert_eq!(r.mounts.len(), 2);
        assert_eq!(r.mounts[0].host, gitconfig);
        assert!(r.mounts[0].readonly);
        assert_eq!(r.mounts[1].container, "/home/dev/#notacomment");
        assert!(!r.mounts[1].readonly);
        assert_eq!(r.volumes[0].name, "podbay-ai-oc");
        assert_eq!(r.tools.len(), 2);
        assert_eq!(r.extensions[0].script, f.paths.profiles_dir().join("ai.sh"));
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn unknown_keys_and_bad_modes_are_errors() {
        let f = Fixture::new();
        f.profile("typo", "[[mount]]\nhost = \"/\"\ncontianer = \"/x\"\n");
        let err = format!("{:#}", resolve(&f.paths, "typo").unwrap_err());
        assert!(err.contains("contianer"), "{err}");

        f.profile(
            "mode",
            "[[mount]]\nhost = \"/\"\ncontainer = \"/x\"\nmode = \"wr\"\n",
        );
        assert!(resolve(&f.paths, "mode").is_err());
    }

    #[test]
    fn missing_host_path_warns() {
        let f = Fixture::new();
        f.profile("p", "[[mount]]\nhost = \"~/nope\"\ncontainer = \"/x\"\n");
        let r = resolve(&f.paths, "p").unwrap();
        assert!(r.mounts.is_empty());
        assert_eq!(r.warnings.len(), 1);
    }

    #[test]
    fn missing_extension_script_is_error() {
        let f = Fixture::new();
        f.profile("p", "extension = \"nope.sh\"\n");
        assert!(resolve(&f.paths, "p").is_err());
    }

    #[test]
    fn extends_merges_and_overrides() {
        let f = Fixture::new();
        f.home_file("a");
        f.home_file("b");
        f.profile(
            "base",
            r#"
            [[mount]]
            host = "~/a"
            container = "/shared"
            [[volume]]
            name = "cfg"
            container = "/cfg"
            [tools]
            node = "20"
            git = "latest"
            "#,
        )
        .profile(
            "child",
            r#"
            extends = ["base"]
            [[mount]]
            host = "~/b"
            container = "/shared"
            mode = "rw"
            [tools]
            node = "22"
            "#,
        );
        let r = resolve(&f.paths, "child").unwrap();
        assert_eq!(r.chain, ["base", "child"]);
        assert_eq!(r.mounts.len(), 1);
        assert_eq!(r.mounts[0].host, f.paths.home.join("b"));
        assert_eq!(r.mounts[0].profile, "child");
        // Volumes keep the namespace of the profile that declared them, so
        // every profile extending `base` shares the same volume.
        assert_eq!(r.volumes[0].name, "podbay-base-cfg");
        assert_eq!(r.tools["node"].as_str(), Some("22"));
        assert_eq!(r.tools["git"].as_str(), Some("latest"));
    }

    #[test]
    fn diamond_applies_shared_parent_once() {
        let f = Fixture::new();
        f.profile("root", "[tools]\nx = \"1\"\n")
            .profile("left", "extends = [\"root\"]\n")
            .profile("right", "extends = [\"root\"]\n")
            .profile("top", "extends = [\"left\", \"right\"]\n");
        let r = resolve(&f.paths, "top").unwrap();
        assert_eq!(r.chain, ["root", "left", "right", "top"]);
    }

    #[test]
    fn cycles_are_reported() {
        let f = Fixture::new();
        f.profile("a", "extends = [\"b\"]\n")
            .profile("b", "extends = [\"a\"]\n");
        let err = resolve(&f.paths, "a").unwrap_err().to_string();
        assert!(err.contains("a -> b -> a"), "{err}");
    }

    #[test]
    fn missing_parent_names_child() {
        let f = Fixture::new();
        f.profile("a", "extends = [\"ghost\"]\n");
        let err = resolve(&f.paths, "a").unwrap_err().to_string();
        assert!(
            err.contains("\"ghost\"") && err.contains("extended by \"a\""),
            "{err}"
        );
    }

    #[test]
    fn list_and_init() {
        let f = Fixture::new();
        f.profile("zeta", "").profile("alpha", "");
        fs::write(f.paths.profiles_dir().join("notes.txt"), "").unwrap();
        assert_eq!(list(&f.paths).unwrap(), ["alpha", "zeta"]);

        let file = init(&f.paths, "new", &["alpha".into()]).unwrap();
        let r = resolve(&f.paths, "new").unwrap();
        assert_eq!(r.chain, ["alpha", "new"]);
        assert!(
            init(&f.paths, "new", &[]).is_err(),
            "overwrote {}",
            file.display()
        );
    }

    #[test]
    fn caps_and_limits_merge_down_the_chain() {
        let f = Fixture::new();
        f.profile(
            "base",
            "cap_add = [\"sys_ptrace\"]\n[limits]\npids = 100\nmemory = \"4g\"\n",
        )
        .profile(
            "child",
            "extends = [\"base\"]\ncap_add = [\"CAP_SYS_PTRACE\", \"NET_RAW\"]\n\
             [limits]\nmemory = \"8g\"\n",
        );
        let r = resolve(&f.paths, "child").unwrap();
        let caps: Vec<(&str, &str)> = r
            .caps
            .iter()
            .map(|c| (c.name.as_str(), c.profile.as_str()))
            .collect();
        assert_eq!(caps, [("SYS_PTRACE", "base"), ("NET_RAW", "child")]);
        assert_eq!(r.limits.pids, Some(100));
        assert_eq!(r.limits.memory.as_deref(), Some("8g"));
    }

    #[test]
    fn unknown_capability_and_bad_limits_are_errors() {
        let f = Fixture::new();
        f.profile("caps", "cap_add = [\"SYS_EVERYTHING\"]\n");
        let err = format!("{:#}", resolve(&f.paths, "caps").unwrap_err());
        assert!(err.contains("SYS_EVERYTHING"), "{err}");
        f.profile("lim", "[limits]\ncpus = 0\n");
        assert!(resolve(&f.paths, "lim").is_err());
    }

    #[test]
    fn template_parses() {
        let parsed: ProfileFile = toml::from_str(&template("x", &[])).unwrap();
        assert!(parsed.extends.is_empty() && parsed.mounts.is_empty());
    }
}

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
//! shell = "zsh"                  # login shell for interactive sessions
//! path = ["/home/dev/.cargo/bin"] # prepended to PATH
//! network = ["api.example.com"]   # or "full"; default "none"
//!
//! [env]
//! RUSTUP_HOME = "/home/dev/.rustup"
//!
//! [[volume]]
//! name = "rustup"
//! container = "/home/dev/.rustup"
//! provision = true      # also mounted while provisioning
//!
//! [provision]
//! command = "rustup toolchain install"
//! watch = ["rust-toolchain.toml"]
//! ```

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::{Limits, NetworkSetting};
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
    /// Removed along with mise; kept only to explain what replaced it.
    tools: Option<toml::Value>,
    shell: Option<String>,
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    path: Vec<String>,
    provision: Option<ProvisionEntry>,
    /// Capabilities the session keeps, such as "SYS_PTRACE".
    #[serde(default)]
    cap_add: Vec<String>,
    #[serde(default)]
    limits: Limits,
    network: Option<NetworkSetting>,
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
    #[serde(default)]
    provision: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvisionEntry {
    command: String,
    #[serde(default)]
    watch: Vec<String>,
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
    /// Also mounted in the provisioning container, which installs into it.
    pub provision: bool,
    pub profile: String,
}

/// A provisioning command and the profile that declared it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionStep {
    pub command: String,
    pub profile: String,
}

/// A root build script layered onto the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extension {
    pub profile: String,
    pub script: PathBuf,
}

/// A profile with its `extends` chain flattened.
#[derive(Debug, Default, Clone)]
pub struct ResolvedProfile {
    /// Profiles in the order they were applied (ancestors first).
    pub chain: Vec<String>,
    pub mounts: Vec<Mount>,
    pub volumes: Vec<Volume>,
    /// Shell started when no command is given; the image's default if unset.
    pub shell: Option<String>,
    /// Environment for the session and provisioning.
    pub env: std::collections::BTreeMap<String, String>,
    /// Directories put in front of PATH, highest priority first.
    pub path: Vec<String>,
    /// Commands provisioning runs, ancestors first.
    pub provision: Vec<ProvisionStep>,
    /// Project-relative files and directories whose changes mean the
    /// provisioning commands must run again.
    pub watch: Vec<String>,
    pub extensions: Vec<Extension>,
    /// Capabilities added back for the session, in the order declared.
    pub caps: Vec<Capability>,
    /// Limits set by the chain; unset fields fall back to config.toml.
    pub limits: Limits,
    /// `network` settings down the chain, ancestors first, each with the
    /// profile that set it.
    pub network: Vec<(NetworkSetting, String)>,
    /// Non-fatal problems, such as a mount whose host path is missing.
    pub warnings: Vec<String>,
}

impl ResolvedProfile {
    /// What the provisioning container gets: the environment and the
    /// provision volumes, none of the bind mounts, other volumes or
    /// capabilities, and no shell.
    pub fn for_provisioning(&self) -> Self {
        Self {
            chain: self.chain.clone(),
            volumes: self
                .volumes
                .iter()
                .filter(|v| v.provision)
                .cloned()
                .collect(),
            env: self.env.clone(),
            path: self.path.clone(),
            ..Default::default()
        }
    }

    /// Gives every provision volume a per-project name (`--local-cache`), so
    /// a project's installs can't reach any other project.
    pub fn localize_provision_volumes(&mut self, repo_key: &str) {
        for v in self.volumes.iter_mut().filter(|v| v.provision) {
            v.name = format!("{}-{repo_key}", v.name);
        }
    }
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

/// Environment variable names, minus the ones podbay sets itself.
fn validate_env_name(name: &str) -> Result<()> {
    let valid = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        bail!("invalid environment variable name {name:?}");
    }
    if name == "PATH" {
        bail!("set PATH with `path = [...]`, which adds to it instead of replacing it");
    }
    if name.starts_with("PODBAY_") {
        bail!("{name} is reserved for podbay");
    }
    Ok(())
}

/// Paths in the project that provisioning watches: relative, and never
/// leaving the project.
fn validate_project_path(path: &str) -> Result<()> {
    if path.is_empty() || path.starts_with('/') {
        bail!("watch path {path:?} must be relative to the project");
    }
    if path.split('/').any(|seg| seg == "..") {
        bail!("watch path {path:?} must not contain '..'");
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
            provision: entry.provision,
            profile: name.to_owned(),
        });
    }

    if profile.tools.is_some() {
        bail!(
            "[tools] is gone: podbay no longer bundles mise. Install tools with a \
             [provision] command and keep them in a volume with provision = true"
        );
    }

    if let Some(shell) = profile.shell {
        if shell.is_empty() || shell.contains(char::is_whitespace) {
            bail!("shell {shell:?} must be a program name or path, without spaces");
        }
        out.shell = Some(shell);
    }

    for (key, value) in profile.env {
        validate_env_name(&key)?;
        out.env.insert(key, value);
    }

    // A profile's own directories go in front of inherited ones.
    for dir in profile.path.iter().rev() {
        validate_container_path(dir)?;
        out.path.retain(|d| d != dir);
        out.path.insert(0, dir.clone());
    }

    if let Some(provision) = profile.provision {
        if provision.command.trim().is_empty() {
            bail!("[provision] command must not be empty");
        }
        for rel in provision.watch {
            validate_project_path(&rel)?;
            if !out.watch.contains(&rel) {
                out.watch.push(rel);
            }
        }
        out.provision.push(ProvisionStep {
            command: provision.command,
            profile: name.to_owned(),
        });
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

    if let Some(network) = profile.network {
        out.network.push((network, name.to_owned()));
    }
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
            provision = true
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
        assert!(r.volumes[0].provision);
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
        assert!(!r.volumes[0].provision);
    }

    #[test]
    fn shell_env_path_and_provision_merge_down_the_chain() {
        let f = Fixture::new();
        f.profile(
            "base",
            r#"
            shell = "zsh"
            path = ["/opt/a", "/opt/shared"]
            [env]
            A = "1"
            B = "base"
            [provision]
            command = "install-a"
            watch = ["a.lock", "shared.lock"]
            "#,
        )
        .profile(
            "child",
            r#"
            extends = ["base"]
            shell = "fish"
            path = ["/opt/shared", "/opt/b"]
            [env]
            B = "child"
            [provision]
            command = "install-b"
            watch = ["shared.lock", "conf.d"]
            "#,
        );
        let r = resolve(&f.paths, "child").unwrap();
        assert_eq!(r.shell.as_deref(), Some("fish"));
        assert_eq!(r.path, ["/opt/shared", "/opt/b", "/opt/a"]);
        assert_eq!(r.env["A"], "1");
        assert_eq!(r.env["B"], "child");
        let steps: Vec<(&str, &str)> = r
            .provision
            .iter()
            .map(|s| (s.command.as_str(), s.profile.as_str()))
            .collect();
        assert_eq!(steps, [("install-a", "base"), ("install-b", "child")]);
        assert_eq!(r.watch, ["a.lock", "shared.lock", "conf.d"]);
    }

    #[test]
    fn rejects_bad_shell_env_path_and_watch() {
        let f = Fixture::new();
        for (name, body, needle) in [
            (
                "tools",
                "[tools]\nnode = \"22\"\n",
                "no longer bundles mise",
            ),
            ("shell", "shell = \"zsh -l\"\n", "without spaces"),
            ("pathenv", "[env]\nPATH = \"/x\"\n", "path = [...]"),
            ("reserved", "[env]\nPODBAY_UID = \"0\"\n", "reserved"),
            ("envname", "[env]\n\"A-B\" = \"1\"\n", "invalid environment"),
            ("relpath", "path = [\"bin\"]\n", "absolute"),
            ("colon", "path = [\"/a:/b\"]\n", "must not contain"),
            (
                "abswatch",
                "[provision]\ncommand = \"x\"\nwatch = [\"/etc\"]\n",
                "relative",
            ),
            (
                "upwatch",
                "[provision]\ncommand = \"x\"\nwatch = [\"../x\"]\n",
                "'..'",
            ),
            (
                "empty",
                "[provision]\ncommand = \" \"\n",
                "must not be empty",
            ),
        ] {
            f.profile(name, body);
            let err = format!("{:#}", resolve(&f.paths, name).unwrap_err());
            assert!(err.contains(needle), "{name}: {err}");
        }
    }

    #[test]
    fn diamond_applies_shared_parent_once() {
        let f = Fixture::new();
        f.profile("root", "[provision]\ncommand = \"x\"\n")
            .profile("left", "extends = [\"root\"]\n")
            .profile("right", "extends = [\"root\"]\n")
            .profile("top", "extends = [\"left\", \"right\"]\n");
        let r = resolve(&f.paths, "top").unwrap();
        assert_eq!(r.chain, ["root", "left", "right", "top"]);
        assert_eq!(r.provision.len(), 1);
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
    fn network_settings_are_kept_in_chain_order() {
        use crate::config::{Network, NetworkMode};
        let f = Fixture::new();
        f.profile("base", "network = [\"a.example.com\"]\n")
            .profile(
                "ai",
                "extends = [\"base\"]\nnetwork = [\"b.example.com:8443\"]\n",
            )
            .profile("open", "extends = [\"ai\"]\nnetwork = \"full\"\n");
        let r = resolve(&f.paths, "ai").unwrap();
        let names: Vec<&str> = r.network.iter().map(|(_, n)| n.as_str()).collect();
        assert_eq!(names, ["base", "ai"]);
        let mut net = Network::default();
        for (setting, _) in &r.network {
            net.apply(setting);
        }
        assert_eq!(net.describe(), "allow a.example.com, b.example.com:8443");

        let r = resolve(&f.paths, "open").unwrap();
        assert_eq!(
            r.network.last(),
            Some(&(NetworkSetting::Mode(NetworkMode::Full), "open".into()))
        );

        f.profile("bad", "network = \"lan\"\n");
        assert!(resolve(&f.paths, "bad").is_err());
        f.profile("ip", "network = [\"169.254.169.254\"]\n");
        let err = format!("{:#}", resolve(&f.paths, "ip").unwrap_err());
        assert!(err.contains("IP addresses"), "{err}");
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

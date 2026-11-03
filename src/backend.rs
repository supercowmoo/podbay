//! Container engines. Everything engine-specific lives behind [`Backend`] so
//! the rest of podbay can be tested without one, and so other runtimes can be
//! added later.

use std::env;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// The uid/gid the image's `dev` user is given inside the container.
pub const IMAGE_DEV_UID: u32 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Podman,
    Docker,
}

impl std::str::FromStr for BackendKind {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "podman" => Ok(Self::Podman),
            "docker" => Ok(Self::Docker),
            _ => Err(()),
        }
    }
}

impl BackendKind {
    pub fn program(self) -> &'static str {
        match self {
            Self::Podman => "podman",
            Self::Docker => "docker",
        }
    }

    /// Prefers podman (rootless by default) when both are installed.
    pub fn detect() -> Result<Self> {
        [Self::Podman, Self::Docker]
            .into_iter()
            .find(|k| on_path(k.program()))
            .context("neither podman nor docker found on PATH")
    }
}

fn on_path(program: &str) -> bool {
    env::var_os("PATH")
        .is_some_and(|path| env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// How the container's processes map onto host users.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// Extra `run` flags that set up the user namespace.
    pub flags: Vec<String>,
    /// The uid/gid the entrypoint gives `dev` before dropping root. Files
    /// `dev` creates in bind mounts must end up owned by the host user.
    /// 0 means the container's root already is the unprivileged host user
    /// (rootless engines without keep-id), so podbay-init stays root.
    pub uid: u32,
    pub gid: u32,
    /// Things the user should know, such as running a rootful daemon.
    pub notes: Vec<String>,
}

/// How far [`Backend::build_image`] goes to avoid reusing old content.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Refresh {
    /// Run every step again instead of taking it from the build cache.
    pub no_cache: bool,
    /// Pull the `FROM` image again first. Only for images that come from a
    /// registry: podbay's own layers exist only locally.
    pub pull: bool,
}

pub trait Backend {
    fn kind(&self) -> BackendKind;
    fn image_exists(&self, image: &str) -> Result<bool>;
    /// Builds `tag` from the `Containerfile` in `context`.
    fn build_image(&self, tag: &str, context: &Path, refresh: Refresh) -> Result<()>;
    /// Pulls `image` from its registry.
    fn pull_image(&self, image: &str) -> Result<()>;
    /// What `image` runs by default: its entrypoint followed by its command.
    fn default_command(&self, image: &str) -> Result<Vec<String>>;
    /// The file at `path` in `image`, or `None` if it has none there.
    fn read_file(&self, image: &str, path: &str) -> Result<Option<Vec<u8>>>;
    /// Every image's `name:tag`.
    fn list_images(&self) -> Result<Vec<String>>;
    /// Removes an image; fails if a container uses it.
    fn remove_image(&self, image: &str) -> Result<()>;
    /// Containers whose names start with `prefix`, with whether each runs.
    fn list_containers(&self, prefix: &str) -> Result<Vec<(String, bool)>>;
    fn remove_container(&self, name: &str) -> Result<()>;
    /// Every volume's name.
    fn list_volumes(&self) -> Result<Vec<String>>;
    fn identity(&self) -> Result<Identity>;
    /// cgroup controllers the engine can use for this user, such as
    /// `memory` and `pids`. `None` when the engine can't say, in which case
    /// limits are passed through and the engine reports any problem.
    fn cgroup_controllers(&self) -> Result<Option<Vec<String>>>;
}

/// An engine driven through its command-line interface.
pub struct Cli {
    kind: BackendKind,
    host_uid: u32,
    host_gid: u32,
}

impl Cli {
    pub fn new(kind: BackendKind) -> Self {
        Self {
            kind,
            // SAFETY: getuid and getgid always succeed and touch no memory.
            host_uid: unsafe { libc::getuid() },
            host_gid: unsafe { libc::getgid() },
        }
    }

    fn command(&self) -> Command {
        Command::new(self.kind.program())
    }

    /// Runs the engine and returns its stdout's non-empty lines.
    fn lines(&self, args: &[&str]) -> Result<Vec<String>> {
        let out = self
            .command()
            .args(args)
            .stderr(Stdio::inherit())
            .output()
            .with_context(|| format!("running `{} {}`", self.kind.program(), args[0]))?;
        if !out.status.success() {
            bail!("`{} {}` failed", self.kind.program(), args.join(" "));
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect())
    }

    /// Runs the engine with its output hidden; fails if it does.
    fn quiet(&self, args: &[&str]) -> Result<()> {
        let status = self
            .command()
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| format!("running `{} {}`", self.kind.program(), args[0]))?;
        if !status.success() {
            bail!("`{} {}` failed", self.kind.program(), args.join(" "));
        }
        Ok(())
    }

    fn docker_is_rootless(&self) -> Result<bool> {
        let out = self
            .command()
            .args(["info", "--format", "{{json .SecurityOptions}}"])
            .stderr(Stdio::inherit())
            .output()
            .context("running `docker info`")?;
        if !out.status.success() {
            bail!("`docker info` failed; is the docker daemon running?");
        }
        Ok(String::from_utf8_lossy(&out.stdout).contains("name=rootless"))
    }
}

impl Backend for Cli {
    fn kind(&self) -> BackendKind {
        self.kind
    }

    fn image_exists(&self, image: &str) -> Result<bool> {
        let status = self
            .command()
            .args(["image", "inspect", image])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| format!("running `{} image inspect`", self.kind.program()))?;
        Ok(status.success())
    }

    fn build_image(&self, tag: &str, context: &Path, refresh: Refresh) -> Result<()> {
        let mut cmd = self.command();
        cmd.args(["build", "--tag", tag]);
        if refresh.no_cache {
            cmd.arg("--no-cache");
        }
        if refresh.pull {
            // Both Podman and Docker read `--pull=true` as "always pull".
            cmd.arg("--pull=true");
        }
        // Build output goes to stderr so stdout stays clean for callers.
        let status = cmd
            .arg("--file")
            .arg(context.join("Containerfile"))
            .arg(context)
            .stdout(Stdio::from(std::io::stderr()))
            .status()
            .with_context(|| format!("running `{} build`", self.kind.program()))?;
        if !status.success() {
            bail!("building image {tag} failed");
        }
        Ok(())
    }

    fn pull_image(&self, image: &str) -> Result<()> {
        let status = self
            .command()
            .args(["pull", image])
            .stdout(Stdio::from(std::io::stderr()))
            .status()
            .with_context(|| format!("running `{} pull`", self.kind.program()))?;
        if !status.success() {
            bail!("pulling {image} failed");
        }
        Ok(())
    }

    fn default_command(&self, image: &str) -> Result<Vec<String>> {
        let out = self
            .command()
            .args(["image", "inspect", "--format", "{{json .Config}}", image])
            .stderr(Stdio::inherit())
            .output()
            .with_context(|| format!("running `{} image inspect`", self.kind.program()))?;
        if !out.status.success() {
            bail!("inspecting {image} failed");
        }
        parse_default_command(&out.stdout)
            .with_context(|| format!("reading the configuration of {image}"))
    }

    fn read_file(&self, image: &str, path: &str) -> Result<Option<Vec<u8>>> {
        // A stopped container is enough to copy from; the command is never
        // run, and images without one need something to create it with.
        let out = self
            .command()
            .args(["create", image, "/podbay-never-run"])
            .stderr(Stdio::inherit())
            .output()
            .with_context(|| format!("running `{} create`", self.kind.program()))?;
        if !out.status.success() {
            bail!("creating a container from {image} failed");
        }
        let id = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        let dir = crate::tempdir::tempdir()?;
        let dest = dir.path().join("file");
        let copied = self
            .command()
            .arg("cp")
            .arg(format!("{id}:{path}"))
            .arg(&dest)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self
            .command()
            .args(["rm", "--force", &id])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match copied {
            // A symbolic link copies as a link; only a regular file counts.
            Ok(status) if status.success() && dest.symlink_metadata()?.is_file() => {
                Ok(Some(std::fs::read(&dest)?))
            }
            Ok(_) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("running `{} cp`", self.kind.program())),
        }
    }

    fn list_images(&self) -> Result<Vec<String>> {
        Ok(self
            .lines(&["images", "--format", "{{.Repository}}:{{.Tag}}"])?
            .into_iter()
            .filter(|l| !l.contains("<none>"))
            .collect())
    }

    fn remove_image(&self, image: &str) -> Result<()> {
        self.quiet(&["rmi", image])
    }

    fn list_containers(&self, prefix: &str) -> Result<Vec<(String, bool)>> {
        let filter = format!("name={prefix}");
        Ok(self
            .lines(&[
                "ps",
                "--all",
                "--filter",
                &filter,
                "--format",
                "{{.Names}}\t{{.State}}",
            ])?
            .into_iter()
            .filter_map(|line| {
                let (name, state) = line.split_once('\t')?;
                // The filter matches anywhere in the name; keep prefixes.
                name.starts_with(prefix).then(|| {
                    (
                        name.to_owned(),
                        state.trim().eq_ignore_ascii_case("running"),
                    )
                })
            })
            .collect())
    }

    fn remove_container(&self, name: &str) -> Result<()> {
        self.quiet(&["rm", "--force", name])
    }

    fn list_volumes(&self) -> Result<Vec<String>> {
        self.lines(&["volume", "ls", "--format", "{{.Name}}"])
    }

    fn identity(&self) -> Result<Identity> {
        identity_for(self.kind, self.host_uid, self.host_gid, || {
            self.docker_is_rootless()
        })
    }

    fn cgroup_controllers(&self) -> Result<Option<Vec<String>>> {
        if self.kind != BackendKind::Podman {
            return Ok(None);
        }
        let out = self
            .command()
            .args(["info", "--format", "{{json .Host.CgroupControllers}}"])
            .stderr(Stdio::null())
            .output()
            .context("running `podman info`")?;
        if !out.status.success() {
            return Ok(None);
        }
        Ok(parse_json_strings(&String::from_utf8_lossy(&out.stdout)))
    }
}

/// The entrypoint and command from an image's `{{json .Config}}`; either may
/// be null.
fn parse_default_command(json: &[u8]) -> Result<Vec<String>> {
    let text = std::str::from_utf8(json).context("the engine's output isn't UTF-8")?;
    let config = crate::json::parse(text).map_err(anyhow::Error::msg)?;
    let list = |key: &str| {
        config[key]
            .as_strings()
            .with_context(|| format!("{key} isn't a list of strings"))
    };
    Ok([list("Entrypoint")?, list("Cmd")?].concat())
}

/// Parses a JSON array of plain strings, such as `["cpu","memory"]`. `null`
/// (podman's answer on cgroup v1) is an empty list.
fn parse_json_strings(text: &str) -> Option<Vec<String>> {
    let text = text.trim();
    if text == "null" {
        return Some(Vec::new());
    }
    let inner = text.strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    inner
        .split(',')
        .map(|item| {
            let item = item.trim().strip_prefix('"')?.strip_suffix('"')?;
            (!item.contains(['"', '\\'])).then(|| item.to_owned())
        })
        .collect()
}

/// Decides how users map, separated from the engine queries for testing.
fn identity_for(
    kind: BackendKind,
    host_uid: u32,
    host_gid: u32,
    docker_is_rootless: impl FnOnce() -> Result<bool>,
) -> Result<Identity> {
    let root = |notes: Vec<String>| Identity {
        flags: vec!["--user=0:0".into()],
        uid: 0,
        gid: 0,
        notes,
    };
    if host_uid == 0 {
        return Ok(root(vec![
            "running as root on the host; the container's root is the real root".into(),
        ]));
    }
    Ok(match kind {
        // keep-id maps the host user onto the image's dev uid, so files dev
        // writes in bind mounts are owned by the host user and no remapping
        // is needed. The container still starts as (namespaced) root so the
        // podbay-init can fix volume ownership before dropping privileges.
        BackendKind::Podman => Identity {
            flags: vec![
                format!("--userns=keep-id:uid={IMAGE_DEV_UID},gid={IMAGE_DEV_UID}"),
                "--user=0:0".into(),
            ],
            uid: IMAGE_DEV_UID,
            gid: IMAGE_DEV_UID,
            notes: vec![],
        },
        BackendKind::Docker => {
            if docker_is_rootless()? {
                // Container root is the host user; there is no keep-id
                // equivalent, so stay root inside the user namespace.
                root(vec![])
            } else {
                Identity {
                    flags: vec!["--user=0:0".into()],
                    uid: host_uid,
                    gid: host_gid,
                    notes: vec![
                        "docker daemon is rootful: a container escape is root on the host. \
                         Prefer podman or rootless docker."
                            .into(),
                    ],
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_commands() {
        let cmd = |json: &str| parse_default_command(json.as_bytes()).unwrap();
        assert_eq!(
            cmd(r#"{"Entrypoint":["tini","--"],"Cmd":["claude"],"Env":["A=b"]}"#),
            ["tini", "--", "claude"]
        );
        assert_eq!(cmd(r#"{"Entrypoint":null,"Cmd":["/hello"]}"#), ["/hello"]);
        assert!(cmd(r#"{"Entrypoint":null,"Cmd":null}"#).is_empty());
        assert!(cmd("{}").is_empty());
    }

    fn never() -> Result<bool> {
        panic!("should not query docker")
    }

    #[test]
    fn podman_rootless_uses_keep_id() {
        let id = identity_for(BackendKind::Podman, 1234, 1234, never).unwrap();
        assert!(id.flags[0].starts_with("--userns=keep-id:uid=1000"));
        assert_eq!((id.uid, id.gid), (1000, 1000));
        assert!(id.notes.is_empty());
    }

    #[test]
    fn rootful_docker_remaps_dev_to_host_user_and_warns() {
        let id = identity_for(BackendKind::Docker, 1234, 5678, || Ok(false)).unwrap();
        assert_eq!((id.uid, id.gid), (1234, 5678));
        assert_eq!(id.notes.len(), 1);
    }

    #[test]
    fn rootless_docker_stays_root() {
        let id = identity_for(BackendKind::Docker, 1234, 1234, || Ok(true)).unwrap();
        assert_eq!(id.uid, 0);
        assert!(id.notes.is_empty());
    }

    #[test]
    fn controller_lists() {
        assert_eq!(
            parse_json_strings("[\"cpu\", \"memory\",\"pids\"]\n"),
            Some(vec!["cpu".into(), "memory".into(), "pids".into()])
        );
        assert_eq!(parse_json_strings("null"), Some(vec![]));
        assert_eq!(parse_json_strings("[]"), Some(vec![]));
        assert_eq!(parse_json_strings("garbage"), None);
        assert_eq!(parse_json_strings("[\"a\\\"b\"]"), None);
    }

    #[test]
    fn host_root_stays_root() {
        for kind in [BackendKind::Podman, BackendKind::Docker] {
            assert_eq!(identity_for(kind, 0, 0, never).unwrap().uid, 0);
        }
    }
}

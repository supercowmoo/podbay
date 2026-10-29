//! Container engines. Everything engine-specific lives behind [`Backend`] so
//! the rest of podbay can be tested without one, and so other runtimes can be
//! added later.

use std::env;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::Deserialize;

/// The uid/gid the image's `dev` user is given inside the container.
pub const IMAGE_DEV_UID: u32 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    Podman,
    Docker,
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
    /// (rootless engines without keep-id), so the entrypoint stays root.
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
            host_uid: rustix::process::getuid().as_raw(),
            host_gid: rustix::process::getgid().as_raw(),
        }
    }

    fn command(&self) -> Command {
        Command::new(self.kind.program())
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
        // entrypoint can fix volume ownership before dropping privileges.
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

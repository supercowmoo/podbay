//! Turns a resolved profile into the exact `run` arguments for the engine.
//! Pure data in, pure data out: nothing here touches the engine, so the
//! command line can be checked in tests and shown with `--dry-run`.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::backend::Identity;
use crate::profile::{ResolvedProfile, validate_container_path};

pub const WORKSPACE: &str = "/home/dev/workspace/repomount";
pub const MISE_DATA: &str = "/home/dev/.local/share/mise";
pub const MISE_CONFIG: &str = "/home/dev/.config/mise/config.toml";
pub const SHARED_MISE_VOLUME: &str = "podbay-mise-data";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mount {
    Bind {
        source: PathBuf,
        target: String,
        readonly: bool,
    },
    Volume {
        name: String,
        target: String,
    },
}

impl Mount {
    fn target(&self) -> &str {
        match self {
            Self::Bind { target, .. } | Self::Volume { target, .. } => target,
        }
    }

    fn to_arg(&self) -> String {
        match self {
            Self::Bind {
                source,
                target,
                readonly,
            } => {
                let ro = if *readonly { ",readonly" } else { "" };
                format!(
                    "--mount=type=bind,source={},target={target}{ro}",
                    source.display()
                )
            }
            Self::Volume { name, target } => {
                format!("--mount=type=volume,source={name},target={target}")
            }
        }
    }
}

/// Where mise keeps installed tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MiseData {
    /// One volume shared by every project.
    Shared,
    /// A host directory private to one project (`--local-cache`).
    Local(PathBuf),
}

pub struct Inputs<'a> {
    pub image: String,
    pub project: PathBuf,
    pub repo_key: &'a str,
    pub profile: &'a ResolvedProfile,
    pub identity: &'a Identity,
    pub mise_data: MiseData,
    /// Generated mise config holding the profile's tools, if it has any.
    pub mise_config: Option<PathBuf>,
    pub command: Vec<String>,
    pub tty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPlan {
    pub image: String,
    pub flags: Vec<String>,
    pub mounts: Vec<Mount>,
    pub env: Vec<(String, String)>,
    pub command: Vec<String>,
}

impl RunPlan {
    pub fn build(inputs: Inputs<'_>) -> Result<Self> {
        let mut mounts = vec![Mount::Bind {
            source: inputs.project,
            target: WORKSPACE.into(),
            readonly: false,
        }];
        mounts.push(match inputs.mise_data {
            MiseData::Shared => Mount::Volume {
                name: SHARED_MISE_VOLUME.into(),
                target: MISE_DATA.into(),
            },
            MiseData::Local(dir) => Mount::Bind {
                source: dir,
                target: MISE_DATA.into(),
                readonly: false,
            },
        });
        if let Some(config) = inputs.mise_config {
            mounts.push(Mount::Bind {
                source: config,
                target: MISE_CONFIG.into(),
                readonly: true,
            });
        }
        for m in &inputs.profile.mounts {
            mounts.push(Mount::Bind {
                source: m.host.clone(),
                target: m.container.clone(),
                readonly: m.readonly,
            });
        }
        for v in &inputs.profile.volumes {
            mounts.push(Mount::Volume {
                name: v.name.clone(),
                target: v.container.clone(),
            });
        }

        let mut seen = HashSet::new();
        for m in &mounts {
            validate_container_path(m.target())?;
            if let Mount::Bind { source, .. } = m
                && source.to_string_lossy().contains([',', '\n'])
            {
                bail!(
                    "host path {} must not contain ',' or newlines",
                    source.display()
                );
            }
            if !seen.insert(m.target()) {
                bail!("{} is mounted more than once", m.target());
            }
        }

        // Directories the entrypoint must make writable for dev: engines may
        // create volume mount points (and their parents) as root.
        let writable: Vec<&str> = mounts
            .iter()
            .filter(|m| matches!(m, Mount::Volume { .. }) || m.target() == MISE_DATA)
            .map(Mount::target)
            .collect();

        let env = vec![
            ("PODBAY_UID".into(), inputs.identity.uid.to_string()),
            ("PODBAY_GID".into(), inputs.identity.gid.to_string()),
            ("PODBAY_WORKSPACE".into(), WORKSPACE.into()),
            ("PODBAY_VOLUME_PATHS".into(), writable.join(":")),
            ("PODBAY_PROFILE".into(), inputs.profile.chain.join(",")),
        ];

        let mut flags = vec!["--rm".into(), "--interactive".into()];
        if inputs.tty {
            flags.push("--tty".into());
        }
        flags.push(format!("--label=io.podbay.repo-key={}", inputs.repo_key));
        flags.extend(inputs.identity.flags.iter().cloned());

        Ok(Self {
            image: inputs.image,
            flags,
            mounts,
            env,
            command: inputs.command,
        })
    }

    /// Arguments after the engine's program name.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec!["run".to_owned()];
        args.extend(self.flags.iter().cloned());
        args.extend(self.mounts.iter().map(Mount::to_arg));
        args.extend(self.env.iter().map(|(k, v)| format!("--env={k}={v}")));
        args.push(self.image.clone());
        args.extend(self.command.iter().cloned());
        args
    }
}

/// Quotes `arg` for display as a POSIX shell word.
pub fn shell_quote(arg: &str) -> String {
    let safe = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,@+%".contains(c));
    if safe {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{Mount as ProfileMount, Volume};

    fn identity() -> Identity {
        Identity {
            flags: vec!["--user=0:0".into()],
            uid: 1234,
            gid: 1234,
            notes: vec![],
        }
    }

    fn inputs<'a>(profile: &'a ResolvedProfile, identity: &'a Identity) -> Inputs<'a> {
        Inputs {
            image: "img:1".into(),
            project: "/src/app".into(),
            repo_key: "app-0123",
            profile,
            identity,
            mise_data: MiseData::Shared,
            mise_config: None,
            command: vec![],
            tty: false,
        }
    }

    #[test]
    fn minimal_plan() {
        let profile = ResolvedProfile::default();
        let id = identity();
        let args = RunPlan::build(inputs(&profile, &id)).unwrap().args();
        assert_eq!(
            args,
            [
                "run",
                "--rm",
                "--interactive",
                "--label=io.podbay.repo-key=app-0123",
                "--user=0:0",
                "--mount=type=bind,source=/src/app,target=/home/dev/workspace/repomount",
                "--mount=type=volume,source=podbay-mise-data,target=/home/dev/.local/share/mise",
                "--env=PODBAY_UID=1234",
                "--env=PODBAY_GID=1234",
                "--env=PODBAY_WORKSPACE=/home/dev/workspace/repomount",
                "--env=PODBAY_VOLUME_PATHS=/home/dev/.local/share/mise",
                "--env=PODBAY_PROFILE=",
                "img:1",
            ]
        );
    }

    #[test]
    fn profile_mounts_volumes_and_command() {
        let profile = ResolvedProfile {
            chain: vec!["base".into(), "ai".into()],
            mounts: vec![ProfileMount {
                host: "/h/.gitconfig".into(),
                container: "/home/dev/.gitconfig".into(),
                readonly: true,
                profile: "base".into(),
            }],
            volumes: vec![Volume {
                name: "podbay-ai-oc".into(),
                container: "/home/dev/.config/opencode".into(),
                profile: "ai".into(),
            }],
            ..Default::default()
        };
        let id = identity();
        let mut i = inputs(&profile, &id);
        i.mise_data = MiseData::Local("/c/mise/app".into());
        i.mise_config = Some("/c/mise-config.toml".into());
        i.command = vec!["sh".into(), "-c".into(), "id".into()];
        i.tty = true;
        let plan = RunPlan::build(i).unwrap();
        let args = plan.args();

        assert!(args.contains(&"--tty".to_owned()));
        assert!(args.contains(
            &"--mount=type=bind,source=/c/mise/app,target=/home/dev/.local/share/mise".to_owned()
        ));
        assert!(args.contains(
            &"--mount=type=bind,source=/c/mise-config.toml,target=/home/dev/.config/mise/config.toml,readonly"
                .to_owned()
        ));
        assert!(
            args.contains(
                &"--mount=type=bind,source=/h/.gitconfig,target=/home/dev/.gitconfig,readonly"
                    .to_owned()
            )
        );
        assert!(
            args.contains(
                &"--env=PODBAY_VOLUME_PATHS=/home/dev/.local/share/mise:/home/dev/.config/opencode"
                    .to_owned()
            )
        );
        assert!(args.contains(&"--env=PODBAY_PROFILE=base,ai".to_owned()));
        assert_eq!(&args[args.len() - 4..], ["img:1", "sh", "-c", "id"]);
    }

    #[test]
    fn rejects_duplicate_targets() {
        let profile = ResolvedProfile {
            volumes: vec![Volume {
                name: "podbay-x-mise".into(),
                container: MISE_DATA.into(),
                profile: "x".into(),
            }],
            ..Default::default()
        };
        let id = identity();
        let err = RunPlan::build(inputs(&profile, &id)).unwrap_err();
        assert!(err.to_string().contains("more than once"), "{err}");
    }

    #[test]
    fn rejects_commas_in_host_paths() {
        let profile = ResolvedProfile::default();
        let id = identity();
        let mut i = inputs(&profile, &id);
        i.project = "/src/a,b".into();
        assert!(RunPlan::build(i).is_err());
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("--env=A=b"), "--env=A=b");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}

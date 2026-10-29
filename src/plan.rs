//! Turns a resolved profile into the exact `run` arguments for the engine.
//! Pure data in, pure data out: nothing here touches the engine, so the
//! command line can be checked in tests and shown with `--dry-run`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::backend::Identity;
use crate::config::Limits;
use crate::egress;
use crate::profile::{ResolvedProfile, validate_container_path};

pub const HOME: &str = "/home/dev";
pub const WORKSPACE: &str = "/home/dev/workspace/repomount";

/// What the entrypoint needs as root: set up the home directory, fix
/// ownership of mount points, then drop to dev and shed its capabilities
/// (SETPCAP, for the bounding set). The session itself gets none of these.
pub const ENTRYPOINT_CAPS: &[&str] = &[
    "CHOWN",
    "DAC_OVERRIDE",
    "FOWNER",
    "SETGID",
    "SETPCAP",
    "SETUID",
];

/// Writable scratch space on the otherwise read-only root filesystem. The
/// home directory is seeded from the image by the entrypoint.
const TMPFS: &[(&str, &str)] = &[
    (HOME, "rw,exec,nosuid,nodev,mode=0755"),
    ("/tmp", "rw,exec,nosuid,nodev,mode=1777"),
    ("/var/tmp", "rw,exec,nosuid,nodev,mode=1777"),
];

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

/// How the session reaches the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Net {
    /// Loopback only.
    None,
    /// The engine's default network.
    Full,
    /// Share the network namespace of this relay container, which has
    /// only loopback and podbay's proxy on it.
    Relay(String),
}

pub struct Inputs<'a> {
    pub image: String,
    pub project: PathBuf,
    /// Mount the project read-only, as provisioning does.
    pub project_readonly: bool,
    pub repo_key: &'a str,
    pub profile: &'a ResolvedProfile,
    pub identity: &'a Identity,
    /// What to run; empty runs the profile's shell, else the image's default.
    pub command: Vec<String>,
    pub tty: bool,
    /// Capabilities the session keeps, such as `SYS_PTRACE`.
    pub caps: &'a [String],
    /// Limits the engine can apply; see [`Limits`].
    pub limits: &'a Limits,
    pub network: Net,
    /// podbay's seccomp filter, or `None` for the engine's default.
    pub seccomp: Option<PathBuf>,
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
            readonly: inputs.project_readonly,
        }];
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

        let mut seen: HashSet<&str> = TMPFS.iter().map(|(target, _)| *target).collect();
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
            .filter(|m| matches!(m, Mount::Volume { .. }))
            .map(Mount::target)
            .collect();

        let mut env = vec![
            ("PODBAY_UID".into(), inputs.identity.uid.to_string()),
            ("PODBAY_GID".into(), inputs.identity.gid.to_string()),
            ("PODBAY_WORKSPACE".into(), WORKSPACE.into()),
            ("PODBAY_VOLUME_PATHS".into(), writable.join(":")),
            (
                "PODBAY_MOUNT_PATHS".into(),
                mounts
                    .iter()
                    .map(Mount::target)
                    .collect::<Vec<_>>()
                    .join(":"),
            ),
            (
                "PODBAY_CAPS".into(),
                inputs
                    .caps
                    .iter()
                    .map(|c| c.to_ascii_lowercase())
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            ("PODBAY_PROFILE".into(), inputs.profile.chain.join(",")),
        ];
        if !inputs.profile.path.is_empty() {
            // The entrypoint and login shells put this in front of PATH, so
            // the image's own PATH is kept whatever it is.
            env.push(("PODBAY_PATH".into(), inputs.profile.path.join(":")));
        }
        env.extend(
            inputs
                .profile
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        if let Net::Relay(_) = inputs.network {
            let proxy = format!("http://{}", egress::PROXY_ADDR);
            for var in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
                env.push((var.into(), proxy.clone()));
            }
            for var in ["NO_PROXY", "no_proxy"] {
                env.push((var.into(), "localhost,127.0.0.1,::1".into()));
            }
            // Node's built-in fetch ignores the variables above without it.
            env.push(("NODE_USE_ENV_PROXY".into(), "1".into()));
        }

        let mut flags = vec!["--rm".into(), "--interactive".into()];
        if inputs.tty {
            flags.push("--tty".into());
        }
        flags.push(format!("--label=io.podbay.repo-key={}", inputs.repo_key));
        for flag in &inputs.identity.flags {
            match &inputs.network {
                // A container may only set up a network namespace its own
                // user namespace owns, so share the relay's (created with
                // the same mapping) instead of getting a new one.
                Net::Relay(relay) if flag.starts_with("--userns=") => {
                    flags.push(format!("--userns=container:{relay}"));
                }
                _ => flags.push(flag.clone()),
            }
        }

        let mut caps: Vec<&str> = ENTRYPOINT_CAPS.to_vec();
        caps.extend(inputs.caps.iter().map(String::as_str));
        caps.sort_unstable();
        caps.dedup();
        // One flag per capability: docker reads a comma list as one name.
        flags.push("--cap-drop=ALL".into());
        flags.extend(caps.iter().map(|c| format!("--cap-add={c}")));
        flags.push("--security-opt=no-new-privileges".into());
        if let Some(file) = &inputs.seccomp {
            flags.push(seccomp_flag(file)?);
        }
        flags.push("--read-only".into());
        for (target, options) in TMPFS {
            flags.push(format!("--tmpfs={target}:{options}"));
        }
        match &inputs.network {
            Net::None => flags.push("--network=none".into()),
            Net::Full => {}
            Net::Relay(container) => flags.push(format!("--network=container:{container}")),
        }

        match inputs.limits.pids {
            Some(0) => flags.push("--pids-limit=-1".into()),
            Some(n) => flags.push(format!("--pids-limit={n}")),
            None => {}
        }
        if let Some(memory) = &inputs.limits.memory {
            flags.push(format!("--memory={memory}"));
        }
        if let Some(cpus) = inputs.limits.cpus {
            flags.push(format!("--cpus={cpus}"));
        }

        let command = match (inputs.command.is_empty(), &inputs.profile.shell) {
            (true, Some(shell)) => vec![shell.clone(), "-l".into()],
            _ => inputs.command,
        };

        Ok(Self {
            image: inputs.image,
            flags,
            mounts,
            env,
            command,
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

fn seccomp_flag(file: &Path) -> Result<String> {
    let file = file.to_string_lossy();
    if file.contains([',', '\n']) {
        bail!("seccomp profile path {file} must not contain ',' or newlines");
    }
    Ok(format!("--security-opt=seccomp={file}"))
}

/// Where the relay sees the directory holding podbay's socket.
const RELAY_SOCKET_DIR: &str = "/run/podbay";

pub struct RelayInputs<'a> {
    pub image: &'a str,
    pub name: &'a str,
    /// Host directory holding podbay's policy socket.
    pub socket_dir: &'a Path,
    pub identity: &'a Identity,
    pub repo_key: &'a str,
    pub limits: &'a Limits,
    /// podbay's seccomp filter, or `None` for the engine's default.
    pub seccomp: Option<&'a Path>,
}

/// `run` arguments for the relay container of an allowlisted session. It
/// has no network, no capabilities and nothing writable, and runs as the
/// user that owns podbay's socket so it can connect to it.
pub fn relay_args(inputs: &RelayInputs<'_>) -> Result<Vec<String>> {
    let dir = inputs.socket_dir.to_string_lossy();
    if dir.contains([',', '\n']) {
        bail!("socket directory {dir} must not contain ',' or newlines");
    }
    let mut args: Vec<String> = vec![
        "run".into(),
        "--detach".into(),
        "--rm".into(),
        format!("--name={}", inputs.name),
        format!("--label=io.podbay.repo-key={}", inputs.repo_key),
        "--network=none".into(),
        "--read-only".into(),
        "--cap-drop=ALL".into(),
        "--security-opt=no-new-privileges".into(),
    ];
    if let Some(file) = inputs.seccomp {
        args.push(seccomp_flag(file)?);
    }
    args.extend([
        // It holds no state, and as PID 1 without handlers it would ignore
        // SIGTERM, making engines wait before removing it.
        "--stop-signal=SIGKILL".into(),
    ]);
    // The session's user namespace setup, but never root inside it unless
    // root already is the host user.
    args.extend(
        inputs
            .identity
            .flags
            .iter()
            .filter(|f| !f.starts_with("--user="))
            .cloned(),
    );
    args.push(format!(
        "--user={}:{}",
        inputs.identity.uid, inputs.identity.gid
    ));
    if inputs.limits.pids.is_some_and(|n| n > 0) {
        args.push("--pids-limit=512".into());
    }
    args.push(format!(
        "--mount=type=bind,source={dir},target={RELAY_SOCKET_DIR},readonly"
    ));
    args.push(inputs.image.into());
    args.push(egress::PROXY_ADDR.into());
    args.push(format!("{RELAY_SOCKET_DIR}/{}", egress::SOCKET_NAME));
    Ok(args)
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
            project_readonly: false,
            repo_key: "app-0123",
            profile,
            identity,
            command: vec![],
            tty: false,
            caps: &[],
            limits: &NO_LIMITS,
            network: Net::None,
            seccomp: None,
        }
    }

    static NO_LIMITS: Limits = Limits {
        pids: None,
        memory: None,
        cpus: None,
    };

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
                "--cap-drop=ALL",
                "--cap-add=CHOWN",
                "--cap-add=DAC_OVERRIDE",
                "--cap-add=FOWNER",
                "--cap-add=SETGID",
                "--cap-add=SETPCAP",
                "--cap-add=SETUID",
                "--security-opt=no-new-privileges",
                "--read-only",
                "--tmpfs=/home/dev:rw,exec,nosuid,nodev,mode=0755",
                "--tmpfs=/tmp:rw,exec,nosuid,nodev,mode=1777",
                "--tmpfs=/var/tmp:rw,exec,nosuid,nodev,mode=1777",
                "--network=none",
                "--mount=type=bind,source=/src/app,target=/home/dev/workspace/repomount",
                "--env=PODBAY_UID=1234",
                "--env=PODBAY_GID=1234",
                "--env=PODBAY_WORKSPACE=/home/dev/workspace/repomount",
                "--env=PODBAY_VOLUME_PATHS=",
                "--env=PODBAY_MOUNT_PATHS=/home/dev/workspace/repomount",
                "--env=PODBAY_CAPS=",
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
                provision: false,
                profile: "ai".into(),
            }],
            shell: Some("zsh".into()),
            ..Default::default()
        };
        let id = identity();
        let mut i = inputs(&profile, &id);
        i.command = vec!["sh".into(), "-c".into(), "id".into()];
        i.tty = true;
        let plan = RunPlan::build(i).unwrap();
        let args = plan.args();

        assert!(args.contains(&"--tty".to_owned()));
        assert!(
            args.contains(
                &"--mount=type=bind,source=/h/.gitconfig,target=/home/dev/.gitconfig,readonly"
                    .to_owned()
            )
        );
        assert!(args.contains(&"--env=PODBAY_VOLUME_PATHS=/home/dev/.config/opencode".to_owned()));
        assert!(args.contains(&"--env=PODBAY_PROFILE=base,ai".to_owned()));
        // An explicit command wins over the profile's shell.
        assert_eq!(&args[args.len() - 4..], ["img:1", "sh", "-c", "id"]);
        let args = RunPlan::build(inputs(&profile, &id)).unwrap().args();
        assert_eq!(&args[args.len() - 3..], ["img:1", "zsh", "-l"]);
    }

    #[test]
    fn seccomp_filter() {
        let profile = ResolvedProfile::default();
        let id = identity();
        let mut i = inputs(&profile, &id);
        i.seccomp = Some("/c/seccomp/ab.json".into());
        let args = RunPlan::build(i).unwrap().args();
        let nnp = args
            .iter()
            .position(|a| a == "--security-opt=no-new-privileges")
            .unwrap();
        assert_eq!(args[nnp + 1], "--security-opt=seccomp=/c/seccomp/ab.json");

        let mut i = inputs(&profile, &id);
        i.seccomp = Some("/c/a,b.json".into());
        assert!(RunPlan::build(i).is_err());
    }

    #[test]
    fn env_and_path() {
        let profile = ResolvedProfile {
            env: [("CARGO_HOME".to_owned(), "/home/dev/.cargo".to_owned())].into(),
            path: vec!["/home/dev/.cargo/bin".into(), "/opt/bin".into()],
            ..Default::default()
        };
        let id = identity();
        let args = RunPlan::build(inputs(&profile, &id)).unwrap().args();
        for expected in [
            "--env=PODBAY_PATH=/home/dev/.cargo/bin:/opt/bin",
            "--env=CARGO_HOME=/home/dev/.cargo",
        ] {
            assert!(
                args.contains(&expected.to_owned()),
                "missing {expected}: {args:?}"
            );
        }
        assert!(
            !args.iter().any(|a| a.starts_with("--env=PATH=")),
            "{args:?}"
        );
    }

    #[test]
    fn caps_and_limits() {
        let profile = ResolvedProfile::default();
        let id = identity();
        let caps = vec!["SYS_PTRACE".to_owned(), "CHOWN".to_owned()];
        let limits = Limits {
            pids: Some(0),
            memory: Some("8g".into()),
            cpus: Some(1.5),
        };
        let mut i = inputs(&profile, &id);
        i.caps = &caps;
        i.limits = &limits;
        let args = RunPlan::build(i).unwrap().args();
        let chown = args.iter().filter(|a| *a == "--cap-add=CHOWN").count();
        assert_eq!(chown, 1, "{args:?}");
        for expected in [
            "--cap-add=SYS_PTRACE",
            "--env=PODBAY_CAPS=sys_ptrace,chown",
            "--pids-limit=-1",
            "--memory=8g",
            "--cpus=1.5",
        ] {
            assert!(
                args.contains(&expected.to_owned()),
                "missing {expected}: {args:?}"
            );
        }
    }

    #[test]
    fn full_network_and_readonly_project() {
        let profile = ResolvedProfile::default();
        let id = identity();
        let mut i = inputs(&profile, &id);
        i.network = Net::Full;
        i.project_readonly = true;
        let args = RunPlan::build(i).unwrap().args();
        assert!(!args.iter().any(|a| a.starts_with("--network")), "{args:?}");
        assert!(
            args.contains(
                &"--mount=type=bind,source=/src/app,target=/home/dev/workspace/repomount,readonly"
                    .to_owned()
            )
        );
    }

    #[test]
    fn relay_network_sets_the_proxy() {
        let profile = ResolvedProfile::default();
        let id = identity();
        let mut i = inputs(&profile, &id);
        i.network = Net::Relay("podbay-relay-app-1".into());
        let args = RunPlan::build(i).unwrap().args();
        for expected in [
            "--network=container:podbay-relay-app-1",
            "--env=HTTPS_PROXY=http://127.0.0.1:3128",
            "--env=http_proxy=http://127.0.0.1:3128",
            "--env=NO_PROXY=localhost,127.0.0.1,::1",
        ] {
            assert!(
                args.contains(&expected.to_owned()),
                "missing {expected}: {args:?}"
            );
        }
        assert!(!args.contains(&"--network=none".to_owned()));
    }

    #[test]
    fn relay_sessions_share_its_user_namespace() {
        let profile = ResolvedProfile::default();
        let id = Identity {
            flags: vec![
                "--userns=keep-id:uid=1000,gid=1000".into(),
                "--user=0:0".into(),
            ],
            uid: 1000,
            gid: 1000,
            notes: vec![],
        };
        let mut i = inputs(&profile, &id);
        i.network = Net::Relay("relay-1".into());
        let args = RunPlan::build(i).unwrap().args();
        assert!(
            args.contains(&"--userns=container:relay-1".to_owned()),
            "{args:?}"
        );
        assert!(
            !args.iter().any(|a| a.starts_with("--userns=keep-id")),
            "{args:?}"
        );
        assert!(args.contains(&"--user=0:0".to_owned()), "{args:?}");
    }

    #[test]
    fn relay_container() {
        let podman = Identity {
            flags: vec![
                "--userns=keep-id:uid=1000,gid=1000".into(),
                "--user=0:0".into(),
            ],
            uid: 1000,
            gid: 1000,
            notes: vec![],
        };
        let limits = Limits {
            pids: Some(4096),
            ..NO_LIMITS.clone()
        };
        let args = relay_args(&RelayInputs {
            image: "podbay-relay:1",
            name: "podbay-relay-app-1",
            socket_dir: Path::new("/run/user/1000/podbay-x"),
            identity: &podman,
            repo_key: "app-1",
            limits: &limits,
            seccomp: Some(Path::new("/c/seccomp/relay.json")),
        })
        .unwrap();
        assert_eq!(
            args,
            [
                "run",
                "--detach",
                "--rm",
                "--name=podbay-relay-app-1",
                "--label=io.podbay.repo-key=app-1",
                "--network=none",
                "--read-only",
                "--cap-drop=ALL",
                "--security-opt=no-new-privileges",
                "--security-opt=seccomp=/c/seccomp/relay.json",
                "--stop-signal=SIGKILL",
                "--userns=keep-id:uid=1000,gid=1000",
                "--user=1000:1000",
                "--pids-limit=512",
                "--mount=type=bind,source=/run/user/1000/podbay-x,target=/run/podbay,readonly",
                "podbay-relay:1",
                "127.0.0.1:3128",
                "/run/podbay/egress.sock",
            ]
        );
    }

    #[test]
    fn rejects_mounts_over_tmpfs() {
        let profile = ResolvedProfile {
            volumes: vec![Volume {
                name: "podbay-x-tmp".into(),
                container: "/tmp".into(),
                provision: false,
                profile: "x".into(),
            }],
            ..Default::default()
        };
        let id = identity();
        assert!(RunPlan::build(inputs(&profile, &id)).is_err());
    }

    #[test]
    fn rejects_duplicate_targets() {
        let profile = ResolvedProfile {
            volumes: vec![Volume {
                name: "podbay-x-src".into(),
                container: WORKSPACE.into(),
                provision: false,
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

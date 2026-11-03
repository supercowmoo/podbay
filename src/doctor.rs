//! `podbay doctor`: checks that this machine can run podbay sessions as
//! intended, and says what to change when it can't.
//!
//! Each check is `ok`, `warn` (works, with less protection or a missing
//! feature) or `fail` (sessions won't work). Exit status 1 if anything
//! failed.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::backend::{Backend, BackendKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug)]
pub struct Check {
    pub level: Level,
    pub what: &'static str,
    pub detail: String,
    /// What to do about it, for warn and fail.
    pub hint: Option<String>,
}

fn check(level: Level, what: &'static str, detail: impl Into<String>) -> Check {
    Check {
        level,
        what,
        detail: detail.into(),
        hint: None,
    }
}

impl Check {
    fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// Renders the checks, one per line, hints indented below.
pub fn render(checks: &[Check]) -> String {
    let mut out = String::new();
    for c in checks {
        let tag = match c.level {
            Level::Ok => "ok  ",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
        };
        // Multi-line details, such as a TOML error, stay under their line.
        let detail = c.detail.trim_end().replace('\n', "\n      ");
        out.push_str(&format!("{tag}  {}: {detail}\n", c.what));
        if let Some(hint) = &c.hint {
            out.push_str(&format!("      {hint}\n"));
        }
    }
    let worst = checks.iter().map(|c| c.level).max().unwrap_or(Level::Ok);
    out.push_str(match worst {
        Level::Ok => "\nAll good.\n",
        Level::Warn => "\nSessions will work; the warnings above cost protection or features.\n",
        Level::Fail => "\nSessions won't work until the failures above are fixed.\n",
    });
    out
}

/// Runs `program args` and returns its trimmed stdout, if it succeeded.
fn output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Checks about the container engine.
pub fn engine(backend: &dyn Backend, source: &str) -> Vec<Check> {
    let kind = backend.kind();
    let program = kind.program();
    let mut checks = Vec::new();
    if output(program, &["info"]).is_none() {
        checks.push(
            check(
                Level::Fail,
                "engine",
                format!("{program} (from {source}) isn't working: `{program} info` failed"),
            )
            .hint(match kind {
                BackendKind::Docker => {
                    "start the docker daemon, and make sure your user may use it"
                }
                BackendKind::Podman => "run `podman info` to see why",
            }),
        );
        return checks;
    }
    let version = match kind {
        BackendKind::Podman => output(program, &["version", "--format", "{{.Client.Version}}"]),
        BackendKind::Docker => output(program, &["version", "--format", "{{.Server.Version}}"]),
    }
    .unwrap_or_else(|| "unknown version".into());

    match backend.identity() {
        Ok(identity) if identity.notes.is_empty() => {
            let mode = match kind {
                BackendKind::Podman => "rootless",
                BackendKind::Docker => "rootless daemon",
            };
            checks.push(check(
                Level::Ok,
                "engine",
                format!("{program} {version} (from {source}), {mode}"),
            ));
        }
        Ok(identity) => {
            checks.push(
                check(
                    Level::Warn,
                    "engine",
                    format!(
                        "{program} {version} (from {source}): {}",
                        identity.notes.join("; ")
                    ),
                )
                .hint("rootless Podman keeps a container escape from being root on the host"),
            );
        }
        Err(e) => checks.push(check(Level::Fail, "engine", format!("{e:#}"))),
    }

    if kind == BackendKind::Podman {
        match backend.cgroup_controllers() {
            Ok(Some(have)) => {
                let missing: Vec<&str> = ["pids", "memory", "cpu"]
                    .into_iter()
                    .filter(|c| !have.iter().any(|h| h == c))
                    .collect();
                if missing.is_empty() {
                    checks.push(check(
                        Level::Ok,
                        "limits",
                        "pids, memory and cpu limits apply",
                    ));
                } else {
                    checks.push(
                        check(
                            Level::Warn,
                            "limits",
                            format!(
                                "podman can't apply {} limits for your user",
                                missing.join(", ")
                            ),
                        )
                        .hint(
                            "systemd must delegate those cgroup controllers to your user \
                             (cgroups v2, `Delegate=` for user@.service)",
                        ),
                    );
                }
            }
            _ => checks.push(check(
                Level::Warn,
                "limits",
                "podman didn't say which limits it can apply",
            )),
        }
    }
    checks
}

/// `/etc/subuid` or `/etc/subgid` text has a range for `user` or `uid`.
pub fn has_subid_range(text: &str, user: &str, uid: u32) -> bool {
    text.lines().any(|line| {
        let mut fields = line.split(':');
        let owner = fields.next().unwrap_or_default();
        let count = fields.nth(1).and_then(|c| c.trim().parse::<u64>().ok());
        (owner == user || owner == uid.to_string()) && count.is_some_and(|c| c >= 65536)
    })
}

/// Checks about this machine that rootless sessions depend on.
pub fn host(kind: BackendKind) -> Vec<Check> {
    let mut checks = Vec::new();
    // SAFETY: getuid always succeeds. (Running as root is reported with
    // the engine.)
    let uid = unsafe { libc::getuid() };

    let seccomp = fs::read_to_string("/proc/self/status")
        .is_ok_and(|s| s.lines().any(|l| l.starts_with("Seccomp:")));
    checks.push(match seccomp {
        true => check(Level::Ok, "seccomp", "the kernel supports seccomp filters"),
        false => check(
            Level::Fail,
            "seccomp",
            "the kernel doesn't support seccomp filters",
        )
        .hint("podbay runs every container under its seccomp filter; use a kernel with seccomp"),
    });

    if kind == BackendKind::Podman && uid != 0 {
        let user = std::env::var("USER").unwrap_or_default();
        for (file, what) in [("/etc/subuid", "subuid"), ("/etc/subgid", "subgid")] {
            let ok = fs::read_to_string(file).is_ok_and(|t| has_subid_range(&t, &user, uid));
            checks.push(match ok {
                true => check(Level::Ok, what, format!("{file} has a range for you")),
                false => check(
                    Level::Fail,
                    what,
                    format!("{file} has no range of 65536 ids for {user}"),
                )
                .hint(format!(
                    "rootless podman needs one: `sudo usermod --add-subuids 100000-165535 \
                         --add-subgids 100000-165535 {user}`"
                )),
            });
        }
        let userns = fs::read_to_string("/proc/sys/user/max_user_namespaces")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok());
        if userns == Some(0) {
            checks.push(
                check(
                    Level::Fail,
                    "user namespaces",
                    "disabled (user.max_user_namespaces = 0)",
                )
                .hint("rootless podman needs them: `sysctl user.max_user_namespaces=15000`"),
            );
        }
        let tun = Path::new("/dev/net/tun");
        let path = std::ffi::CString::new("/dev/net/tun").expect("no NUL");
        // SAFETY: a valid C string; access only reads it.
        let usable = unsafe { libc::access(path.as_ptr(), libc::R_OK | libc::W_OK) } == 0;
        checks.push(match (tun.exists(), usable) {
            (true, true) => check(
                Level::Ok,
                "network",
                "/dev/net/tun is usable for rootless networking",
            ),
            _ => check(
                Level::Warn,
                "network",
                "/dev/net/tun isn't usable by your user",
            )
            .hint(
                "rootless podman needs it for containers with network access: provisioning, \
                 network = \"full\", and image builds",
            ),
        });
    }

    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if Path::new(&dir).is_dir() => checks.push(check(
            Level::Ok,
            "runtime dir",
            format!("{} for podbay's sockets", Path::new(&dir).display()),
        )),
        _ => checks.push(
            check(
                Level::Warn,
                "runtime dir",
                "XDG_RUNTIME_DIR isn't set, so podbay's sockets go in the temporary directory",
            )
            .hint("log in through a systemd session (or set XDG_RUNTIME_DIR) to keep them private to you"),
        ),
    }
    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subid_ranges() {
        let text = "alice:100000:65536\nbob:165536:1000\n1500:200000:65536\n";
        assert!(has_subid_range(text, "alice", 1000));
        assert!(!has_subid_range(text, "bob", 1001), "too small");
        assert!(has_subid_range(text, "carol", 1500), "by uid");
        assert!(!has_subid_range(text, "dave", 1600));
        assert!(!has_subid_range("", "alice", 1000));
    }

    #[test]
    fn rendering_and_verdict() {
        let checks = [
            check(Level::Ok, "engine", "podman 5"),
            check(Level::Warn, "limits", "no pids").hint("delegate it"),
        ];
        let out = render(&checks);
        assert!(
            out.starts_with("ok    engine: podman 5\nwarn  limits: no pids\n      delegate it\n"),
            "{out}"
        );
        assert!(
            out.ends_with("Sessions will work; the warnings above cost protection or features.\n")
        );
        assert!(render(&[check(Level::Fail, "x", "y")]).contains("won't work"));
        assert!(render(&[check(Level::Ok, "x", "y")]).ends_with("All good.\n"));
    }
}

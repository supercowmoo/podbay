//! End-to-end runs against a real engine. Skipped when the engine is not
//! available. Select it with PODBAY_TEST_BACKEND (default: docker).
//!
//! Uses a lightweight stand-in for the bundled base image (same entrypoint,
//! no toolchain) so the test needs no network beyond pulling Debian.

use std::fs;
use std::io::Read;
use std::net::TcpStream;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use assert_cmd::Command;

const TEST_BASE: &str = "podbay-e2e-base:latest";

fn engine() -> Option<String> {
    let engine = std::env::var("PODBAY_TEST_BACKEND").unwrap_or_else(|_| "docker".into());
    let ok = StdCommand::new(&engine)
        .arg("info")
        .output()
        .is_ok_and(|o| o.status.success());
    if ok {
        Some(engine)
    } else {
        eprintln!("skipping: {engine} is not available");
        None
    }
}

fn build_test_base(engine: &str) {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let ctx = tempfile::tempdir().unwrap();
    fs::copy(
        repo.join("tests/fixtures/test-base/Containerfile"),
        ctx.path().join("Containerfile"),
    )
    .unwrap();
    fs::copy(
        repo.join("base/entrypoint.sh"),
        ctx.path().join("entrypoint.sh"),
    )
    .unwrap();
    fs::copy(
        repo.join("tests/fixtures/test-base/probe-install"),
        ctx.path().join("probe-install"),
    )
    .unwrap();
    let status = StdCommand::new(engine)
        .args(["build", "--quiet", "--tag", TEST_BASE, "--file"])
        .arg(ctx.path().join("Containerfile"))
        .arg(ctx.path())
        .status()
        .unwrap();
    assert!(status.success(), "building the test base image failed");
}

/// podbay with its config and cache under `root`. HOME and XDG_* are left
/// alone: rootless podman finds its image store through them.
fn podbay(root: &Path) -> Command {
    let mut cmd = Command::cargo_bin("podbay").unwrap();
    cmd.env("PODBAY_CONFIG_DIR", root.join("config"))
        .env("PODBAY_CACHE_DIR", root.join("cache"));
    cmd
}

/// Removes an engine object (`volume` or `image`) when dropped.
struct Cleanup<'a> {
    engine: &'a str,
    kind: &'static str,
    name: String,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let _ = StdCommand::new(self.engine)
            .args([self.kind, "rm", "--force", &self.name])
            .output();
    }
}

/// Removes every volume whose name starts with `prefix` when dropped, for
/// the per-project copies `--local-cache` makes.
struct VolumeCleanup<'a> {
    engine: &'a str,
    prefix: String,
}

impl Drop for VolumeCleanup<'_> {
    fn drop(&mut self) {
        let Ok(out) = StdCommand::new(self.engine)
            .args(["volume", "ls", "--quiet"])
            .output()
        else {
            return;
        };
        for name in String::from_utf8_lossy(&out.stdout).lines() {
            if name.starts_with(&self.prefix) {
                let _ = StdCommand::new(self.engine)
                    .args(["volume", "rm", "--force", name])
                    .output();
            }
        }
    }
}

#[test]
fn session_round_trip() {
    let Some(engine) = engine() else { return };
    build_test_base(&engine);

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let home = root.join("home");
    let config = root.join("config");
    let profiles = config.join("profiles");
    let project: PathBuf = root.join("project");
    for d in [&home, &profiles, &project] {
        fs::create_dir_all(d).unwrap();
    }

    // Unique names so parallel or repeated runs do not share volumes.
    let id = format!("e2e{}", std::process::id());
    let _volume = Cleanup {
        engine: &engine,
        kind: "volume",
        name: format!("podbay-{id}-state"),
    };

    fs::write(home.join(".gitconfig"), "[user]\n\tname = test\n").unwrap();
    fs::write(
        config.join("config.toml"),
        format!("base_image = {TEST_BASE:?}\n"),
    )
    .unwrap();
    fs::write(
        profiles.join(format!("{id}.sh")),
        "#!/bin/sh\nset -e\necho from-extension > /etc/podbay-marker\n",
    )
    .unwrap();
    fs::write(
        profiles.join(format!("{id}.toml")),
        format!(
            r#"
extension = "{id}.sh"
path = ["/opt/e2e/bin"]

[env]
E2E_VAR = "hello"

[[mount]]
host = {gitconfig:?}
container = "/home/dev/.gitconfig"

[[volume]]
name = "state"
container = "/home/dev/.config/tool/state"
"#,
            gitconfig = home.join(".gitconfig"),
        ),
    )
    .unwrap();

    let script = r#"
set -e
echo "pwd=$PWD"
echo "home=$HOME"
echo "marker=$(cat /etc/podbay-marker)"
grep -q 'name = test' ~/.gitconfig && echo gitconfig=readable
if (echo x >> ~/.gitconfig) 2>/dev/null; then echo gitconfig=writable; else echo gitconfig=readonly; fi
touch ~/.config/tool/state/ok && echo volume=writable
echo "var=$E2E_VAR"
case "$PATH" in /opt/e2e/bin:*) echo path=prepended ;; *) echo "path=$PATH" ;; esac
bash -lc 'case "$PATH" in /opt/e2e/bin:*) echo login-path=prepended ;; *) echo "login-path=$PATH" ;; esac'
touch created-in-container
"#;

    let podbay = || podbay(root);

    let built = podbay()
        .args(["build", "--backend", &engine, "-p", &id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let tag = String::from_utf8(built).unwrap().trim().to_owned();
    // On rootful docker the last layer gives dev the host user's ids.
    assert!(
        tag.starts_with(&format!("podbay-{id}:")) || tag.starts_with("podbay-uid"),
        "{tag}"
    );
    let _image = Cleanup {
        engine: &engine,
        kind: "image",
        name: tag,
    };

    let output = podbay()
        .args(["run", "--backend", &engine, "--local-cache", "-p", &id])
        .arg(&project)
        .args(["--", "sh", "-c", script])
        .assert()
        .success()
        .get_output()
        .clone();
    let out = String::from_utf8_lossy(&output.stdout);
    let err = String::from_utf8_lossy(&output.stderr);

    for expected in [
        "pwd=/home/dev/workspace/repomount",
        "home=/home/dev",
        "marker=from-extension",
        "gitconfig=readable",
        "gitconfig=readonly",
        "volume=writable",
        "var=hello",
        "path=prepended",
        "login-path=prepended",
    ] {
        assert!(
            out.contains(expected),
            "missing {expected:?}\nstdout:\n{out}\nstderr:\n{err}"
        );
    }

    // The point of the identity mapping: files the container creates in the
    // project belong to the host user.
    let created = fs::metadata(project.join("created-in-container")).unwrap();
    assert_eq!(created.uid(), rustix::process::getuid().as_raw());
}

#[test]
fn session_report_flags_git_changes() {
    let Some(engine) = engine() else { return };
    build_test_base(&engine);

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let project = root.join("project");
    fs::create_dir_all(project.join(".git/hooks")).unwrap();
    fs::write(project.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(project.join(".git/config"), "[core]\n\tbare = false\n").unwrap();
    fs::create_dir_all(root.join("config")).unwrap();
    fs::write(
        root.join("config/config.toml"),
        format!("base_image = {TEST_BASE:?}\n"),
    )
    .unwrap();

    // What a hostile project or agent might leave behind for the host.
    let script = r#"
set -e
printf '#!/bin/sh\necho pwned\n' > .git/hooks/post-checkout
chmod +x .git/hooks/post-checkout
printf '\tfsmonitor = "sh -c pwned"\n' >> .git/config
mkdir -p vendor/lib/.git
printf '[core]\n\tpager = pwned\n' > vendor/lib/.git/config
exit 7
"#;
    let output = podbay(root)
        .args(["run", "--backend", &engine, "--local-cache"])
        .arg(&project)
        .args(["--", "sh", "-c", script])
        .assert()
        .code(7)
        .get_output()
        .clone();
    let err = String::from_utf8_lossy(&output.stderr);

    for expected in [
        "session report: 3 warnings",
        ".git/hooks/post-checkout: added, an executable file",
        "+ core.fsmonitor = sh -c pwned   [can run commands]",
        "vendor/lib/.git: nested repository changed",
    ] {
        assert!(
            err.contains(expected),
            "missing {expected:?}\nstderr:\n{err}"
        );
    }
}

#[test]
fn session_is_hardened() {
    let Some(engine) = engine() else { return };
    build_test_base(&engine);

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let project = root.join("project");
    let profiles = root.join("config/profiles");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&profiles).unwrap();
    fs::write(
        root.join("config/config.toml"),
        format!("base_image = {TEST_BASE:?}\n"),
    )
    .unwrap();
    let id = format!("e2ecaps{}", std::process::id());
    fs::write(
        profiles.join(format!("{id}.toml")),
        "cap_add = [\"NET_RAW\"]\n",
    )
    .unwrap();

    let run = |profile: Option<&str>, script: &str| {
        let mut cmd = podbay(root);
        cmd.args(["run", "--backend", &engine, "--local-cache"]);
        if let Some(p) = profile {
            cmd.args(["-p", p]);
        }
        let output = cmd
            .arg(&project)
            .args(["--", "sh", "-c", script])
            .assert()
            .success()
            .get_output()
            .clone();
        (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };

    let (out, err) = run(
        None,
        r#"
grep -q '^NoNewPrivs:[[:space:]]*1' /proc/self/status && echo nnp=on
grep '^CapEff' /proc/self/status | tr -d '\t'
grep '^CapBnd' /proc/self/status | tr -d '\t'
if touch /usr/podbay-probe 2>/dev/null; then echo rootfs=writable; else echo rootfs=readonly; fi
test -f ~/.bashrc && echo home=seeded
touch ~/left-behind && echo home=writable
printf '#!/bin/sh\necho tmp=exec\n' > /tmp/probe && chmod +x /tmp/probe && /tmp/probe
echo "pids=$(cat /sys/fs/cgroup/pids.max 2>/dev/null || cat /sys/fs/cgroup/pids/pids.max 2>/dev/null || echo unknown)"
"#,
    );
    for expected in [
        "nnp=on",
        "CapEff:0000000000000000",
        "CapBnd:0000000000000000",
        "rootfs=readonly",
        "home=seeded",
        "home=writable",
        "tmp=exec",
    ] {
        assert!(
            out.contains(expected),
            "missing {expected:?}\nstdout:\n{out}\nstderr:\n{err}"
        );
    }
    // Either the engine applied the default limit, or podbay said it can't.
    assert!(
        out.contains("pids=4096") || err.contains("cannot apply the process limit"),
        "stdout:\n{out}\nstderr:\n{err}"
    );

    // A profile can add a capability back, and the home directory starts
    // fresh every session.
    let (out, err) = run(
        Some(&id),
        r#"
grep '^CapEff' /proc/self/status | tr -d '\t'
test -e ~/left-behind && echo home=persisted || echo home=fresh
"#,
    );
    assert!(
        out.contains("CapEff:0000000000002000") && out.contains("home=fresh"),
        "stdout:\n{out}\nstderr:\n{err}"
    );
    assert!(err.contains("keeps capabilities NET_RAW"), "{err}");
}

#[test]
fn seccomp_filter_blocks_namespaces_and_ptrace() {
    let Some(engine) = engine() else { return };
    build_test_base(&engine);

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let project = root.join("project");
    let profiles = root.join("config/profiles");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&profiles).unwrap();
    fs::write(
        root.join("config/config.toml"),
        format!("base_image = {TEST_BASE:?}\n"),
    )
    .unwrap();
    let id = format!("e2esec{}", std::process::id());
    for (name, body) in [
        ("allow", "[seccomp]\nallow = [\"ptrace\", \"clone3\"]\n"),
        ("cap", "cap_add = [\"SYS_PTRACE\"]\n"),
        ("engine", "[seccomp]\nengine_default = true\n"),
    ] {
        fs::write(profiles.join(format!("{id}-{name}.toml")), body).unwrap();
    }

    // PTRACE_TRACEME through perl's syscall(), by number: the test image
    // has no strace or gdb, but Debian always has perl and unshare.
    let script = r#"
case $(uname -m) in
  x86_64) nr=101 ;;
  aarch64) nr=117 ;;
  *) nr= ;;
esac
if [ -n "$nr" ]; then
  perl -e "print 'ptrace=', (syscall($nr, 0, 0, 0, 0) == 0 ? 'allowed' : 'denied'), qq(\n)"
fi
# clone3 with no arguments: ENOSYS (38) when the filter stops it, EINVAL
# (22) from the kernel. Upstream's own rule for it returns ENOSYS, so this
# catches an allow that loses to it.
perl -e "syscall(435, 0, 0); print 'clone3=', \$!+0, qq(\n)"
if unshare --user true 2>/dev/null; then echo userns=allowed; else echo userns=denied; fi
"#;
    let run = |profile: Option<String>| {
        let mut cmd = podbay(root);
        cmd.args(["run", "--backend", &engine]);
        if let Some(p) = profile {
            cmd.args(["-p", &p]);
        }
        let output = cmd
            .arg(&project)
            .args(["--", "sh", "-c", script])
            .assert()
            .success()
            .get_output()
            .clone();
        (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    let ptrace = |out: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix("ptrace="))
            .map(str::to_owned)
    };

    let (out, err) = run(None);
    assert!(
        out.contains("userns=denied"),
        "stdout:\n{out}\nstderr:\n{err}"
    );
    assert!(out.contains("clone3=38"), "stdout:\n{out}\nstderr:\n{err}");
    let Some(default) = ptrace(&out) else {
        eprintln!("skipping the ptrace checks: no syscall number for this machine");
        return;
    };
    assert_eq!(default, "denied", "stdout:\n{out}\nstderr:\n{err}");

    // A profile can allow it by name, or by keeping SYS_PTRACE.
    let (out, err) = run(Some(format!("{id}-allow")));
    assert_eq!(ptrace(&out).as_deref(), Some("allowed"), "{out}\n{err}");
    assert!(
        err.contains(&format!(
            "seccomp filter also allows ptrace (from {id}-allow), clone3 (from {id}-allow)"
        )),
        "{err}"
    );
    assert!(out.contains("clone3=22"), "{out}");
    assert!(out.contains("userns=denied"), "{out}");
    let (out, err) = run(Some(format!("{id}-cap")));
    assert_eq!(ptrace(&out).as_deref(), Some("allowed"), "{out}\n{err}");

    // Or fall back to the engine's default, which allows it everywhere.
    let (out, err) = run(Some(format!("{id}-engine")));
    assert_eq!(ptrace(&out).as_deref(), Some("allowed"), "{out}\n{err}");
    assert!(
        err.contains("default seccomp filter instead of podbay's"),
        "{err}"
    );
}

#[test]
fn provisioning_runs_profile_commands_and_sessions_are_offline() {
    let Some(engine) = engine() else { return };
    build_test_base(&engine);

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let project = root.join("project");
    let profiles = root.join("config/profiles");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&profiles).unwrap();
    fs::write(
        root.join("config/config.toml"),
        format!("base_image = {TEST_BASE:?}\n"),
    )
    .unwrap();
    let id = format!("e2eprov{}", std::process::id());
    let _volumes = VolumeCleanup {
        engine: &engine,
        prefix: format!("podbay-{id}"),
    };
    fs::write(
        profiles.join(format!("{id}.toml")),
        r#"
[env]
E2E_TOOLS = "from-env"

[[volume]]
name = "tools"
container = "/home/dev/.local/tools"
provision = true

[[volume]]
name = "secret"
container = "/home/dev/.secret"

[provision]
command = "probe-install /home/dev/.local/tools"
watch = [".tool-versions"]
"#,
    )
    .unwrap();
    fs::write(
        profiles.join(format!("{id}-online.toml")),
        format!("extends = [\"{id}\"]\nnetwork = \"full\"\n"),
    )
    .unwrap();
    fs::write(project.join(".tool-versions"), "node 22\n").unwrap();

    let run = |profile: Option<&str>, script: &str| {
        let mut cmd = podbay(root);
        cmd.args(["run", "--backend", &engine, "--local-cache"]);
        if let Some(p) = profile {
            cmd.args(["-p", p]);
        }
        let output = cmd
            .arg(&project)
            .args(["--", "sh", "-c", script])
            .assert()
            .success()
            .get_output()
            .clone();
        (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    let session = r#"
echo "session-network=$(ls /sys/class/net | tr '\n' ' ')"
cat ~/.local/tools/provisioned
echo "installs=$(wc -l < ~/.local/tools/installs)"
"#;

    // Without a [provision] command there is nothing to provision.
    let (_, err) = run(None, "true");
    assert!(!err.contains("provisioning tools"), "{err}");

    // Provisioning runs first, with network, a read-only project, the
    // profile's environment and only the provision volumes; then the
    // session runs with loopback only.
    let (out, err) = run(Some(&id), session);
    assert!(err.contains("provisioning tools for"), "{err}");
    for expected in [
        "session-network=lo \n",
        "project=readonly",
        "secret=absent",
        "env=from-env",
        "versions=node 22",
        "installs=1",
    ] {
        assert!(
            out.contains(expected),
            "missing {expected:?}\nstdout:\n{out}\nstderr:\n{err}"
        );
    }
    let provisioned_network = out
        .lines()
        .find_map(|l| l.strip_prefix("network="))
        .unwrap();
    assert_ne!(provisioned_network.trim(), "lo", "{out}");

    // Nothing changed, so nothing is installed again.
    let (out, err) = run(Some(&id), session);
    assert!(!err.contains("provisioning tools"), "{err}");
    assert!(out.contains("installs=1"), "{out}");

    // A session that edits a watched file gets a new install next run.
    run(Some(&id), "echo 'node 24' > .tool-versions");
    let (out, err) = run(Some(&id), session);
    assert!(err.contains("provisioning tools for"), "{err}");
    assert!(
        out.contains("versions=node 24") && out.contains("installs=2"),
        "{out}"
    );

    // A profile can opt into the network, and podbay says so. It extends
    // the first, so it shares its tools volume and its install.
    let (out, err) = run(Some(&format!("{id}-online")), session);
    assert!(
        err.contains(&format!(
            "full network access (network = \"full\" from profile {id}-online)"
        )),
        "{err}"
    );
    let session_network = out
        .lines()
        .find_map(|l| l.strip_prefix("session-network="))
        .unwrap();
    assert_ne!(session_network.trim(), "lo", "{out}");
}

#[test]
fn network_can_be_toggled_per_run() {
    let Some(engine) = engine() else { return };
    build_test_base(&engine);

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(root.join("config")).unwrap();

    let run = |flags: &[&str]| {
        let output = podbay(root)
            .args(["run", "--backend", &engine])
            .args(flags)
            .arg(&project)
            .args(["--", "sh", "-c", "ls /sys/class/net | tr '\\n' ' '"])
            .assert()
            .success()
            .get_output()
            .clone();
        (
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    let config = |extra: &str| {
        fs::write(
            root.join("config/config.toml"),
            format!("base_image = {TEST_BASE:?}\n{extra}"),
        )
        .unwrap();
    };

    // Off by default; --online turns it on for one run, without a warning
    // since you asked for it.
    config("");
    let (out, _) = run(&[]);
    assert_eq!(out, "lo");
    let (out, err) = run(&["--online"]);
    assert_ne!(out, "lo", "{err}");
    assert!(err.contains("network: full (from --online)"), "{err}");
    assert!(!err.contains("warning"), "{err}");

    // On by default from config.toml, which is stated, not warned about;
    // --offline turns it off for one run.
    config("network = \"full\"\n");
    let (out, err) = run(&[]);
    assert_ne!(out, "lo", "{err}");
    assert!(err.contains("network: full (from config.toml)"), "{err}");
    assert!(!err.contains("warning"), "{err}");
    let (out, err) = run(&["--offline"]);
    assert_eq!(out, "lo", "{err}");
}

#[test]
fn allowlisted_session_reaches_only_listed_hosts() {
    let Some(engine) = engine() else { return };
    build_test_base(&engine);

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let project = root.join("allowlist-project");
    let profiles = root.join("config/profiles");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&profiles).unwrap();
    fs::write(
        root.join("config/config.toml"),
        format!("base_image = {TEST_BASE:?}\n"),
    )
    .unwrap();
    let id = format!("e2eallow{}", std::process::id());
    // .test names never resolve (RFC 6761), and localhost is allowed by
    // name but must still be refused for being local.
    fs::write(
        profiles.join(format!("{id}.toml")),
        "network = [\"allowed.podbay.test\", \"localhost\"]\n",
    )
    .unwrap();

    // Talks to the proxy by hand: the test image has no curl.
    let script = r#"
# /proc/net/dev shows the namespace the process is really in; some
# runtimes show the host's /sys to a container that joined another's.
echo "interfaces=$(tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' ' | tr '\n' ' ')"
echo "proxy=$HTTPS_PROXY"
probe() {
  exec 3<>/dev/tcp/127.0.0.1/3128 || { echo "$1 => no proxy"; return; }
  printf 'CONNECT %s HTTP/1.1\r\nHost: %s\r\n\r\n' "$1" "$1" >&3
  read -r status <&3
  echo "$1 => ${status%$'\r'}"
  exec 3>&-
}
probe evil.example:443
probe localhost:443
probe 169.254.169.254:80
probe allowed.podbay.test:443
if timeout 3 bash -c 'exec 3<>/dev/tcp/1.1.1.1/443' 2>/dev/null; then
  echo direct=open
else
  echo direct=blocked
fi
"#;
    let output = podbay(root)
        .args(["run", "--backend", &engine, "--local-cache", "-p", &id])
        .arg(&project)
        .args(["--", "bash", "-c", script])
        .assert()
        .success()
        .get_output()
        .clone();
    let out = String::from_utf8_lossy(&output.stdout);
    let err = String::from_utf8_lossy(&output.stderr);

    for expected in [
        "interfaces=lo \n",
        "proxy=http://127.0.0.1:3128",
        "evil.example:443 => HTTP/1.1 403 Forbidden",
        "localhost:443 => HTTP/1.1 403 Forbidden",
        "169.254.169.254:80 => HTTP/1.1 403 Forbidden",
        "allowed.podbay.test:443 => HTTP/1.1 502 Bad Gateway",
        "direct=blocked",
    ] {
        assert!(
            out.contains(expected),
            "missing {expected:?}\nstdout:\n{out}\nstderr:\n{err}"
        );
    }
    for expected in [
        "network: HTTPS to allowed.podbay.test, localhost only",
        "evil.example:443: denied, not on the allowlist",
        "localhost:443: denied, it resolves only to private or local addresses",
        "169.254.169.254:80: denied, IP addresses are not allowed",
        "allowed.podbay.test:443: failed, the name did not resolve",
    ] {
        assert!(
            err.contains(expected),
            "missing {expected:?}\nstderr:\n{err}"
        );
    }

    // The relay is gone with the session. Other tests run relays at the
    // same time, so look for this project's only.
    let left = StdCommand::new(&engine)
        .args(["ps", "--all", "--quiet", "--filter"])
        .arg("name=podbay-relay-allowlist-project-")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&left.stdout).trim().is_empty(),
        "relay containers left behind"
    );
}

/// A server in the session, on its own loopback, reached from the host
/// through a forwarded port, in every network mode. The forward goes one
/// way: the session still can't reach the host or the internet.
#[test]
fn forwarded_port_reaches_a_server_in_the_session() {
    let Some(engine) = engine() else { return };
    build_test_base(&engine);

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let project = root.join("ports-project");
    let profiles = root.join("config/profiles");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&profiles).unwrap();
    fs::write(
        root.join("config/config.toml"),
        format!("base_image = {TEST_BASE:?}\n"),
    )
    .unwrap();
    let id = format!("e2eports{}", std::process::id());
    fs::write(
        profiles.join(format!("{id}.toml")),
        "network = [\"allowed.podbay.test\"]\n",
    )
    .unwrap();

    // Serves one connection on loopback only, then checks what the session
    // itself can reach. The test image has perl but no netcat or python.
    let script = r#"
echo "interfaces=$(tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' ' | sort | tr '\n' ' ')"
perl -MIO::Socket::INET -e '
  my $s = IO::Socket::INET->new(LocalAddr => "127.0.0.1", LocalPort => 3080,
                                Listen => 5, ReuseAddr => 1) or die "listen: $!";
  my $c = $s->accept; print $c "hello from the session
"; close $c;'
if timeout 3 bash -c "exec 3<>/dev/tcp/127.0.0.1/$HOST_PORT" 2>/dev/null; then
  echo host-port=open
else
  echo host-port=closed
fi
"#;

    for (flags, mode) in [
        (&["--offline"][..], "offline"),
        (&[], "allowlist"),
        (&["--online"], "full"),
    ] {
        let host_port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut child = StdCommand::new(env!("CARGO_BIN_EXE_podbay"))
            .env("PODBAY_CONFIG_DIR", root.join("config"))
            .env("PODBAY_CACHE_DIR", root.join("cache"))
            .args(["run", "--backend", &engine, "--local-cache", "-p", &id])
            .args(flags)
            .args(["--port", &format!("{host_port}:3080")])
            .arg(&project)
            .args([
                "--",
                "env",
                &format!("HOST_PORT={host_port}"),
                "bash",
                "-c",
                script,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        // The host side listens before the session's server is up; until
        // then connections close with nothing read.
        let deadline = Instant::now() + Duration::from_secs(120);
        let reply = loop {
            if let Ok(mut conn) = TcpStream::connect(("127.0.0.1", host_port)) {
                let _ = conn.set_read_timeout(Some(Duration::from_secs(10)));
                let mut reply = String::new();
                let _ = conn.read_to_string(&mut reply);
                if !reply.is_empty() {
                    break reply;
                }
            }
            if Instant::now() > deadline || child.try_wait().unwrap().is_some() {
                let output = child.wait_with_output().unwrap();
                panic!(
                    "{mode}: no reply through the forward\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            thread::sleep(Duration::from_millis(100));
        };
        assert_eq!(reply, "hello from the session\n", "{mode}");

        let output = child.wait_with_output().unwrap();
        let out = String::from_utf8_lossy(&output.stdout);
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{mode}\nstdout:\n{out}\nstderr:\n{err}"
        );
        for expected in [
            format!("forwarding http://127.0.0.1:{host_port} to session port 3080"),
            format!("127.0.0.1:{host_port} -> session port 3080: "),
        ] {
            assert!(
                err.contains(&expected),
                "{mode}: missing {expected:?}\n{err}"
            );
        }
        // The session never sees the host's loopback.
        assert!(out.contains("host-port=closed"), "{mode}\n{out}");
        match mode {
            "full" => assert!(!out.contains("interfaces=lo \n"), "{mode}\n{out}"),
            _ => assert!(out.contains("interfaces=lo \n"), "{mode}\n{out}"),
        }
        assert_eq!(
            err.contains("podbay: network:\n") || err.contains("no connections through"),
            mode == "allowlist",
            "{mode}\n{err}"
        );

        // The port closes with the session.
        assert!(
            TcpStream::connect(("127.0.0.1", host_port)).is_err(),
            "{mode}: host port {host_port} still open"
        );
    }

    let left = StdCommand::new(&engine)
        .args(["ps", "--all", "--quiet", "--filter"])
        .arg("name=podbay-relay-ports-project-")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&left.stdout).trim().is_empty(),
        "relay containers left behind"
    );
}

/// Needs the internet, so it runs only with PODBAY_TEST_INTERNET=1 (as CI
/// sets it).
#[test]
fn allowlisted_session_reaches_a_real_host() {
    if std::env::var_os("PODBAY_TEST_INTERNET").is_none() {
        eprintln!("skipping: set PODBAY_TEST_INTERNET=1 to run");
        return;
    }
    let Some(engine) = engine() else { return };
    build_test_base(&engine);

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let project = root.join("project");
    let profiles = root.join("config/profiles");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&profiles).unwrap();
    fs::write(
        root.join("config/config.toml"),
        format!("base_image = {TEST_BASE:?}\n"),
    )
    .unwrap();
    let id = format!("e2ereal{}", std::process::id());
    fs::write(
        profiles.join(format!("{id}.toml")),
        "network = [\"example.com\"]\n",
    )
    .unwrap();

    let script = r#"
exec 3<>/dev/tcp/127.0.0.1/3128
printf 'CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n' >&3
read -r status <&3
echo "status=${status%$'\r'}"
"#;
    let output = podbay(root)
        .args(["run", "--backend", &engine, "--local-cache", "-p", &id])
        .arg(&project)
        .args(["--", "bash", "-c", script])
        .assert()
        .success()
        .get_output()
        .clone();
    let out = String::from_utf8_lossy(&output.stdout);
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        out.contains("status=HTTP/1.1 200 Connection established"),
        "stdout:\n{out}\nstderr:\n{err}"
    );
    assert!(err.contains("example.com:443: allowed"), "{err}");
}

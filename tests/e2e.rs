//! End-to-end runs against a real engine. Skipped when the engine is not
//! available. Select it with PODBAY_TEST_BACKEND (default: docker).
//!
//! Uses a lightweight stand-in for the bundled base image (same entrypoint,
//! no toolchain) so the test needs no network beyond pulling Debian.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

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

[[mount]]
host = {gitconfig:?}
container = "/home/dev/.gitconfig"

[[volume]]
name = "state"
container = "/home/dev/.config/tool/state"

[tools]
node = "22"
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
grep -q 'node = "22"' ~/.config/mise/config.toml && echo tools=present
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
        "tools=present",
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

    // --local-cache keeps mise data in a per-project host directory.
    let cache = root.join("cache/mise");
    assert_eq!(fs::read_dir(&cache).unwrap().count(), 1);
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
test -f ~/.bashrc && test -d ~/.config/mise && echo home=seeded
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

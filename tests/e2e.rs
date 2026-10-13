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
    let config = root.join("config/podbay");
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
host = "~/.gitconfig"
container = "/home/dev/.gitconfig"

[[volume]]
name = "state"
container = "/home/dev/.config/tool/state"

[tools]
node = "22"
"#
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

    let podbay = || {
        let mut cmd = Command::cargo_bin("podbay").unwrap();
        cmd.env("HOME", &home)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"));
        cmd
    };

    let built = podbay()
        .args(["build", "--backend", &engine, "-p", &id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let tag = String::from_utf8(built).unwrap().trim().to_owned();
    assert!(tag.starts_with(&format!("podbay-{id}:")), "{tag}");
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
    let cache = root.join("cache/podbay/mise");
    assert_eq!(fs::read_dir(&cache).unwrap().count(), 1);
}

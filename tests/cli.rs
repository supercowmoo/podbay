//! CLI behavior that needs no container engine.

use std::fs;
use std::path::Path;

use assert_cmd::Command;

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("home")).unwrap();
        Self { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn profiles(&self) -> std::path::PathBuf {
        self.root().join("config/podbay/profiles")
    }

    fn podbay(&self) -> Command {
        let mut cmd = Command::cargo_bin("podbay").unwrap();
        cmd.env("HOME", self.root().join("home"))
            .env("XDG_CONFIG_HOME", self.root().join("config"))
            .env("XDG_CACHE_HOME", self.root().join("cache"));
        cmd
    }
}

fn stdout(cmd: &mut Command) -> String {
    String::from_utf8(cmd.assert().success().get_output().stdout.clone()).unwrap()
}

fn stderr_of_failure(cmd: &mut Command) -> String {
    String::from_utf8(cmd.assert().failure().get_output().stderr.clone()).unwrap()
}

#[test]
fn profile_init_list_show() {
    let env = Env::new();
    stdout(env.podbay().args(["profile", "init", "base"]));
    stdout(
        env.podbay()
            .args(["profile", "init", "ai", "--extends", "base"]),
    );

    assert_eq!(stdout(env.podbay().args(["profile", "list"])), "ai\nbase\n");

    let shown = stdout(env.podbay().args(["profile", "show", "ai"]));
    assert!(shown.starts_with("chain: base > ai\n"), "{shown}");
    assert!(shown.contains("podbay-base:"), "{shown}");
}

#[test]
fn profile_init_refuses_to_overwrite() {
    let env = Env::new();
    stdout(env.podbay().args(["profile", "init", "x"]));
    let err = stderr_of_failure(env.podbay().args(["profile", "init", "x"]));
    assert!(err.contains("refusing to overwrite"), "{err}");
}

#[test]
fn invalid_profile_name() {
    let env = Env::new();
    let err = stderr_of_failure(env.podbay().args(["profile", "init", "../escape"]));
    assert!(err.contains("invalid profile name"), "{err}");
}

#[test]
fn show_reports_parse_errors_with_location() {
    let env = Env::new();
    fs::create_dir_all(env.profiles()).unwrap();
    fs::write(
        env.profiles().join("bad.toml"),
        "[[mount]]\nhost = \"/\"\ncontianer = \"/x\"\n",
    )
    .unwrap();
    let err = stderr_of_failure(env.podbay().args(["profile", "show", "bad"]));
    assert!(
        err.contains("bad.toml") && err.contains("contianer"),
        "{err}"
    );
}

#[test]
fn show_warns_about_missing_mount_sources() {
    let env = Env::new();
    fs::create_dir_all(env.profiles()).unwrap();
    fs::write(
        env.profiles().join("p.toml"),
        "[[mount]]\nhost = \"~/missing\"\ncontainer = \"/x\"\n",
    )
    .unwrap();
    let shown = stdout(env.podbay().args(["profile", "show", "p"]));
    assert!(
        shown.contains("warning:") && shown.contains("missing"),
        "{shown}"
    );
}

#[test]
fn run_rejects_missing_project() {
    let env = Env::new();
    let err = stderr_of_failure(env.podbay().args([
        "run",
        "--backend",
        "docker",
        "--dry-run",
        "/definitely/not/here",
    ]));
    assert!(err.contains("not found"), "{err}");
}

#[test]
fn run_rejects_unknown_profile() {
    let env = Env::new();
    let err = stderr_of_failure(env.podbay().args([
        "run",
        "--backend",
        "docker",
        "--dry-run",
        "-p",
        "ghost",
        ".",
    ]));
    assert!(err.contains("profile \"ghost\" not found"), "{err}");
}

#[test]
fn stacked_profiles_show_together() {
    let env = Env::new();
    fs::create_dir_all(env.profiles()).unwrap();
    fs::write(
        env.profiles().join("rust.toml"),
        "[[volume]]\nname = \"cargo\"\ncontainer = \"/home/dev/.cargo\"\nprovision = true\n\
         [provision]\ncommand = \"rustup show\"\n",
    )
    .unwrap();
    fs::write(
        env.profiles().join("agent.toml"),
        "shell = \"zsh\"\nports = [3080]\n[[volume]]\nname = \"state\"\ncontainer = \"/home/dev/.agent\"\n",
    )
    .unwrap();
    fs::write(env.profiles().join("fishy.toml"), "shell = \"fish\"\n").unwrap();

    for args in [&["rust", "agent"][..], &["rust,agent"]] {
        let shown = stdout(env.podbay().args(["profile", "show"]).args(args));
        assert!(shown.starts_with("chain: rust > agent\n"), "{shown}");
        for expected in [
            "podbay-rust-cargo -> /home/dev/.cargo  (provision, from rust)",
            "podbay-agent-state -> /home/dev/.agent  (from agent)",
            "rustup show  (from rust)",
            "127.0.0.1:3080 -> session port 3080  (from agent)",
        ] {
            assert!(shown.contains(expected), "missing {expected:?}\n{shown}");
        }
    }

    let shown = stdout(env.podbay().args(["profile", "show", "agent", "fishy"]));
    assert!(
        shown.contains("note: shell from agent is replaced by fishy's"),
        "{shown}"
    );
}

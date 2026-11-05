//! CLI behavior that needs no container engine.

use std::fs;
use std::path::Path;

use std::process::Command;

mod common;
use common::Run;

struct Env {
    dir: common::TempDir,
}

impl Env {
    fn new() -> Self {
        let dir = common::tempdir().unwrap();
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
        let mut cmd = common::podbay_command();
        cmd.env("HOME", self.root().join("home"))
            .env("XDG_CONFIG_HOME", self.root().join("config"))
            .env("XDG_CACHE_HOME", self.root().join("cache"));
        cmd
    }
}

fn stdout(cmd: &mut Command) -> String {
    String::from_utf8(cmd.run_ok().stdout).unwrap()
}

fn stderr_of_failure(cmd: &mut Command) -> String {
    String::from_utf8(cmd.run_fails().stderr).unwrap()
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

/// The example profiles in examples/ stay valid as podbay changes, and
/// stack the way the README says.
#[test]
fn example_profiles_resolve() {
    let env = Env::new();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    fs::create_dir_all(env.profiles()).unwrap();
    for entry in fs::read_dir(repo.join("examples/profiles")).unwrap() {
        let path = entry.unwrap().path();
        fs::copy(&path, env.profiles().join(path.file_name().unwrap())).unwrap();
    }
    // Where dsh.toml expects its settings file, under the test's HOME.
    let dsh = env.root().join("home/.config/podbay/dsh");
    fs::create_dir_all(&dsh).unwrap();
    fs::copy(
        repo.join("examples/dsh/cordis.patch.yml"),
        dsh.join("cordis.patch.yml"),
    )
    .unwrap();

    let output = env
        .podbay()
        .args(["profile", "show", "rust", "dsh"])
        .run_ok();
    let shown = String::from_utf8_lossy(&output.stdout);
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        !shown.contains("warning") && err.is_empty(),
        "{shown}\n{err}"
    );
    for expected in [
        "chain: rust > dsh",
        "podbay-rust-cargo -> /home/dev/.cargo  (provision, from rust)",
        "(extension from rust)",
        "(extension from dsh)",
        "podbay-dsh-home -> /home/dev/.dsh  (from dsh)",
        "~/.config/podbay/dsh/cordis.patch.yml -> /home/dev/.dsh/cordis.patch.yml  (ro, from dsh)",
        "OPENROUTER_API_KEY  (from dsh)",
        "network: allow index.crates.io, static.crates.io, openrouter.ai",
        "127.0.0.1:3080 -> session port 3080  (from dsh)",
    ] {
        assert!(shown.contains(expected), "missing {expected:?}\n{shown}");
    }
}

#[test]
fn doctor_reports_broken_profiles() {
    let env = Env::new();
    fs::create_dir_all(env.profiles()).unwrap();
    fs::write(env.profiles().join("good.toml"), "shell = \"zsh\"\n").unwrap();
    fs::write(env.profiles().join("bad.toml"), "[[mount]]\n").unwrap();
    let output = env
        .podbay()
        .args(["doctor", "--backend", "docker"])
        .run_fails();
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(out.contains("FAIL  profile: bad: invalid profile"), "{out}");
    assert!(out.contains("      missing field `host`"), "{out}");
    assert!(out.contains("ok    seccomp:"), "{out}");
    assert!(
        out.ends_with("Sessions won't work until the failures above are fixed.\n"),
        "{out}"
    );

    fs::remove_file(env.profiles().join("bad.toml")).unwrap();
    let output = env
        .podbay()
        .args(["doctor", "--backend", "docker"])
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(out.contains("profiles: 1 load: good"), "{out}");
}

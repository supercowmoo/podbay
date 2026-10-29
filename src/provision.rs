//! Provisioning: installing a project's tools before the session starts.
//!
//! Profiles bring their own toolchain managers: a `[provision]` command
//! (`rustup toolchain install`, `uv sync`, `nvm install`, ...) that installs
//! into volumes marked `provision = true`. It runs in a separate short-lived
//! container with network access, so tools are in place even when the
//! session itself is offline. That container sees only what installing
//! needs: the project, read-only, the provision volumes and the profile's
//! environment. Bind mounts and other volumes, which may hold credentials,
//! are left out.
//!
//! `podbay run` provisions automatically when anything that decides the
//! installed tools changed since the last time: the image, the commands,
//! the provision volumes, the environment, or the project files the
//! profile watches. That is tracked by a fingerprint stored per project
//! under the cache directory.

use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::hash::sha256_hex_fields;
use crate::paths::Paths;
use crate::profile::ResolvedProfile;

/// Files larger than this are fingerprinted by size alone.
const MAX_READ: u64 = 1 << 20;

/// Entries of a watched directory beyond this are ignored.
const MAX_DIR_ENTRIES: usize = 1_000;

/// The shell script provisioning runs: every step in order, stopping at the
/// first that fails.
pub fn script(profile: &ResolvedProfile) -> String {
    let mut out = "set -e\n".to_owned();
    for step in &profile.provision {
        out.push_str(&step.command);
        out.push('\n');
    }
    out
}

/// The fingerprint of everything provisioning depends on, or `None` when
/// the profile has nothing to provision. `local` is `--local-cache`, which
/// installs into the project's own copies of the provision volumes.
pub fn fingerprint(
    project: &Path,
    image: &str,
    profile: &ResolvedProfile,
    local: bool,
) -> Option<String> {
    if profile.provision.is_empty() {
        return None;
    }
    let mut fields: Vec<Vec<u8>> = vec![
        image.into(),
        if local { "local" } else { "shared" }.into(),
        script(profile).into(),
        profile.path.join(":").into(),
    ];
    for (k, v) in &profile.env {
        fields.push(format!("env:{k}").into());
        fields.push(v.as_bytes().to_vec());
    }
    for v in profile.volumes.iter().filter(|v| v.provision) {
        fields.push(format!("volume:{}:{}", v.name, v.container).into());
    }
    for (rel, state) in watched_files(project, &profile.watch) {
        fields.push(rel.into());
        fields.push(state);
    }
    let fields: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
    Some(sha256_hex_fields(&fields))
}

/// The watched files and their state, by relative path. A watched directory
/// stands for the files directly in it.
fn watched_files(project: &Path, watch: &[String]) -> Vec<(String, Vec<u8>)> {
    let mut rels: Vec<String> = Vec::new();
    for rel in watch {
        let path = Path::new(rel);
        let is_dir = no_symlinks_below(project, path);
        if !is_dir {
            rels.push(rel.clone());
            continue;
        }
        let Ok(entries) = fs::read_dir(project.join(path)) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .take(MAX_DIR_ENTRIES)
            .collect();
        names.sort();
        let dir = rel.trim_end_matches('/');
        rels.extend(names.into_iter().map(|n| format!("{dir}/{n}")));
    }
    rels.into_iter()
        .filter_map(|rel| state(project, Path::new(&rel)).map(|s| (rel, s)))
        .collect()
}

/// What a project file looks like, for the fingerprint. The container may
/// have written these files, so symlinks are never followed: a link is
/// recorded by its target, and nothing behind a linked directory is read.
fn state(project: &Path, rel: &Path) -> Option<Vec<u8>> {
    if !no_symlinks_below(project, rel.parent()?) {
        return None;
    }
    let path = project.join(rel);
    let meta = fs::symlink_metadata(&path).ok()?;
    if meta.file_type().is_symlink() {
        let target = fs::read_link(&path).ok()?;
        let mut out = b"link:".to_vec();
        out.extend_from_slice(target.as_os_str().as_encoded_bytes());
        return Some(out);
    }
    if !meta.is_file() {
        return None;
    }
    if meta.len() > MAX_READ {
        return Some(format!("size:{}", meta.len()).into_bytes());
    }
    match read_nofollow(&path) {
        Ok(data) => {
            let mut out = b"file:".to_vec();
            out.extend(data);
            Some(out)
        }
        Err(_) => Some(b"unreadable".to_vec()),
    }
}

/// True if every directory from `project` down to `project/dir` is a real
/// directory, not a symlink.
fn no_symlinks_below(project: &Path, dir: &Path) -> bool {
    let mut path = project.to_path_buf();
    for part in dir.components() {
        path.push(part);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => {}
            _ => return false,
        }
    }
    true
}

fn read_nofollow(path: &Path) -> io::Result<Vec<u8>> {
    // O_NONBLOCK in case the file was swapped for a FIFO since the check.
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    let mut data = Vec::new();
    file.take(MAX_READ).read_to_end(&mut data)?;
    Ok(data)
}

/// Where the fingerprint of the last successful provisioning is kept. The
/// shared volumes and the project's own copies are provisioned separately.
fn stamp_file(paths: &Paths, repo_key: &str, local: bool) -> PathBuf {
    let kind = if local { "local" } else { "shared" };
    paths
        .cache_dir
        .join("provisioned")
        .join(format!("{repo_key}-{kind}"))
}

/// True unless the last successful provisioning had this fingerprint.
pub fn is_due(paths: &Paths, repo_key: &str, local: bool, fingerprint: &str) -> bool {
    match fs::read_to_string(stamp_file(paths, repo_key, local)) {
        Ok(previous) => previous.trim() != fingerprint,
        Err(_) => true,
    }
}

pub fn record(paths: &Paths, repo_key: &str, local: bool, fingerprint: &str) -> Result<()> {
    let file = stamp_file(paths, repo_key, local);
    let dir = file.parent().expect("stamp files live in a directory");
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::write(&file, format!("{fingerprint}\n"))
        .with_context(|| format!("writing {}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::test_paths;
    use crate::profile::{ProvisionStep, Volume};
    use std::os::unix::fs::symlink;

    const IMAGE: &str = "img:1";

    fn profile(watch: &[&str]) -> ResolvedProfile {
        ResolvedProfile {
            provision: vec![ProvisionStep {
                command: "install".into(),
                profile: "p".into(),
            }],
            watch: watch.iter().map(|&w| w.to_owned()).collect(),
            ..Default::default()
        }
    }

    fn print(project: &Path, profile: &ResolvedProfile) -> Option<String> {
        fingerprint(project, IMAGE, profile, false)
    }

    #[test]
    fn script_runs_every_step() {
        let mut p = profile(&[]);
        p.provision.push(ProvisionStep {
            command: "second && third".into(),
            profile: "q".into(),
        });
        assert_eq!(script(&p), "set -e\ninstall\nsecond && third\n");
    }

    #[test]
    fn nothing_to_provision() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("rust-toolchain.toml"), "x").unwrap();
        assert_eq!(print(dir.path(), &ResolvedProfile::default()), None);
        assert!(print(dir.path(), &profile(&[])).is_some());
    }

    #[test]
    fn follows_watched_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let prof = profile(&[".tool-versions", "conf.d"]);
        fs::write(p.join(".tool-versions"), "node 22\n").unwrap();
        let first = print(p, &prof).unwrap();
        assert_eq!(print(p, &prof).unwrap(), first);

        fs::write(p.join(".tool-versions"), "node 24\n").unwrap();
        let second = print(p, &prof).unwrap();
        assert_ne!(second, first);

        fs::create_dir_all(p.join("conf.d")).unwrap();
        fs::write(p.join("conf.d/extra.toml"), "").unwrap();
        let third = print(p, &prof).unwrap();
        assert_ne!(third, second);

        // Unwatched files don't matter.
        fs::write(p.join("README"), "hi").unwrap();
        assert_eq!(print(p, &prof).unwrap(), third);

        // Commands, environment, provision volumes, the cache choice and
        // the image all count.
        let mut other = prof.clone();
        other.provision[0].command = "install --all".into();
        assert_ne!(print(p, &other).unwrap(), third);
        let mut other = prof.clone();
        other.env.insert("RUSTUP_HOME".into(), "/x".into());
        assert_ne!(print(p, &other).unwrap(), third);
        let mut other = prof.clone();
        other.volumes.push(Volume {
            name: "podbay-p-rustup".into(),
            container: "/home/dev/.rustup".into(),
            provision: true,
            profile: "p".into(),
        });
        assert_ne!(print(p, &other).unwrap(), third);
        assert_ne!(fingerprint(p, IMAGE, &prof, true).unwrap(), third);
        assert_ne!(fingerprint(p, "img:2", &prof, false).unwrap(), third);
    }

    #[test]
    fn never_follows_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("project");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&p).unwrap();
        fs::create_dir_all(outside.join("conf")).unwrap();
        fs::write(outside.join("secret"), "one").unwrap();
        fs::write(outside.join("conf/config.toml"), "one").unwrap();
        symlink(outside.join("secret"), p.join(".tool-versions")).unwrap();
        symlink(outside.join("conf"), p.join("conf")).unwrap();
        let prof = profile(&[".tool-versions", "conf", "conf/config.toml"]);

        let before = print(&p, &prof).unwrap();
        fs::write(outside.join("secret"), "two").unwrap();
        fs::write(outside.join("conf/config.toml"), "two").unwrap();
        assert_eq!(print(&p, &prof).unwrap(), before);
    }

    #[test]
    fn stamps() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        assert!(is_due(&paths, "app-1", false, "abc"));
        record(&paths, "app-1", false, "abc").unwrap();
        assert!(!is_due(&paths, "app-1", false, "abc"));
        assert!(is_due(&paths, "app-1", false, "def"));
        assert!(is_due(&paths, "app-1", true, "abc"));
        assert!(is_due(&paths, "app-2", false, "abc"));
    }
}

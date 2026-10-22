//! Provisioning: installing a project's tools before the session starts.
//!
//! Sessions have no network by default, so mise can't download anything
//! once they're running. Instead, a separate short-lived container with
//! network access runs `mise install` first. It sees only what installing
//! needs: the project, read-only, the mise data directory and the profile's
//! tool list. Profile mounts and volumes, which may hold credentials, are
//! left out.
//!
//! `podbay run` provisions automatically when anything that decides the
//! installed tools changed since the last time: the image, the profile's
//! tools, where mise data lives, or the project's mise files. That is
//! tracked by a fingerprint stored per project under the cache directory.

use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::hash::sha256_hex_fields;
use crate::paths::Paths;
use crate::plan::MiseData;

/// Project files mise reads its tool versions from: its own config files and
/// the idiomatic version files of common languages. Tools configured
/// anywhere else need an explicit `podbay provision`.
const PROJECT_FILES: &[&str] = &[
    "mise.toml",
    "mise.local.toml",
    ".mise.toml",
    ".mise.local.toml",
    "mise/config.toml",
    ".mise/config.toml",
    ".config/mise.toml",
    ".config/mise/config.toml",
    ".tool-versions",
    ".nvmrc",
    ".node-version",
    ".bun-version",
    ".python-version",
    ".python-versions",
    ".ruby-version",
    ".go-version",
    ".java-version",
    ".sdkmanrc",
    ".terraform-version",
    "rust-toolchain",
    "rust-toolchain.toml",
];

/// Directories whose `*.toml` files mise also loads.
const PROJECT_DIRS: &[&str] = &[".config/mise/conf.d"];

/// Files larger than this are fingerprinted by size alone.
const MAX_READ: u64 = 1 << 20;

/// The command provisioning runs. Images without mise (a custom base image)
/// have nothing to provision.
pub const COMMAND: &str = "if command -v mise >/dev/null 2>&1; then exec mise install; fi; \
     echo 'podbay: the image has no mise; nothing to provision' >&2";

/// What decides which tools a session gets.
pub struct Inputs<'a> {
    pub image: &'a str,
    pub mise_data: &'a MiseData,
    /// The generated mise config with the profile's tools, if any.
    pub tools: Option<&'a str>,
}

/// The fingerprint of everything provisioning depends on, or `None` when
/// there's nothing to install: no profile tools and no project mise files.
pub fn fingerprint(project: &Path, inputs: &Inputs<'_>) -> Option<String> {
    let files = project_files(project);
    if files.is_empty() && inputs.tools.is_none() {
        return None;
    }
    let data = match inputs.mise_data {
        MiseData::Shared => "shared".to_owned(),
        MiseData::Local(dir) => format!("local:{}", dir.display()),
    };
    let mut fields: Vec<&[u8]> = vec![
        inputs.image.as_bytes(),
        data.as_bytes(),
        inputs.tools.unwrap_or_default().as_bytes(),
    ];
    for (rel, state) in &files {
        fields.push(rel.as_bytes());
        fields.push(state);
    }
    Some(sha256_hex_fields(&fields))
}

/// The project's mise files and their state, by relative path.
fn project_files(project: &Path) -> Vec<(String, Vec<u8>)> {
    let mut rels: Vec<String> = PROJECT_FILES.iter().map(|&r| r.to_owned()).collect();
    for dir in PROJECT_DIRS {
        if !no_symlinks_below(project, Path::new(dir)) {
            continue;
        }
        let Ok(entries) = fs::read_dir(project.join(dir)) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".toml"))
            .take(1_000)
            .collect();
        names.sort();
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
/// shared and local mise data are provisioned separately.
fn stamp_file(paths: &Paths, repo_key: &str, mise_data: &MiseData) -> PathBuf {
    let kind = match mise_data {
        MiseData::Shared => "shared",
        MiseData::Local(_) => "local",
    };
    paths
        .cache_dir
        .join("provisioned")
        .join(format!("{repo_key}-{kind}"))
}

/// True unless the last successful provisioning had this fingerprint.
pub fn is_due(paths: &Paths, repo_key: &str, mise_data: &MiseData, fingerprint: &str) -> bool {
    match fs::read_to_string(stamp_file(paths, repo_key, mise_data)) {
        Ok(previous) => previous.trim() != fingerprint,
        Err(_) => true,
    }
}

pub fn record(
    paths: &Paths,
    repo_key: &str,
    mise_data: &MiseData,
    fingerprint: &str,
) -> Result<()> {
    let file = stamp_file(paths, repo_key, mise_data);
    let dir = file.parent().expect("stamp files live in a directory");
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::write(&file, format!("{fingerprint}\n"))
        .with_context(|| format!("writing {}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::test_paths;
    use std::os::unix::fs::symlink;

    const IMAGE: &str = "img:1";

    fn print(project: &Path, tools: Option<&str>) -> Option<String> {
        fingerprint(
            project,
            &Inputs {
                image: IMAGE,
                mise_data: &MiseData::Shared,
                tools,
            },
        )
    }

    #[test]
    fn nothing_to_install() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README"), "hi").unwrap();
        assert_eq!(print(dir.path(), None), None);
        assert!(print(dir.path(), Some("[tools]\nnode = \"22\"\n")).is_some());
    }

    #[test]
    fn follows_project_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::write(p.join(".tool-versions"), "node 22\n").unwrap();
        let first = print(p, None).unwrap();
        assert_eq!(print(p, None).unwrap(), first);

        fs::write(p.join(".tool-versions"), "node 24\n").unwrap();
        let second = print(p, None).unwrap();
        assert_ne!(second, first);

        fs::create_dir_all(p.join(".config/mise/conf.d")).unwrap();
        fs::write(p.join(".config/mise/conf.d/extra.toml"), "").unwrap();
        assert_ne!(print(p, None).unwrap(), second);

        // A different image or tool list needs its own install.
        let tools = print(p, Some("[tools]\nx = \"1\"\n")).unwrap();
        let local = fingerprint(
            p,
            &Inputs {
                image: IMAGE,
                mise_data: &MiseData::Local("/c/mise/app".into()),
                tools: Some("[tools]\nx = \"1\"\n"),
            },
        )
        .unwrap();
        assert_ne!(tools, local);
    }

    #[test]
    fn never_follows_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("project");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&p).unwrap();
        fs::create_dir_all(outside.join("mise")).unwrap();
        fs::write(outside.join("secret"), "one").unwrap();
        fs::write(outside.join("mise/config.toml"), "one").unwrap();
        symlink(outside.join("secret"), p.join(".tool-versions")).unwrap();
        symlink(outside.join("mise"), p.join("mise")).unwrap();

        let before = print(&p, None).unwrap();
        fs::write(outside.join("secret"), "two").unwrap();
        fs::write(outside.join("mise/config.toml"), "two").unwrap();
        assert_eq!(print(&p, None).unwrap(), before);
    }

    #[test]
    fn stamps() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let data = MiseData::Shared;
        assert!(is_due(&paths, "app-1", &data, "abc"));
        record(&paths, "app-1", &data, "abc").unwrap();
        assert!(!is_due(&paths, "app-1", &data, "abc"));
        assert!(is_due(&paths, "app-1", &data, "def"));
        assert!(is_due(
            &paths,
            "app-1",
            &MiseData::Local("/x".into()),
            "abc"
        ));
        assert!(is_due(&paths, "app-2", &data, "abc"));
    }
}

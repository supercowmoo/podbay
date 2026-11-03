//! Which parts of a project's git directory a session may not change.
//!
//! git on the host runs hooks, and programs its config names
//! (`core.fsmonitor`, `core.hooksPath`, filters, aliases, ...), the next
//! time you use it. So `.git/hooks` and `.git/config`, and the same in
//! submodules' git directories, are mounted read-only over the project: a
//! session can commit, branch and diff, but can't plant anything for the
//! host's git to run.
//!
//! Renaming a directory moves the mounts inside it along, so `mv .git x`
//! would carry the read-only mounts away and let the session build a new
//! `.git`. Each git directory, and every directory between `.git` and a
//! submodule's, is therefore also mounted onto itself, writable: a mount
//! point can't be renamed or removed.
//!
//! What stays writable is covered by the session report instead, such as a
//! new `.git/commondir` pointing git at another directory.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::plan::{InnerMount, WORKSPACE};

/// Submodule git directories looked at, at most, and how deep.
const MAX_MODULES: usize = 200;
const MAX_DEPTH: usize = 8;

/// What to mount over the project, and problems worth saying.
#[derive(Debug, Default)]
pub struct Guard {
    pub mounts: Vec<InnerMount>,
    pub warnings: Vec<String>,
}

impl Guard {
    /// Whether anything is made read-only.
    pub fn protects(&self) -> bool {
        self.mounts.iter().any(|m| m.readonly)
    }
}

/// What to protect in `project` (canonical). A project without a `.git`
/// directory has nothing to protect. With `create`, a missing `.git/hooks`
/// is created, empty, as `git init` does, so the session can't create it
/// either; a dry run leaves it out instead.
pub fn guard(project: &Path, create: bool) -> Guard {
    let mut out = Guard::default();
    let git = project.join(".git");
    match fs::symlink_metadata(&git) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            // A `.git` file (a linked worktree or submodule checkout) keeps
            // its hooks and config elsewhere, outside the session's reach.
            return out;
        }
        Err(_) => return out,
    }
    pin(&mut out, git.clone(), Path::new(".git"));
    protect_git_dir(&git, Path::new(".git"), create, &mut out);

    // Submodules' git directories: .git/modules/<name>[/modules/<name>...].
    let mut queue = vec![(git.join("modules"), PathBuf::from(".git/modules"), 0)];
    let mut seen = 0;
    while let Some((dir, rel, depth)) = queue.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if !meta.is_dir() {
                continue;
            }
            seen += 1;
            if seen > MAX_MODULES || depth > MAX_DEPTH {
                out.warnings.push(format!(
                    "more submodule git directories than podbay protects ({MAX_MODULES}); \
                     the rest are writable"
                ));
                return out;
            }
            let path = entry.path();
            let rel = rel.join(entry.file_name());
            // This directory, and the `modules` one it's in, stay put.
            pin(&mut out, dir.clone(), rel.parent().expect("has a parent"));
            pin(&mut out, path.clone(), &rel);
            if path.join("config").is_file() {
                protect_git_dir(&path, &rel, false, &mut out);
            }
            queue.push((path.join("modules"), rel.join("modules"), depth + 1));
        }
    }
    out
}

/// Protects `hooks` and `config` in one git directory, `rel` from the
/// project root.
fn protect_git_dir(dir: &Path, rel: &Path, create_hooks: bool, out: &mut Guard) {
    let hooks = dir.join("hooks");
    match fs::symlink_metadata(&hooks) {
        Ok(meta) if meta.is_dir() => add(out, hooks, &rel.join("hooks")),
        Ok(_) => out.warnings.push(format!(
            "{} isn't a directory, so podbay can't make it read-only",
            hooks.display()
        )),
        Err(e) if e.kind() == ErrorKind::NotFound && create_hooks => match fs::create_dir(&hooks) {
            Ok(()) => add(out, hooks, &rel.join("hooks")),
            Err(e) => out
                .warnings
                .push(format!("creating {}: {e}", hooks.display())),
        },
        Err(_) => {}
    }
    let config = dir.join("config");
    match fs::symlink_metadata(&config) {
        Ok(meta) if meta.is_file() => add(out, config, &rel.join("config")),
        Ok(_) => out.warnings.push(format!(
            "{} isn't a regular file, so podbay can't make it read-only",
            config.display()
        )),
        Err(_) => {}
    }
}

/// Read-only, over the project.
fn add(out: &mut Guard, host: PathBuf, rel: &Path) {
    mount(out, host, rel, true);
}

/// Mounted onto itself, writable, so it can't be renamed or removed.
fn pin(out: &mut Guard, host: PathBuf, rel: &Path) {
    mount(out, host, rel, false);
}

fn mount(out: &mut Guard, host: PathBuf, rel: &Path, readonly: bool) {
    let target = format!("{WORKSPACE}/{}", rel.display());
    if out.mounts.iter().any(|m| m.target == target) {
        return;
    }
    // Paths go into the engine's comma-separated --mount and a
    // colon-separated list; a name with those can't be mounted.
    if target.contains([',', ':', '\n']) || host.to_string_lossy().contains([',', '\n']) {
        out.warnings.push(format!(
            "{} has a name podbay can't mount, so it stays writable",
            host.display()
        ));
        return;
    }
    out.mounts.push(InnerMount {
        source: host,
        target,
        readonly,
    });
}

/// The `writable` paths of a project whose rest is read-only: each
/// mounted writable over the read-only project. With `create`, missing
/// ones are created as directories first; a dry run leaves them out.
///
/// A path that is, or passes through, a symbolic link is refused: the
/// engine would follow it, and could mount something outside the project.
pub fn writable(
    project: &Path,
    paths: &[(String, String)],
    create: bool,
) -> anyhow::Result<Vec<InnerMount>> {
    let mut out = Vec::new();
    for (rel, profile) in paths {
        let mut host = project.to_owned();
        let mut missing = false;
        for part in Path::new(rel).components() {
            host.push(part);
            match fs::symlink_metadata(&host) {
                Ok(meta) if meta.file_type().is_symlink() => anyhow::bail!(
                    "writable path {rel:?} (profile {profile}) goes through the symbolic link \
                     {}; podbay won't mount through it",
                    host.display()
                ),
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    missing = true;
                    break;
                }
                Err(e) => anyhow::bail!("{}: {e}", host.display()),
            }
        }
        let host = project.join(rel);
        if missing {
            if !create {
                continue;
            }
            fs::create_dir_all(&host)
                .map_err(|e| anyhow::anyhow!("creating {}: {e}", host.display()))?;
        }
        out.push(InnerMount {
            source: host,
            target: format!("{WORKSPACE}/{rel}"),
            readonly: false,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read-only targets, from the project root.
    fn targets(g: &Guard) -> Vec<String> {
        g.mounts
            .iter()
            .filter(|m| m.readonly)
            .map(|m| m.target.strip_prefix(WORKSPACE).unwrap().to_owned())
            .collect()
    }

    /// Directories mounted onto themselves.
    fn pinned(g: &Guard) -> Vec<String> {
        let mut out: Vec<String> = g
            .mounts
            .iter()
            .filter(|m| !m.readonly)
            .map(|m| m.target.strip_prefix(WORKSPACE).unwrap().to_owned())
            .collect();
        out.sort();
        out
    }

    #[test]
    fn hooks_and_config_of_the_repository_and_its_submodules() {
        let dir = crate::tempdir::tempdir().unwrap();
        let p = dir.path();
        fs::create_dir_all(p.join(".git/modules/lib/modules/inner")).unwrap();
        fs::write(p.join(".git/config"), "").unwrap();
        fs::write(p.join(".git/modules/lib/config"), "").unwrap();
        fs::create_dir(p.join(".git/modules/lib/hooks")).unwrap();
        fs::write(p.join(".git/modules/lib/modules/inner/config"), "").unwrap();

        // A dry run creates nothing.
        assert!(!targets(&guard(p, false)).contains(&"/.git/hooks".to_owned()));
        assert!(!p.join(".git/hooks").exists());

        let g = guard(p, true);
        assert!(g.warnings.is_empty(), "{:?}", g.warnings);
        let mut got = targets(&g);
        got.sort();
        assert_eq!(
            got,
            [
                "/.git/config",
                "/.git/hooks",
                "/.git/modules/lib/config",
                "/.git/modules/lib/hooks",
                "/.git/modules/lib/modules/inner/config",
            ]
        );
        assert_eq!(
            pinned(&g),
            [
                "/.git",
                "/.git/modules",
                "/.git/modules/lib",
                "/.git/modules/lib/modules",
                "/.git/modules/lib/modules/inner",
            ]
        );
        // The repository's missing hooks directory was created, empty; a
        // submodule's wasn't.
        assert!(p.join(".git/hooks").is_dir());
        assert!(!p.join(".git/modules/lib/modules/inner/hooks").exists());
    }

    #[test]
    fn nothing_without_a_git_directory() {
        let dir = crate::tempdir::tempdir().unwrap();
        assert!(guard(dir.path(), true).mounts.is_empty());
        fs::write(dir.path().join(".git"), "gitdir: ../elsewhere\n").unwrap();
        assert!(guard(dir.path(), true).mounts.is_empty());
    }

    #[test]
    fn odd_entries_are_reported_not_mounted() {
        let dir = crate::tempdir::tempdir().unwrap();
        let p = dir.path();
        fs::create_dir_all(p.join(".git")).unwrap();
        std::os::unix::fs::symlink("/etc", p.join(".git/hooks")).unwrap();
        fs::create_dir(p.join(".git/config")).unwrap();
        let g = guard(p, true);
        assert!(!g.protects(), "{:?}", g.mounts);
        assert_eq!(g.warnings.len(), 2, "{:?}", g.warnings);
    }

    #[test]
    fn writable_paths_are_mounted_and_created_but_never_through_links() {
        let dir = crate::tempdir::tempdir().unwrap();
        let p = dir.path();
        fs::create_dir_all(p.join("src")).unwrap();
        fs::write(p.join("Cargo.lock"), "").unwrap();
        let list = |names: &[&str]| -> Vec<(String, String)> {
            names
                .iter()
                .map(|n| (n.to_string(), "p".to_string()))
                .collect()
        };
        // A dry run leaves out what doesn't exist yet.
        let m = writable(p, &list(&["src", "target/debug", "Cargo.lock"]), false).unwrap();
        let t: Vec<&str> = m.iter().map(|m| m.target.as_str()).collect();
        assert_eq!(
            t,
            [
                format!("{WORKSPACE}/src"),
                format!("{WORKSPACE}/Cargo.lock")
            ]
        );
        assert!(m.iter().all(|m| !m.readonly));
        assert!(!p.join("target").exists());
        let m = writable(p, &list(&["target/debug"]), true).unwrap();
        assert_eq!(m.len(), 1);
        assert!(p.join("target/debug").is_dir());

        std::os::unix::fs::symlink("/etc", p.join("escape")).unwrap();
        for bad in ["escape", "escape/ssl"] {
            let err = writable(p, &list(&[bad]), true).unwrap_err().to_string();
            assert!(err.contains("symbolic link"), "{bad}: {err}");
        }
        assert!(!Path::new("/etc/ssl/target").exists());
    }
}

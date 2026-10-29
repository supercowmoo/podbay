//! The seccomp filter every podbay container runs under.
//!
//! Engines ship their own defaults, and they differ: Podman's lets any
//! container call `unshare`, `clone` with namespace flags, `setns`, `mount`,
//! `pivot_root`, `keyctl` and `ptrace`, so a session can create a user
//! namespace and hold every capability inside it, which is where most kernel
//! privilege escalations start. Docker's blocks those, but lets every
//! container `ptrace` its neighbours.
//!
//! podbay passes the same filter to both: Docker's default profile
//! (`seccomp/moby-default.json`, vendored from moby/profiles, Apache-2.0)
//! with `ptrace`, `process_vm_readv` and `process_vm_writev` allowed only
//! when the session keeps `SYS_PTRACE`, plus whatever a profile's
//! `[seccomp] allow` adds back. The profile's own capability rules still
//! apply, so `cap_add` keeps working as it does with Docker.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::hash::sha256_hex;
use crate::paths::Paths;

/// moby/profiles commit 4b8214fc4971e06172938a1f1554ccff6647ee80,
/// `seccomp/default.json`.
const MOBY_DEFAULT: &str = include_str!("seccomp/moby-default.json");

/// Calls that read or control another process. Docker allows them to every
/// container on kernels from 4.8; podbay leaves them to the `SYS_PTRACE`
/// rule that Docker's profile also has.
const PTRACE: &[&str] = &["ptrace", "process_vm_readv", "process_vm_writev"];

/// The filter as JSON, with `allow` permitted on top.
pub fn profile(allow: &[String]) -> String {
    let mut doc: Value = serde_json::from_str(MOBY_DEFAULT).expect("vendored profile is valid");
    let rules = doc["syscalls"]
        .as_array_mut()
        .expect("vendored profile has syscalls");
    for rule in rules.iter_mut() {
        if is_unconditional_allow(rule) {
            let names = rule["names"].as_array_mut().expect("rules have names");
            names.retain(|n| !PTRACE.contains(&n.as_str().unwrap_or_default()));
        }
    }
    rules.retain(|rule| !rule["names"].as_array().is_some_and(Vec::is_empty));
    if !allow.is_empty() {
        rules.push(json!({ "names": allow, "action": "SCMP_ACT_ALLOW" }));
    }
    serde_json::to_string(&doc).expect("JSON values serialize")
}

/// A rule that allows its calls to everyone: no capability, architecture or
/// kernel condition, and no argument filter.
fn is_unconditional_allow(rule: &Value) -> bool {
    let empty = |key: &str| match &rule[key] {
        Value::Null => true,
        Value::Object(map) => map.is_empty(),
        Value::Array(list) => list.is_empty(),
        _ => false,
    };
    rule["action"] == "SCMP_ACT_ALLOW"
        && ["includes", "excludes", "args"].iter().all(|k| empty(k))
        // Docker's kernel-version rule for ptrace counts as unconditional:
        // every kernel podbay runs on is newer.
        || rule["action"] == "SCMP_ACT_ALLOW"
            && empty("excludes")
            && empty("args")
            && rule["includes"]
                .as_object()
                .is_some_and(|inc| inc.keys().all(|k| k == "minKernel"))
}

/// Where the filter for `json` lives: named by its content, so concurrent
/// sessions with different profiles never overwrite each other's.
pub fn path(paths: &Paths, json: &str) -> PathBuf {
    paths
        .cache_dir
        .join("seccomp")
        .join(format!("{}.json", &sha256_hex(json.as_bytes())[..16]))
}

/// Writes the filter to [`path`] unless it's already there.
pub fn write(paths: &Paths, json: &str) -> Result<PathBuf> {
    let file = path(paths, json);
    if file.is_file() {
        return Ok(file);
    }
    let dir = file.parent().expect("seccomp files live in a directory");
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // Write then rename, so an engine never reads a half-written file.
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating a file in {}", dir.display()))?;
    tmp.write_all(json.as_bytes())?;
    tmp.persist(&file)
        .with_context(|| format!("writing {}", file.display()))?;
    Ok(file)
}

/// Syscall names as libseccomp spells them.
pub fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !valid {
        anyhow::bail!("invalid syscall name {name:?} in [seccomp] allow");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::test_paths;

    /// Names allowed with no condition at all.
    fn unconditional(doc: &Value) -> Vec<String> {
        doc["syscalls"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| is_unconditional_allow(r))
            .flat_map(|r| r["names"].as_array().unwrap().iter())
            .map(|n| n.as_str().unwrap().to_owned())
            .collect()
    }

    /// Names allowed when the container keeps `cap`.
    fn with_cap(doc: &Value, cap: &str) -> Vec<String> {
        doc["syscalls"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| {
                r["action"] == "SCMP_ACT_ALLOW"
                    && r["includes"]["caps"]
                        .as_array()
                        .is_some_and(|c| c.iter().any(|c| c == cap))
            })
            .flat_map(|r| r["names"].as_array().unwrap().iter())
            .map(|n| n.as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn upstream_still_looks_as_expected() {
        // If a profile update breaks these, revisit the transformation.
        let doc: Value = serde_json::from_str(MOBY_DEFAULT).unwrap();
        assert_eq!(doc["defaultAction"], "SCMP_ACT_ERRNO");
        let open = unconditional(&doc);
        for name in PTRACE {
            assert!(open.iter().any(|n| n == name), "{name}");
        }
        for name in [
            "unshare",
            "mount",
            "setns",
            "keyctl",
            "io_uring_setup",
            "bpf",
        ] {
            assert!(!open.iter().any(|n| n == name), "{name}");
        }
        for name in PTRACE {
            assert!(with_cap(&doc, "CAP_SYS_PTRACE").iter().any(|n| n == name));
        }
    }

    #[test]
    fn ptrace_needs_the_capability() {
        let doc: Value = serde_json::from_str(&profile(&[])).unwrap();
        let open = unconditional(&doc);
        assert!(open.iter().any(|n| n == "read"));
        for name in PTRACE {
            assert!(!open.iter().any(|n| n == name), "{name} is allowed");
            assert!(
                with_cap(&doc, "CAP_SYS_PTRACE").iter().any(|n| n == name),
                "{name} lost its SYS_PTRACE rule"
            );
        }
        assert!(!profile(&[]).contains("minKernel"));
    }

    #[test]
    fn allow_adds_a_rule() {
        let base = profile(&[]);
        let more = profile(&["ptrace".into(), "io_uring_setup".into()]);
        assert_ne!(base, more);
        let doc: Value = serde_json::from_str(&more).unwrap();
        let open = unconditional(&doc);
        assert!(open.iter().any(|n| n == "ptrace"));
        assert!(open.iter().any(|n| n == "io_uring_setup"));
        // Stable output, so the file name is too.
        assert_eq!(profile(&[]), base);
    }

    #[test]
    fn files_are_named_by_content() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let a = write(&paths, &profile(&[])).unwrap();
        assert_eq!(write(&paths, &profile(&[])).unwrap(), a);
        let b = write(&paths, &profile(&["ptrace".into()])).unwrap();
        assert_ne!(a, b);
        assert_eq!(a, path(&paths, &profile(&[])));
        let written: Value = serde_json::from_str(&fs::read_to_string(&a).unwrap()).unwrap();
        assert_eq!(written["defaultAction"], "SCMP_ACT_ERRNO");
    }

    #[test]
    fn names() {
        for ok in ["ptrace", "io_uring_setup", "clock_settime64"] {
            validate_name(ok).unwrap();
        }
        for bad in ["", "PTRACE", "1x", "a b", "a-b", "x;y"] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
    }
}

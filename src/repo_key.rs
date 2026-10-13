//! Stable, human-readable identifiers for a project directory.

use std::path::Path;

use crate::hash::sha256_hex;

/// Returns `<name>-<hash>`: the lowercased directory name for readability,
/// plus a hash of the full path so two checkouts named `app` never collide.
///
/// `path` should already be canonical; the key is derived from it verbatim.
pub fn repo_key(path: &Path) -> String {
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    let mut safe = String::with_capacity(base.len());
    for c in base.chars() {
        if c.is_ascii_alphanumeric() {
            safe.push(c);
        } else if !safe.ends_with('-') {
            safe.push('-');
        }
    }
    let safe = safe.trim_matches('-');
    let safe = if safe.is_empty() { "repo" } else { safe };

    let hash = sha256_hex(path.as_os_str().as_encoded_bytes());
    format!("{safe}-{}", &hash[..12])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readable_prefix_and_hash() {
        let key = repo_key(Path::new("/home/me/src/My Project.rs"));
        assert!(key.starts_with("my-project-rs-"), "{key}");
        assert_eq!(key.len(), "my-project-rs-".len() + 12);
    }

    #[test]
    fn same_name_different_path_differs() {
        assert_ne!(repo_key(Path::new("/a/app")), repo_key(Path::new("/b/app")));
    }

    #[test]
    fn deterministic() {
        assert_eq!(repo_key(Path::new("/a/app")), repo_key(Path::new("/a/app")));
    }

    #[test]
    fn unusable_name_falls_back() {
        assert!(repo_key(Path::new("/")).starts_with("repo-"));
        assert!(repo_key(Path::new("/x/___")).starts_with("repo-"));
    }
}

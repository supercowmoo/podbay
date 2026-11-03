//! `podbay clean`: removes what podbay can rebuild and no longer needs.
//!
//! - relay containers left by a podbay that was killed (one whose session
//!   is still running is left alone);
//! - podbay's images, except the bundled base and relay this podbay
//!   builds, which take longest to make again; everything else is rebuilt
//!   on the next run that needs it;
//! - cached seccomp filters, written again when needed.
//!
//! Volumes are listed, never removed: they hold logins, history and
//! installed tools.

use std::fs;
use std::path::Path;

/// Whether `image` (`name:tag`) is one podbay built, as either engine
/// shows it (Podman prefixes local images with `localhost/`).
pub fn is_podbay_image(image: &str) -> bool {
    image
        .strip_prefix("localhost/")
        .unwrap_or(image)
        .starts_with("podbay-")
}

/// podbay's images in `all` that aren't in `keep`, in a stable order.
pub fn images_to_remove(all: &[String], keep: &[String]) -> Vec<String> {
    let mut out: Vec<String> = all
        .iter()
        .filter(|image| is_podbay_image(image))
        .filter(|image| {
            let bare = image.strip_prefix("localhost/").unwrap_or(image);
            !keep.iter().any(|k| k == bare)
        })
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The podbay process a relay container belongs to: its name ends with
/// that process's id (`podbay-relay-<project>-<pid>`).
pub fn relay_owner(name: &str) -> Option<u32> {
    name.strip_prefix("podbay-relay-")?
        .rsplit_once('-')?
        .1
        .parse()
        .ok()
}

/// Whether the podbay process `pid` is still running, as seen in `proc`
/// (normally `/proc`). A process with that id that isn't podbay means the
/// id was reused, so the owner is gone.
pub fn owner_alive(proc: &Path, pid: u32) -> bool {
    fs::read_to_string(proc.join(pid.to_string()).join("comm"))
        .is_ok_and(|comm| comm.trim() == "podbay")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_images_go() {
        let all: Vec<String> = [
            "localhost/podbay-base:aaa",
            "podbay-base:old",
            "podbay-relay:bbb",
            "podbay-image:ccc",
            "localhost/podbay-rust:ddd",
            "alpine:3.22",
            "docker.io/library/debian:trixie-slim",
            "mypodbay-thing:1",
        ]
        .map(String::from)
        .to_vec();
        let keep = ["podbay-base:aaa".to_owned(), "podbay-relay:bbb".to_owned()];
        assert_eq!(
            images_to_remove(&all, &keep),
            [
                "localhost/podbay-rust:ddd",
                "podbay-base:old",
                "podbay-image:ccc",
            ]
        );
    }

    #[test]
    fn relay_owners() {
        assert_eq!(relay_owner("podbay-relay-app-0123abcd-4242"), Some(4242));
        assert_eq!(relay_owner("podbay-relay-my-app-1"), Some(1));
        assert_eq!(relay_owner("podbay-relay-x"), None);
        assert_eq!(relay_owner("other-relay-app-1"), None);
        assert_eq!(relay_owner("podbay-relay-app-abc"), None);
    }

    #[test]
    fn owners() {
        let proc = crate::tempdir::tempdir().unwrap();
        fs::create_dir_all(proc.path().join("10")).unwrap();
        fs::write(proc.path().join("10/comm"), "podbay\n").unwrap();
        fs::create_dir_all(proc.path().join("11")).unwrap();
        fs::write(proc.path().join("11/comm"), "bash\n").unwrap();
        assert!(owner_alive(proc.path(), 10));
        assert!(
            !owner_alive(proc.path(), 11),
            "id reused by another program"
        );
        assert!(!owner_alive(proc.path(), 12), "gone");
        // This test's own process isn't called podbay.
        assert!(!owner_alive(Path::new("/proc"), std::process::id()));
    }
}

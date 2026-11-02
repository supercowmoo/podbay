//! Every crate podbay builds, with its exact version, is on this list. A
//! new dependency, direct or pulled in by another, or any version change,
//! fails this test until someone has looked at it and added it here.
//!
//! Development and build dependencies count too: they run on developers'
//! machines and in CI.

use std::collections::BTreeSet;
use std::process::Command;

const APPROVED: &[&str] = &[
    // podbay's own crates.
    "podbay v0.1.0",
    "podbay-init v0.1.0",
    "podbay-relay v0.1.0",
    // Error context.
    "anyhow v1.0.104",
    // System calls the standard library doesn't wrap.
    "libc v0.2.190",
    // Parsing config.toml and profiles, with only the parser enabled.
    "toml v1.1.8+spec-1.1.0",
    "toml_parser v1.1.5+spec-1.1.0",
    "toml_datetime v1.1.2+spec-1.1.0",
    "serde_spanned v1.1.2",
    "winnow v1.0.4",
];

#[test]
fn only_approved_crates_are_built() {
    let output = Command::new(env!("CARGO"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "tree",
            "--locked",
            "--workspace",
            "--edges",
            "normal,build,dev",
            "--prefix",
            "none",
            "--format",
            "{p}",
        ])
        .output()
        .expect("running cargo tree");
    assert!(
        output.status.success(),
        "cargo tree failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let built: BTreeSet<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            // "name vX.Y.Z (/path) (*)" -> "name vX.Y.Z"
            let mut words = l.split_whitespace();
            format!("{} {}", words.next().unwrap(), words.next().unwrap_or(""))
        })
        .collect();
    let approved: BTreeSet<String> = APPROVED.iter().map(|s| s.to_string()).collect();

    let new: Vec<&String> = built.difference(&approved).collect();
    let gone: Vec<&String> = approved.difference(&built).collect();
    assert!(
        new.is_empty() && gone.is_empty(),
        "the crates podbay builds changed.\n  not approved: {new:?}\n  no longer built: {gone:?}\n\
         Review any new crate and its source before adding it to APPROVED in {}",
        file!()
    );
}

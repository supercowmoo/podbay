//! The translated documents stay true to the English ones.
//!
//! README.md and THREAT_MODEL.md describe what podbay protects, so a
//! translation that has fallen behind can tell a reader something that is
//! no longer true. Each translation records the SHA-256 of the English file
//! it was translated from, and this test fails as soon as the English file
//! changes, until the translation has been brought up to date and the
//! hash in its marker updated. It also checks what can be checked without
//! reading the translated prose: code blocks (commands, profiles and
//! podbay's own output) are the English ones verbatim and in order, every
//! English section has an anchor of the same name, and every link within
//! and between the documents points at an anchor that exists.
//!
//! Text between `<!-- zh-only -->` and `<!-- /zh-only -->` exists only in
//! the translation and is left out of the code block comparison.

use std::fs;
use std::path::Path;

#[allow(dead_code)] // only sha256_hex is needed here
#[path = "../src/hash.rs"]
mod hash;

/// English document, its translation.
const PAIRS: &[(&str, &str)] = &[
    ("README.md", "README.zh-CN.md"),
    ("THREAT_MODEL.md", "THREAT_MODEL.zh-CN.md"),
];

fn read(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Fenced code blocks, each with its fence lines, in order of appearance.
fn code_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let fence = line.trim_start().starts_with("```");
        match current.as_mut() {
            Some(block) => {
                block.push_str(line);
                block.push('\n');
                if fence {
                    blocks.extend(current.take());
                }
            }
            None if fence => current = Some(format!("{line}\n")),
            None => {}
        }
    }
    assert!(current.is_none(), "unterminated code block");
    blocks
}

/// `text` without the parts marked as existing only in the translation.
fn without_translation_only(text: &str, name: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("<!-- zh-only -->") {
        out.push_str(&rest[..start]);
        let end = rest[start..]
            .find("<!-- /zh-only -->")
            .unwrap_or_else(|| panic!("{name}: <!-- zh-only --> is never closed"));
        rest = &rest[start + end..];
    }
    out.push_str(rest);
    out
}

/// Headings outside code blocks, as GitHub turns them into anchors:
/// lowercased, spaces to hyphens, other punctuation dropped.
fn heading_anchors(text: &str) -> Vec<String> {
    let mut in_code = false;
    let mut anchors = Vec::new();
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code || !line.starts_with('#') {
            continue;
        }
        let title = line.trim_start_matches('#').trim();
        let anchor: String = title
            .chars()
            .filter_map(|c| match c {
                ' ' => Some('-'),
                '-' | '_' => Some(c),
                c if c.is_alphanumeric() => Some(c.to_ascii_lowercase()),
                _ => None,
            })
            .collect();
        anchors.push(anchor);
    }
    anchors
}

/// The `<a id="...">` anchors in a translation.
fn explicit_anchors(text: &str) -> Vec<&str> {
    text.split("<a id=\"")
        .skip(1)
        .filter_map(|s| s.split('"').next())
        .collect()
}

/// Link targets of the form `](file#anchor)` or `](#anchor)`, as
/// (file, anchor), with an empty file for links within the document.
fn anchor_links(text: &str) -> Vec<(&str, &str)> {
    text.split("](")
        .skip(1)
        .filter_map(|s| s.split(')').next())
        .filter_map(|target| target.split_once('#'))
        .filter(|(file, _)| !file.contains(':'))
        .collect()
}

#[test]
fn translations_match_the_english_they_were_made_from() {
    for (english, translated) in PAIRS {
        let source = read(english);
        let text = read(translated);
        let hash = hash::sha256_hex(source.as_bytes());
        let marker = format!("<!-- source: {english} sha256:{hash} -->");
        assert!(
            text.contains(&marker),
            "{translated} was translated from another version of {english}. \
             Bring the translation up to date with what changed in {english} \
             (git log -p {english}), then set its marker to:\n{marker}"
        );
    }
}

#[test]
fn translations_keep_the_english_code_blocks() {
    for (english, translated) in PAIRS {
        let theirs = code_blocks(&read(english));
        let ours = code_blocks(&without_translation_only(&read(translated), translated));
        for (i, (a, b)) in theirs.iter().zip(&ours).enumerate() {
            assert_eq!(
                a,
                b,
                "code block {} of {translated} differs from {english}'s",
                i + 1
            );
        }
        assert_eq!(
            theirs.len(),
            ours.len(),
            "{translated} has a different number of code blocks than {english}"
        );
    }
}

#[test]
fn translations_keep_the_english_anchors() {
    for (english, translated) in PAIRS {
        let text = read(translated);
        let ids = explicit_anchors(&text);
        for anchor in heading_anchors(&read(english)) {
            assert!(
                ids.contains(&anchor.as_str()),
                "{translated} has no <a id=\"{anchor}\"></a> for {english}'s section"
            );
        }
    }
}

#[test]
fn translation_links_resolve() {
    for (_, translated) in PAIRS {
        let text = read(translated);
        for (file, anchor) in anchor_links(&text) {
            let target = if file.is_empty() {
                text.clone()
            } else {
                read(file)
            };
            let found = explicit_anchors(&target).contains(&anchor)
                || heading_anchors(&target).iter().any(|a| a == anchor);
            assert!(found, "{translated}: link to {file}#{anchor} has no target");
        }
    }
}

#[test]
fn anchors_follow_github() {
    assert_eq!(
        heading_anchors("## Other channels, and the stage that closes them\n"),
        ["other-channels-and-the-stage-that-closes-them"]
    );
    assert_eq!(
        heading_anchors("```\n# not a heading\n```\n## Git hooks and config\n"),
        ["git-hooks-and-config"]
    );
}

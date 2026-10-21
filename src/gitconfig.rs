//! Just enough of git's config format to list the settings in a repository's
//! config, so the session report can say which ones changed and flag those
//! that make git run programs.
//!
//! This is not git's parser, and a crafted file may parse differently here
//! than in git. The report therefore treats any change to a config file as a
//! warning; the settings listed are a reading aid, not the verdict.

/// Settings in file order as `(name, value)`. Section and key names are
/// lowercased; subsection names keep their case, as in git. A key with no
/// `=` is a boolean and reads as `true`.
pub fn parse(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let mut section: Option<String> = None;
    let mut chars = text.chars().peekable();
    let mut line = 1;

    while let Some(&c) = chars.peek() {
        match c {
            '\n' => {
                line += 1;
                chars.next();
            }
            c if c.is_whitespace() => {
                chars.next();
            }
            '#' | ';' => skip_line(&mut chars),
            '[' => {
                chars.next();
                section = Some(header(&mut chars).map_err(|e| format!("line {line}: {e}"))?);
            }
            c if c.is_ascii_alphabetic() => {
                let Some(section) = &section else {
                    return Err(format!("line {line}: setting outside a section"));
                };
                let mut key = String::new();
                while let Some(&c) = chars.peek()
                    && (c.is_ascii_alphanumeric() || c == '-')
                {
                    key.push(c.to_ascii_lowercase());
                    chars.next();
                }
                while chars.next_if(|&c| c == ' ' || c == '\t').is_some() {}
                let value = match chars.peek() {
                    Some('=') => {
                        chars.next();
                        value(&mut chars, &mut line).map_err(|e| format!("line {line}: {e}"))?
                    }
                    None | Some('\n' | '\r' | '#' | ';') => "true".to_owned(),
                    Some(c) => return Err(format!("line {line}: unexpected {c:?} after {key}")),
                };
                out.push((format!("{section}.{key}"), value));
            }
            c => return Err(format!("line {line}: unexpected {c:?}")),
        }
    }
    Ok(out)
}

fn skip_line(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while chars.next_if(|&c| c != '\n').is_some() {}
}

/// After `[`: `section]`, `section "sub"]` or the old `section.sub]`.
fn header(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Result<String, String> {
    let mut name = String::new();
    while let Some(&c) = chars.peek()
        && (c.is_ascii_alphanumeric() || c == '-' || c == '.')
    {
        name.push(c.to_ascii_lowercase());
        chars.next();
    }
    if name.is_empty() {
        return Err("empty section name".into());
    }
    while chars.next_if(|&c| c == ' ' || c == '\t').is_some() {}
    if chars.next_if_eq(&'"').is_some() {
        name.push('.');
        loop {
            match chars.next() {
                Some('"') => break,
                Some('\\') => match chars.next() {
                    Some(c) if c != '\n' => name.push(c),
                    _ => return Err("bad escape in subsection".into()),
                },
                Some('\n') | None => return Err("unterminated subsection".into()),
                Some(c) => name.push(c),
            }
        }
    }
    match chars.next() {
        Some(']') => Ok(name),
        _ => Err("expected ']'".into()),
    }
}

/// After `=`: the rest of the value, with quotes, escapes and continuation
/// lines handled and unquoted surrounding whitespace dropped.
fn value(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    line: &mut usize,
) -> Result<String, String> {
    let mut out = String::new();
    let mut pending_space = String::new();
    let mut quoted = false;
    while chars.next_if(|&c| c == ' ' || c == '\t').is_some() {}
    loop {
        let Some(c) = chars.next() else {
            return match quoted {
                true => Err("unterminated quote".into()),
                false => Ok(out),
            };
        };
        match c {
            '\n' if quoted => return Err("unterminated quote".into()),
            '\n' => {
                *line += 1;
                return Ok(out);
            }
            '#' | ';' if !quoted => {
                skip_line(chars);
                return Ok(out);
            }
            ' ' | '\t' | '\r' if !quoted => pending_space.push(c),
            '"' => {
                out.push_str(&std::mem::take(&mut pending_space));
                quoted = !quoted;
            }
            '\\' => {
                let escaped = match chars.next() {
                    Some('n') => '\n',
                    Some('t') => '\t',
                    Some('b') => '\u{8}',
                    Some(c @ ('"' | '\\')) => c,
                    Some('\n') => {
                        *line += 1;
                        continue;
                    }
                    Some('\r') if chars.next_if_eq(&'\n').is_some() => {
                        *line += 1;
                        continue;
                    }
                    _ => return Err("bad escape".into()),
                };
                out.push_str(&std::mem::take(&mut pending_space));
                out.push(escaped);
            }
            c => {
                out.push_str(&std::mem::take(&mut pending_space));
                out.push(c);
            }
        }
    }
}

/// Whether a setting can make git run a program, load more config, or send
/// traffic somewhere else. Errs towards yes.
pub fn runs_commands(name: &str, value: &str) -> bool {
    let parts: Vec<&str> = name.split('.').collect();
    let (section, key) = (parts[0], parts[parts.len() - 1]);
    let has_subsection = parts.len() > 2;
    let exact = [
        "core.hookspath",
        "core.fsmonitor",
        "core.sshcommand",
        "core.pager",
        "core.editor",
        "core.askpass",
        "core.gitproxy",
        "core.worktree",
        "core.attributesfile",
        "core.alternaterefscommand",
        "sequence.editor",
        "diff.external",
        "gpg.program",
        "credential.helper",
        "include.path",
        "interactive.difffilter",
        "uploadpack.packobjectshook",
        "web.browser",
        "sendemail.sendmailcmd",
        "sendemail.smtpserver",
        "protocol.allow",
    ];
    let with_subsection = [
        ("diff", "textconv"),
        ("diff", "command"),
        ("merge", "driver"),
        ("filter", "clean"),
        ("filter", "smudge"),
        ("filter", "process"),
        ("credential", "helper"),
        ("gpg", "program"),
        ("includeif", "path"),
        ("difftool", "cmd"),
        ("mergetool", "cmd"),
        ("browser", "cmd"),
        ("man", "cmd"),
        ("remote", "uploadpack"),
        ("remote", "receivepack"),
        ("remote", "vcs"),
        ("url", "insteadof"),
        ("url", "pushinsteadof"),
        ("protocol", "allow"),
    ];
    exact.contains(&name)
        || (has_subsection && with_subsection.contains(&(section, key)))
        || section == "pager"
        || (section == "alias" && value.trim_start().starts_with('!'))
        || (section == "submodule" && key == "update" && value.trim_start().starts_with('!'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(text: &str) -> Vec<(String, String)> {
        parse(text).unwrap()
    }

    fn pair(name: &str, value: &str) -> (String, String) {
        (name.to_owned(), value.to_owned())
    }

    #[test]
    fn typical_repo_config() {
        let text = "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\
                    [remote \"origin\"]\n\turl = git@example.com:a/b.git\n\
                    \tfetch = +refs/heads/*:refs/remotes/origin/*\n\
                    [branch \"Main\"]\n\tremote = origin\n";
        assert_eq!(
            settings(text),
            [
                pair("core.repositoryformatversion", "0"),
                pair("core.bare", "false"),
                pair("remote.origin.url", "git@example.com:a/b.git"),
                pair("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"),
                pair("branch.Main.remote", "origin"),
            ]
        );
    }

    #[test]
    fn names_are_case_insensitive_but_subsections_are_not() {
        assert_eq!(
            settings("[CoRe]\nFsMonitor = x\n[Diff \"Foo\"]\nTextConv = y\n"),
            [pair("core.fsmonitor", "x"), pair("diff.Foo.textconv", "y")]
        );
        assert_eq!(settings("[Diff.Foo]\na = 1\n"), [pair("diff.foo.a", "1")]);
    }

    #[test]
    fn values() {
        assert_eq!(
            settings(
                "[a]\n\
                 plain =  spaced   out  # comment\n\
                 quoted = \" keep ; this \"\n\
                 escapes = a\\tb\\\"c\\\\\n\
                 cont = one \\\n  two\n\
                 bool\n\
                 empty =\n"
            ),
            [
                pair("a.plain", "spaced   out"),
                pair("a.quoted", " keep ; this "),
                pair("a.escapes", "a\tb\"c\\"),
                pair("a.cont", "one   two"),
                pair("a.bool", "true"),
                pair("a.empty", ""),
            ]
        );
    }

    #[test]
    fn setting_on_header_line() {
        assert_eq!(
            settings("[core] fsmonitor = x\n"),
            [pair("core.fsmonitor", "x")]
        );
    }

    #[test]
    fn rejects_what_it_cannot_read() {
        for bad in [
            "key = 1\n",
            "[core\n",
            "[a]\nk = \"open\n",
            "[a]\nk = bad\\q\n",
            "[a]\n%x\n",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn dangerous_settings() {
        for (name, value) in [
            ("core.fsmonitor", "x"),
            ("core.hookspath", ".hooks"),
            ("diff.foo.textconv", "x"),
            ("filter.lfs.smudge", "x"),
            ("gpg.ssh.program", "x"),
            ("alias.st", "!sh -c x"),
            ("pager.log", "x"),
            ("includeif.gitdir:/x/.path", "x"),
            ("url.https://evil/.insteadof", "https://github.com/"),
        ] {
            assert!(runs_commands(name, value), "{name} not flagged");
        }
        for (name, value) in [
            ("alias.st", "status"),
            ("user.name", "x"),
            ("branch.main.remote", "origin"),
            ("diff.renames", "true"),
        ] {
            assert!(!runs_commands(name, value), "{name} flagged");
        }
    }
}

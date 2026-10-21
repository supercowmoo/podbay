# Threat model

This document covers what podbay protects, from whom, and where the
boundary currently has holes. It describes the code as it is now, at the end
of stage 2. Later stages close specific gaps, and each gap below names the
stage that addresses it.

## What podbay is for

You run code you don't fully trust against a codebase: an AI coding agent,
a project's build or test scripts, dependencies with install hooks. podbay
puts that code in a disposable container. The goal is that the code can do
anything it likes to the project, and nothing to the rest of your machine
except what you declared.

## Assets

In rough order of value:

1. **The host account.** Code execution as you, outside the container, is
   the main thing to prevent.
2. **Secrets on the host**: SSH and GPG keys, cloud credentials, browser
   sessions, tokens in your shell environment.
3. **Secrets you mount in**: whatever your profile shares (a `~/.gitconfig`,
   an agent's auth volume).
4. **Other projects**, and the state podbay shares between projects (mise
   tools, profile volumes).
5. **The project itself.** It's mounted read-write by design. Damage to it is
   in scope only where it turns into one of the above.

## Adversary

The adversary is the code running in the container, or whoever controls it:
a malicious repository or dependency, or an agent that has been
prompt-injected. Assume it is root inside the container (it may find a way
to escalate there) and knows exactly how podbay works.

The following are trusted: you, your podbay config directory
(`~/.config/podbay`, including profiles and extension scripts), the podbay
binary, the container engine, the kernel and the base image's upstream
sources.

## Trust boundaries

| Boundary | Enforced by | Notes |
| --- | --- | --- |
| Container to host processes | Engine namespaces, kernel | Default engine settings until stage 3 |
| Container to host files | Only declared mounts are visible | The project is read-write |
| Container to host network | **Not enforced** | Unrestricted until stage 4 |
| Project to image build | Extensions are read only from your config dir | A project can never add root build steps |
| Session to session | `--rm`, per-profile volumes | Some state is shared, see below |
| Container output to host | Session report, escaping | See "The project as a channel" |

## The project as a channel back to the host

This is the main thing stage 2 addresses. The container cannot run code on
the host directly, but it can write files there that the host later runs:

- **Git hooks** in `.git/hooks/`, or wherever `core.hooksPath` points. Git
  runs them on commit, checkout, merge, rebase and push.
- **Git config** settings that name programs: `core.fsmonitor` (run by
  almost any git command, including `git status` and many shell prompts),
  `core.pager`, `core.editor`, `core.sshCommand`, diff and merge drivers,
  filters, credential helpers, `!` aliases, `include.path`, and others.
- **The `.git` entry itself.** Replacing it with a file
  (`gitdir: somewhere/else`) or a different directory points host git at a
  repository the container wrote.
- **Nested repositories.** A `.git` in a subdirectory takes over when you
  run git in that subdirectory, and brings its own hooks and config.
  Creating one in a project that has no `.git` of its own also shadows any
  repository above it.
- **Editor and shell auto-run files**: `.envrc` (direnv), `.vscode/` tasks
  and settings, dev container definitions, mise config with hooks and
  tasks. Most of these tools ask for trust first, but some trust a path
  rather than its contents, so an edit can slip past an earlier approval.

git's `safe.directory` check does not help: files the container writes are
owned by you on the host.

### The session report

After every `podbay run`, podbay reports changes to the files above:

- The project's git directory is snapshotted by content before the
  container starts and compared after it exits. This covers `.git` itself,
  config, `config.worktree`, `info/attributes`, `commondir`, every entry in
  the hooks directory, the `core.hooksPath` directory and included config
  files when they're inside the project, submodule git directories under
  `.git/modules`, and linked worktree config. Config changes are listed
  setting by setting, and settings that can run commands are marked.
- The rest of the tree is scanned for `.git` entries below the top level,
  and nested repositories are flagged when their config, hooks or
  equivalent changed during the session. Detection uses ctime, which the
  kernel sets and nothing in the container can backdate. The listed
  editor and shell files are checked the same way.
- Read-write file mounts from your profile are checked by ctime too.

The reading side is built to work on hostile input. podbay never follows
symlinks, including in parent directories. It opens only regular files,
with `O_NOFOLLOW | O_NONBLOCK`, and reads at most 64 MiB. Every string from
the project is escaped before printing, so terminal escape sequences and
bidirectional overrides can't hide or rewrite the report. Before printing,
podbay also resets text attributes and the character set, which the
container controlled until it exited.

podbay ignores Ctrl-C while it waits for the engine, as `system(3)` does.
The interrupt still reaches the container, and the report still prints.

### Limits of the report

The report is a tripwire, not a guarantee. It tells you to look before you
run git; it does not stop anything.

- **It only reports.** Nothing is reverted or quarantined, and the exit code
  is the container's. If you don't read the report, it doesn't help.
- **No report after a failed run.** If podbay itself is killed (SIGKILL,
  terminal closed), there is no report. If you're unsure whether a session
  was clean, check `.git/hooks` and `.git/config` yourself before using git.
- **podbay's config parser is not git's.** A crafted file could parse
  differently in each. Any byte-level change to a config file is therefore a
  warning, whether or not a setting changed. The settings list helps you
  judge the change; the warning doesn't depend on it.
- **The command list is incomplete by nature.** Settings outside it are
  still reported as changes, just not marked as runnable.
- **Concurrent sessions** on the same project share one working tree. One
  session's report can include changes made by another, and changes made
  after a report is printed are not covered.
- **Read-write directory mounts** from profiles are not inspected.
- **Large trees.** The scan stops after 500,000 entries and says so.
- **Not covered at all**: anything else in the working tree that you later
  run on the host, such as `Makefile`, `package.json` scripts, test
  fixtures, binaries or a `.cargo/config.toml`. Treat the project's working
  tree as untrusted output, and review the diff before building or running
  it outside podbay. Once the report is clean, `git diff` and `git status`
  on the host are safe ways to review it.
- **Git's own parsers.** Repository data (objects, packs, refs, the index)
  written by the container is read by host git. Bugs in git's handling of
  malformed data are out of scope.

## Other channels, and the stage that closes them

- **Network (stage 4).** The container has full network access. Anything it
  can read (the project, mounted files, volumes) can be exfiltrated, and it
  can reach services on your LAN and, depending on the engine, on the
  host's loopback.
- **Privilege inside the container (stage 3).** Default capabilities, no
  `no-new-privileges` and a writable root filesystem make escalation to root
  inside the container easier, and container root is closer to the kernel.
  On rootless engines that root is still your unprivileged user. On rootful
  Docker a container escape is real root, which podbay warns about on every
  run. Prefer rootless Podman.
- **Resource exhaustion (stage 3).** There are no CPU, memory or process
  limits, so a fork bomb or a memory hog affects the host.
- **Kernel attack surface (stages 3 and 5).** No custom seccomp or Landlock
  policy yet beyond the engine's defaults.
- **The terminal.** With `--tty`, the container writes directly to your
  terminal. Escape sequences can change the window title, write to the
  clipboard (OSC 52) on terminals that allow it, or hit bugs in the
  terminal emulator. The report resets styling but cannot undo everything.
  Use a terminal that disables clipboard writes and title reporting by
  default.

## Shared state between projects

- **`podbay-mise-data`**, the shared mise volume. A malicious project can
  install or replace tools that other projects then run. Use
  `--local-cache` for untrusted code. It keeps that project's installs in
  `~/.cache/podbay/mise/<repo-key>/`.
- **Profile volumes** are shared by every project run with that profile (and
  every profile that extends it). A malicious project can tamper with an
  agent's config or steal its credentials there, and that affects the next
  project run with the same profile. Keep a separate profile for untrusted
  work.
- **Images** are built only from your config directory, so a project cannot
  change them.

## Non-goals

- Protecting the project from the code you run in it.
- Hiding the project's contents from that code.
- Defending against a malicious podbay config, base image or engine.

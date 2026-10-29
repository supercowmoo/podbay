# Threat model

This document covers what podbay protects, from whom, and where the
boundary currently has holes. It describes the code as it is now, at the end
of stage 4. Later stages close specific gaps, and each gap below names the
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
4. **Other projects**, and the state podbay shares between projects
   (provisioned toolchains, profile volumes).
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
| Container to host processes | Engine namespaces, no capabilities, `no-new-privileges`, podbay's seccomp filter, kernel | See "Privilege inside the container" |
| Container to host files | Only declared mounts are visible | The project is read-write |
| Container to network | `--network=none`, or a relay and podbay's allowlist | A profile can still opt into full access |
| Provisioning to network | **Not enforced** | Runs your installers on project-chosen versions; see "Provisioning" |
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

- **Network (reduced in stage 4).** A session has only a loopback
  interface by default, with its own network namespace, so it can reach
  neither the internet, the LAN nor the host's loopback.
  - **Allowlisted sessions** share the network namespace of a relay
    container that has no network either, so they too see only loopback.
    The only way out is the relay's proxy port, which forwards to podbay on
    the host over a Unix socket. podbay allows HTTPS (`CONNECT`) to listed
    names only, never resolves anything else, refuses IP literals, and
    refuses any name that resolves to a non-public address, so an
    allowlisted name pointed at 127.0.0.1, the LAN or 169.254.169.254 gets
    nowhere. What remains:
    - Anything the session can read can be sent to an allowed host. Allow
      hosts that only accept data you'd be fine with ending up there: an
      API the agent authenticates to with your key is reachable by anything
      in the session that can read that key.
    - A wildcard such as `*.example.com` lets the session look up any name
      below it, which reaches example.com's DNS servers, and connect to any
      of its hosts. Prefer exact names.
    - Requests are filtered by name only. A server behind an allowed name
      that fronts other sites (some CDNs) could be asked for them inside
      the TLS stream, which podbay doesn't see.
    - The policy runs inside podbay, so the session has no network access
      once podbay exits; if podbay is killed, the relay container is left
      behind but has nothing to forward to.
    - On rootless Podman, the session also shares the relay's user
      namespace, which it needs to join its network namespace; both use the
      same mapping. With the runc runtime, such a session sees the host's
      `/sys` read-only instead of its own, which exposes interface names and
      hardware details but no network access.
  - **Full access** (`network = "full"` in a profile or `config.toml`, or
    `--online`) gives the session the engine's network. It can send
    anything it can read (the project, mounted files, volumes) anywhere, and
    reach your LAN and, depending on the engine, the host's loopback.
    podbay states it on every run that has it, and warns when a profile is
    what turned it on.
  - Provisioning, below.
- **Provisioning (stage 4).** Tools are installed before the session, in a
  container with full network access that runs the profile's `[provision]`
  commands. The commands come only from your config directory. The
  container gets the project read-only, the volumes marked
  `provision = true`, and the profile's `[env]` and `path`, and nothing
  else: no bind mounts, no other volumes, no `cap_add`, the same hardening
  as a session otherwise. Which versions get installed is usually chosen by
  project files (`rust-toolchain.toml`, `.nvmrc`, lockfiles), and many
  installers run code at install time, so this container can send the
  project's contents out. That's the non-goal below rather than a new
  exposure, but it means a project can reach the network once per change
  to the files the profile watches, without a session having network
  access. A session can trigger this for the next run by editing those
  files, and the next run says so when it provisions. Provisioning writes
  to the provision volumes, so the shared-volume caveat under "Shared
  state" applies to it; with `--local-cache` the writes stay in that
  project's own copies.
- **The profile's `path`.** Its directories usually live in volumes the
  session can write. They apply only to the session's own processes: the
  entrypoint resolves everything it runs as root from the image's PATH
  before adding them.
- **Privilege inside the container (reduced in stage 3).** The session
  runs as `dev` with an empty capability set, including the bounding set,
  and `no-new-privileges`. setuid binaries and file capabilities in the
  image give nothing. The root filesystem is read-only; only `/home/dev`,
  `/tmp` and `/var/tmp` (tmpfs, fresh each session) and the declared
  mounts are writable. The entrypoint runs as (namespaced) root with
  CHOWN, DAC_OVERRIDE, FOWNER, SETGID, SETPCAP and SETUID, for under a
  second, before your command runs. Two gaps remain:
  - A profile's `cap_add` hands capabilities back on purpose. podbay
    prints them on every run, but some (`SYS_ADMIN`, `SYS_MODULE`,
    `SYS_RAWIO`, `NET_ADMIN`, `DAC_READ_SEARCH`, ...) amount to giving up
    most of the boundary.
  - On rootful Docker a container escape is real root, which podbay warns
    about on every run. Prefer rootless Podman.
- **Resource exhaustion (reduced in stage 3).** Processes and threads are
  capped at 4096 by default; memory and CPU caps are opt-in. On rootless
  engines a limit only applies when systemd delegates that cgroup
  controller to your user. podbay checks this on Podman and warns when it
  has to run without a limit. tmpfs space counts against memory, so a
  session can still fill RAM through `/tmp` unless a memory limit is set.
  Disk usage in the project and volumes is not limited.
- **Kernel attack surface (reduced in stage 5).** Every container podbay
  starts (sessions, provisioning, the relay) runs under podbay's seccomp
  filter instead of the engine's default. It is Docker's default profile
  with `ptrace`, `process_vm_readv` and `process_vm_writev` also blocked
  unless the session keeps `SYS_PTRACE`. That matters most on Podman, whose
  default lets a container `unshare` a user namespace and then hold every
  capability inside it, which most kernel privilege escalations start
  from, plus `mount`, `setns` and `keyctl`. Three gaps remain:
  - A profile's `[seccomp] allow` and `cap_add` reopen calls on purpose;
    podbay prints them on every run. `engine_default = true` drops
    podbay's filter altogether, which podbay also prints.
  - Syscalls the filter allows can still have kernel bugs. The filter
    limits which kernel code is reachable, not whether it is correct.
  - No Landlock policy yet, so within the container, file access is
    limited only by ownership and the read-only root filesystem.
- **The terminal.** With `--tty`, the container writes directly to your
  terminal. Escape sequences can change the window title, write to the
  clipboard (OSC 52) on terminals that allow it, or hit bugs in the
  terminal emulator. The report resets styling but cannot undo everything.
  Use a terminal that disables clipboard writes and title reporting by
  default.

## Shared state between projects

- **Provision volumes**, where toolchains are installed. A malicious
  project can install or replace tools that later projects using the same
  profile then run. Use `--local-cache` for untrusted code. It gives
  that project its own copies of the provision volumes
  (`podbay-<profile>-<name>-<repo-key>`).
- **Profile volumes** are shared by every project run with that profile (and
  every profile that extends it). A malicious project can tamper with an
  agent's config or steal its credentials there, and that affects the next
  project run with the same profile. Keep a separate profile for untrusted
  work.
- **Images** are built only from your config directory, so a project cannot
  change them.

## Non-goals

- Protecting the project from the code you run in it.
- Hiding the project's contents from that code, or from the installers
  your provisioning commands run for it.
- Defending against a malicious podbay config, base image or engine.

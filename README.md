# Podbay

A generic podman runtime that I use. It launches a disposable container around a codebase you choose to work in.

The base image (`podbay-base`) is small and adaptable. It is Debian slim with apt essentials, mise, and non-root user (`dev`). Everything specific to you and your codebase arrives at runtime: the codebase is mounted at `/home/dev/workspace/repomount`, personal dotfiles are mounted read-only, mutable tool state lives in named volumes, and mise installs what the codebase and your profile asks for.

Anything that grants elevated privilege, such as additions to the image, can only be requested from user-owned space, never from within the container. This means that absent a kernel exploit, the only thing that can be modified from inside the container is the codebase and the optional repo cache (which has added layers of verification).

## Status

Rebuilding from a local prototype. Usable core: base image, run script, profiles.

## Quick start

```
podman build -t podbay-base:latest base/
./bin/podbay --project /some/repo
```

Inside, mise automatically installs whatever the repo's own config
(`.mise.toml`, `.nvmrc`, ...) asks for.
Your shell starts in `/home/dev/workspace/repomount` as `dev`.

### A note on trust

mise can install tools from many sources including random repositories. A
malicious repo could declare a custom tool that installs malware. By default
the shared mise cache (`podbay-mise-cache`) means that install would persist
and be visible to other repos using the same cache. If you are working with
code you do not trust, use `--local-cache` to keep that repo's installs
isolated to `~/.cache/podbay/mise-caches/<repo-key>/`. You should also be
careful with mise.toml. I personally only use it unless I'm declaring
something more custom. Simply auditing mise.toml yourself before running
intalls will solve this.

## Profiles

A codebase will only declare what it needs. For the tools and files you always
want, make a profile:

```
./bin/podbay --init-profile example0
```

That creates `~/.config/podbay/profiles/example0/` with four editable files:

- **`mounts.toml`** - generic bind mounts of your files into the container.
  Each entry maps a host path to a container path (`~` is allowed), mounted
  read-only by default. A host path that doesn't exist is skipped with a
  warning, not an error. Best for host dotfiles you edit outside the container
  (e.g. `.gitconfig`, `.zshrc`).
- **`volumes.toml`** - named Podman volumes for state that should persist
  across disposable container runs. Use these for directories the container
  needs to create or write into, such as tool config, auth files, caches, and
  themes. Volumes are namespaced per profile and survive `--rm`.
- **`mise-global.toml`** - personal tools, installed by mise into every
  session. These are additive with whatever the codebase asks for, so they're
  always available without touching the codebase config.
- **`extension.sh`** - optional build-time extension script. If present, it is
  baked into a derived image and executed as root during the build. Use this
  for system-level dependencies that mise cannot install, such as the shared
  libraries needed by Chromium/Playwright.

Then run with the profile:

```
./bin/podbay --project /some/repo --profile example0
```

`~/.config/podbay/profiles/common/mounts.toml`, `common/volumes.toml`, and
optionally `common/extension.sh` are always merged in first, regardless of
`--profile` - put things that belong to "you" generally (like `.gitconfig`)
in `common/`, and workflow-specific mounts, volumes, and image extensions in
the named profile.
`./bin/podbay --list-profiles` shows what you have.

## Setting up a good profile

Put host dotfiles you edit on the host in `common/mounts.toml`:

```toml
# ~/.config/podbay/profiles/common/mounts.toml
[[mount]]
host = "~/.gitconfig"
container = "/home/dev/.gitconfig"
mode = "ro"

[[mount]]
host = "~/.zshrc"
container = "/home/dev/.zshrc"
mode = "ro"
```

Put workflow-specific tools and their persistent state in a named profile.
For example, an `ai` profile for opencode:

```toml
# ~/.config/podbay/profiles/ai/volumes.toml
[[volume]]
name = "opencode-config"
container = "/home/dev/.config/opencode"

[[volume]]
name = "opencode-data"
container = "/home/dev/.local/share/opencode"
```

```toml
# ~/.config/podbay/profiles/ai/mise-global.toml
[tools]
opencode = "latest"
```

Run with:

```
./bin/podbay --project /some/repo --profile ai
```

`mise` installs opencode, and its auth, skills, agents, and themes persist
in the named volumes across sessions even though the container is launched
with `--rm`.

### Mounts vs volumes

Use **`mounts.toml`** for files you own and edit on the host (read-only
dotfiles). Use **`volumes.toml`** for directories the container needs to
create or write into. If a bind mount points at a parent directory that does
not already exist in the image, Podman may create it as root, and the
container user will not be able to write there — that is exactly the case
volumes solve.


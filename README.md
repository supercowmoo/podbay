# Podbay

A generic podman runtime that I use. It launches a disposable container around a codebase you choose to work in.

The base image (`podbay-base`) is small and adaptable. It is Debian slim with apt essentials, mise, and non-root user (`dev`). Everything specific to you and your codebase arrives at runtime through mounts: the codebase is mounted at `/workspace`, personal tool config is mounted in read-only, and mise installs what the codebase and your profile asks for.

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
Your shell starts in `/workspace` as `dev`.

## Profiles

A codebase will only declare what it needs. For the tools and files you always
want, make a profile:

```
./bin/podbay --init-profile example0
```

That creates `~/.config/podbay/profiles/example0/` with two editable files:

- **`mounts.toml`** - generic bind mounts of your files into the container.
  Each entry maps a host path to a container path (`~` is allowed), mounted
  read-only by default. A host path that doesn't exist is skipped with a
  warning, not an error.
- **`mise-global.toml`** - personal tools, installed by mise into every
  session. These are additive with whatever the codebase asks for, so they're
  always available without touching the codebase config.

Then run with the profile:

```
./bin/podbay --project /some/repo --profile example0
```

`~/.config/podbay/profiles/common/mounts.toml` is always merged in first,
regardless of `--profile` - put things that belong to "you" generally (like
`.gitconfig`) in `common/`, and workflow-specific mounts in the named
profile. `./bin/podbay --list-profiles` shows what you have.


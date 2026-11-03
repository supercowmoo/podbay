# Working on podbay

Instructions for coding agents (and people) changing this repository.
README.md says what podbay does; THREAT_MODEL.md says what it protects and
what it doesn't. Read both before changing behaviour, and keep both true:
a change to what a session can do, see or reach updates them in the same
change.

Both have Simplified Chinese translations, `README.zh-CN.md` and
`THREAT_MODEL.zh-CN.md`, and they are kept true the same way: a change to
the English updates the translation in the same change. Each translation
names the SHA-256 of the English it matches, and `tests/translations.rs`
fails until that marker is updated, which is only done after the
translated text has been. Code blocks stay the English ones verbatim, since
they show podbay's own (English) output; translation-only notes go between
`<!-- zh-only -->` and `<!-- /zh-only -->`. Say in the handover if the
translation was done by an agent and not checked by a Chinese reader.

## How the maintainer works

- **Never push, and never open pull requests.** Hand over the work as a
  plain diff against the current `origin/master` (`git diff`, including
  new files via `git add -N`), saved as a downloadable file, plus a commit
  message in its own file. The maintainer applies, commits and pushes.
- One diff per round, applied on its own. If a new diff replaces an
  earlier one that wasn't pushed, say so.
- Before building on a push, check CI passed on it.
- Commit messages: a short subject line, then prose paragraphs wrapped at
  72 columns: what changed and why, then what was tested and how, and
  plainly what wasn't.
- Be exact about testing. A test that skipped is not a pass (see
  `PODBAY_TEST_REQUIRE_ENGINE` below), and anything checked only by hand,
  or not at all, is called out.

## Layout

- `src/` is the `podbay` binary. `plan.rs` turns a resolved profile into
  engine arguments and is pure; `commands.rs` runs things; `backend.rs` is
  the engine boundary (the `Backend` trait), where a native runtime would
  plug in.
- `init/` is podbay-init, the first process in every container: it sets up
  the session and drops privileges. `relay/` is the network relay
  (allowlist proxy and forwarded ports). Both are standard library only,
  built inside containers with plain `rustc`, and stay that way.
- `base/` is the bundled base image. Every image, bundled or not, gets
  podbay-init and the dev user through the adapter layer in `src/image.rs`,
  which runs nothing inside the image.
- `examples/` holds profiles that `tests/cli.rs` resolves, so they can't go
  stale. `src/seccomp/moby-default.json` is vendored from Docker
  (Apache-2.0); don't edit it, change `src/seccomp.rs` instead.

## Checks

```
cargo fmt --check
cargo clippy --locked --workspace --all-targets -- -D warnings
PODBAY_TEST_REQUIRE_ENGINE=1 cargo test --locked --workspace
PODBAY_TEST_BACKEND=podman PODBAY_TEST_REQUIRE_ENGINE=1 cargo test --locked --test e2e
```

The end-to-end tests skip silently when their engine isn't available;
`PODBAY_TEST_REQUIRE_ENGINE=1` makes that a failure. Run them on Docker and
on rootless Podman, and as a non-root user: running as root takes a
different path (the session stays root), so root-only runs miss the common
case. `podbay doctor` explains a machine that can't run them. Docker Hub
rate-limits pulls (`429 Too Many Requests`); that's not a podbay failure.

## Rules that keep podbay safe

- **Dependencies:** no new crate, and no version change, without the
  maintainer agreeing. `tests/dependencies.rs` lists every crate built and
  fails otherwise. Prefer a small, tested piece of standard-library code
  over a dependency.
- **Pin everything:** crate versions exactly (`=x.y.z`, built with
  `--locked`), images by digest, CI actions by commit, and downloads in
  examples by version and checksum.
- **The project is untrusted.** Nothing read from the mounted project may
  choose profiles, mounts, network, capabilities or anything that runs as
  root. Only the user's config directory can.
- **Files from a session are hostile.** Code running as root, on the host
  or in podbay-init, never follows symbolic links (`symlink_metadata`,
  `lchown`, `O_NOFOLLOW`), bounds what it reads, and escapes anything it
  prints.
- **Secrets stay out of output:** values from `env_from_host` reach the
  engine by name only, never on a command line, in `--dry-run` or in a
  message.
- **Behaviour changes are tested end to end** on a real engine, including
  the attack they're meant to stop (see the git-protection and `writable`
  tests), and documented in README.md and THREAT_MODEL.md.

## Style

- Comments say why, in plain sentences; doc comments on everything
  non-obvious.
- User-facing messages start with `podbay:`, say what happened and what to
  do, and match the wording in the README.
- Errors in config and profiles point at the line and column
  (`src/settings.rs`).

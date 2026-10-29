#!/usr/bin/env bash
# Starts as (possibly namespaced) root with a handful of capabilities, sets up
# the dev user's home on its tmpfs, fixes ownership of mount points, then
# drops to dev with no capabilities beyond those the profile asked for.
#
# Inputs from podbay:
#   PODBAY_UID / PODBAY_GID   ids dev runs as; 0 means stay root (the
#                             engine is rootless and root is the host user)
#   PODBAY_MOUNT_PATHS        colon-separated mount points; their parent
#                             directories must belong to dev
#   PODBAY_VOLUME_PATHS       colon-separated mount points dev must own
#   PODBAY_CAPS               comma-separated capabilities to keep, such as
#                             sys_ptrace (empty: none)
#   PODBAY_WORKSPACE          directory to start in
#   PODBAY_PATH               colon-separated directories to put in front
#                             of PATH (the profile's `path`)
set -euo pipefail

workspace=${PODBAY_WORKSPACE:-/home/dev/workspace/repomount}
skeleton=/usr/local/share/podbay/home

# The profile's PATH entries apply to the session only. They usually point
# into volumes the session can write, so nothing this script runs as root
# may be looked up there.
session_path() {
  if [ -n "${PODBAY_PATH:-}" ]; then
    export PATH="$PODBAY_PATH:$PATH"
  fi
}

if [ "$(id -u)" -ne 0 ]; then
  cd "$workspace"
  session_path
  exec "$@"
fi
setpriv=$(command -v setpriv)

uid=${PODBAY_UID:-1000}
gid=${PODBAY_GID:-1000}
export HOME=/home/dev USER=dev LOGNAME=dev

# setpriv capability lists: "-all" plus "+cap" for each kept capability.
caps="-all"
IFS=',' read -ra keep <<< "${PODBAY_CAPS:-}"
for cap in "${keep[@]}"; do
  [ -n "$cap" ] && caps+=",+$cap"
done

check_dev() {
  local have
  have="$(id -u dev):$(id -g dev)"
  if [ "$have" != "$uid:$gid" ]; then
    echo "podbay: the image's dev user is $have, expected $uid:$gid;" \
      "rebuild with podbay build" >&2
    exit 1
  fi
}

# /home/dev is a fresh tmpfs, so copy in what the image put there. Files the
# engine already placed (bind-mounted dotfiles) are left alone.
seed_home() {
  if [ -d "$skeleton" ]; then
    cp -a --update=none "$skeleton/." /home/dev/
  fi
  chown "$uid:$gid" /home/dev
}

own_mount_parents() {
  local IFS=':' path dir
  for path in ${PODBAY_MOUNT_PATHS:-}; do
    [ -n "$path" ] || continue
    # Engines create missing parents of a mount point as root.
    dir=$(dirname "$path")
    while [ "$dir" != /home/dev ] && [ "$dir" != / ]; do
      [ "$(stat -c %u "$dir")" = "$uid" ] || chown "$uid:$gid" "$dir"
      dir=$(dirname "$dir")
    done
  done
}

own_volume_paths() {
  local IFS=':' path
  for path in ${PODBAY_VOLUME_PATHS:-}; do
    [ -n "$path" ] || continue
    mkdir -p "$path"
    # Recurse only when the volume is new or the host user changed.
    if [ "$(stat -c %u "$path")" != "$uid" ]; then
      chown -R "$uid:$gid" "$path"
    fi
  done
}

if [ "$uid" -ne 0 ]; then
  check_dev
fi
seed_home
own_mount_parents
own_volume_paths

cd "$workspace"
session_path
if [ "$uid" -eq 0 ]; then
  # Root inside a rootless engine: keep the uid, lose the capabilities.
  exec "$setpriv" --inh-caps="$caps" --bounding-set="$caps" -- "$@"
fi
# Dropping from root to dev clears every capability; kept ones come back
# through the ambient set. The bounding set stops anything else returning.
exec "$setpriv" --reuid="$uid" --regid="$gid" --init-groups \
  --inh-caps="$caps" --ambient-caps="$caps" --bounding-set="$caps" -- "$@"

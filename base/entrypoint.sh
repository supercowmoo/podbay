#!/usr/bin/env bash
# Starts as (possibly namespaced) root, makes the dev user match the host
# user, fixes ownership of volume mount points, then drops privileges.
#
# Inputs from podbay:
#   PODBAY_UID / PODBAY_GID   ids dev should have; 0 means stay root (the
#                             engine is rootless and root is the host user)
#   PODBAY_VOLUME_PATHS       colon-separated mount points dev must own
#   PODBAY_WORKSPACE          directory to start in
set -euo pipefail

workspace=${PODBAY_WORKSPACE:-/home/dev/workspace/repomount}

if [ "$(id -u)" -ne 0 ]; then
  cd "$workspace"
  exec "$@"
fi

uid=${PODBAY_UID:-1000}
gid=${PODBAY_GID:-1000}
export HOME=/home/dev USER=dev LOGNAME=dev

if [ "$uid" -eq 0 ]; then
  cd "$workspace"
  exec "$@"
fi

remap_dev() {
  local old_uid old_gid
  old_uid=$(id -u dev)
  old_gid=$(id -g dev)
  if [ "$uid" = "$old_uid" ] && [ "$gid" = "$old_gid" ]; then
    return 0
  fi
  # Edit the databases directly: usermod -u would also chown everything
  # under the home directory, including the mounted project.
  sed -i "s/^dev:x:${old_uid}:${old_gid}:/dev:x:${uid}:${gid}:/" /etc/passwd
  sed -i "s/^dev:x:${old_gid}:/dev:x:${gid}:/" /etc/group
  # Re-own what the image put in the home directory. -xdev keeps find out
  # of bind mounts and volumes.
  find /home/dev -xdev \( -uid "$old_uid" -o -gid "$old_gid" \) \
    -exec chown -h "$uid:$gid" {} +
}

own_volume_paths() {
  local IFS=':' path dir
  for path in ${PODBAY_VOLUME_PATHS:-}; do
    [ -n "$path" ] || continue
    mkdir -p "$path"
    # Engines create missing parents of a mount point as root.
    dir=$(dirname "$path")
    while [ "$dir" != /home/dev ] && [ "$dir" != / ]; do
      [ "$(stat -c %u "$dir")" = "$uid" ] || chown "$uid:$gid" "$dir"
      dir=$(dirname "$dir")
    done
    # Recurse only when the volume is new or the host user changed.
    if [ "$(stat -c %u "$path")" != "$uid" ]; then
      chown -R "$uid:$gid" "$path"
    fi
  done
}

remap_dev
own_volume_paths

cd "$workspace"
exec setpriv --reuid="$uid" --regid="$gid" --init-groups -- "$@"

#!/usr/bin/env bash
set -euo pipefail

ensure_volume_paths() {
  # PODBAY_VOLUME_PATHS is a colon-separated list of declared volume mount
  # points. Podman may have created missing parent directories as root when
  # the container started; fix ownership so the unprivileged user can write.
  if [ -z "${PODBAY_VOLUME_PATHS:-}" ]; then
    return 0
  fi

  local IFS=':'
  local path
  for path in $PODBAY_VOLUME_PATHS; do
    [ -n "$path" ] || continue
    mkdir -p "$path" 2>/dev/null || true
    chown -R dev:dev "$path" 2>/dev/null || true
  done
}

if [ "$(id -u)" -eq 0 ]; then
  chown -R dev:dev /home/dev 2>/dev/null || true
  ensure_volume_paths
  exec gosu dev "$0" "$@"
fi

cd /workspace
exec "$@"

#!/usr/bin/env bash
set -euo pipefail

if [ "$(id -u)" -eq 0 ]; then
  chown -R dev:dev /home/dev 2>/dev/null || true
  exec gosu dev "$0" "$@"
fi

cd /workspace
exec "$@"

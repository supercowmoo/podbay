#!/usr/bin/env bash

repo_key_for() {
  local path="$1"
  local real base safe hash

  real=$(realpath -m "$path")
  base=$(basename "$real")
  safe=$(printf '%s' "$base" | tr '[:upper:]' '[:lower:]' | tr -cs 'a-z0-9-' '-')
  safe=${safe#-}
  safe=${safe%-}
  if [ -z "$safe" ]; then
    safe=repo
  fi
  hash=$(printf '%s' "$real" | sha256sum | cut -c1-8)
  printf '%s-%s\n' "$safe" "$hash"
}

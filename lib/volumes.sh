#!/usr/bin/env bash

# Shared functions for parsing Podbay profile volumes.toml files.
# The trim helper is duplicated here so this file can be sourced independently.

trim() {
  local s="$1"
  s=${s#"${s%%[![:space:]]*}"}
  s=${s%"${s##*[![:space:]]}"}
  printf '%s' "$s"
}
# Each [[volume]] entry declares a named Podman volume that persists across
# disposable container runs.

sanitize_volume_namespace() {
  local ns="$1"
  # Keep only safe characters for a Podman volume name component.
  ns=${ns//[^a-zA-Z0-9_.-]/-}
  # Ensure it doesn't start with a dot or hyphen.
  ns=${ns/#./}
  ns=${ns/#-/}
  printf '%s' "$ns"
}

validate_volume_name() {
  local name="$1"
  local file="$2"
  if [[ ! "$name" =~ ^[a-zA-Z0-9][a-zA-Z0-9_.-]*$ ]]; then
    printf 'podbay: warning: invalid volume name "%s" in %s; skipping\n' "$name" "$file" >&2
    return 1
  fi
  return 0
}

emit_volume() {
  local name="$1" container="$2" namespace="$3" file="$4"

  if [ -z "$name" ] || [ -z "$container" ]; then
    printf 'podbay: warning: skipping incomplete volume entry (name=%s container=%s)\n' "$name" "$container" >&2
    return 0
  fi

  validate_volume_name "$name" "$file" || return 0

  local ns
  ns=$(sanitize_volume_namespace "$namespace")
  local full_name="podbay-vol-${ns}-${name}"

  if ! podman volume exists "$full_name" >/dev/null 2>&1; then
    if ! podman volume create "$full_name" >/dev/null; then
      printf 'podbay: warning: failed to create volume "%s"; skipping\n' "$full_name" >&2
      return 0
    fi
  fi

  mounts+=("$full_name:$container:rw")
  podbay_volume_paths+=("$container")
}

load_volumes() {
  local file="$1"
  local namespace="$2"
  local name="" container=""
  local line key value

  [ -f "$file" ] || return 0

  while IFS= read -r line; do
    line=${line%%\#*}
    case "$line" in
      '[[volume]]')
        if [ -n "$name" ]; then
          emit_volume "$name" "$container" "$namespace" "$file"
        fi
        name=""
        container=""
        ;;
      *=*)
        key=${line%%=*}
        value=${line#*=}
        key=$(trim "$key")
        value=$(trim "$value")
        value=${value#\"}
        value=${value%\"}
        case "$key" in
          name) name=$value ;;
          container) container=$value ;;
        esac
        ;;
    esac
  done < "$file"

  if [ -n "$name" ]; then
    emit_volume "$name" "$container" "$namespace" "$file"
  fi
}

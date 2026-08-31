#!/usr/bin/env bash

trim() {
  local s="$1"
  s=${s#"${s%%[![:space:]]*}"}
  s=${s%"${s##*[![:space:]]}"}
  printf '%s' "$s"
}

emit_mount() {
  local host="$1" container="$2" mode="$3"
  host=${host/#\~/$HOME}
  if [ "$mode" != "ro" ] && [ "$mode" != "rw" ]; then
    printf 'podbay: warning: invalid mode "%s" for %s; treating as ro\n' "$mode" "$host" >&2
    mode=ro
  fi
  if [ -z "$host" ] || [ -z "$container" ]; then
    printf 'podbay: warning: skipping incomplete mount entry (host=%s container=%s)\n' "$host" "$container" >&2
    return 0
  fi
  if [ ! -e "$host" ]; then
    printf 'podbay: warning: mount source does not exist, skipping: %s\n' "$host" >&2
    return 0
  fi
  mounts+=("$host:$container:$mode")
}

load_mounts() {
  local file="$1"
  local host="" container="" mode="ro"
  local line key value

  [ -f "$file" ] || return 0

  while IFS= read -r line; do
    line=${line%%\#*}
    case "$line" in
      '[[mount]]')
        if [ -n "$host" ]; then
          emit_mount "$host" "$container" "$mode"
        fi
        host=""
        container=""
        mode="ro"
        ;;
      *=*)
        key=${line%%=*}
        value=${line#*=}
        key=$(trim "$key")
        value=$(trim "$value")
        value=${value#\"}
        value=${value%\"}
        case "$key" in
          host) host=$value ;;
          container) container=$value ;;
          mode) mode=$value ;;
        esac
        ;;
    esac
  done < "$file"

  if [ -n "$host" ]; then
    emit_mount "$host" "$container" "$mode"
  fi
}

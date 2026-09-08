#!/usr/bin/env bash
#
# Podbay profile image extensions.
#
# A profile may optionally contain an "extension.sh" script. If present, the
# run script bakes it into a derived image (build context is the script only;
# the repo is never part of the context). The script runs as root during the
# build, so it can apt-install system libraries, etc.
#
#   ~/.config/podbay/profiles/common/extension.sh      # applied to every profile
#   ~/.config/podbay/profiles/<name>/extension.sh      # applied on top of common
#
# Derived images are tagged with a content hash so unchanged scripts reuse an
# existing build and changes to the base image or a common extension force a
# rebuild of downstream profile images.

# Return success if the file exists and contains at least one line that is not
# blank and not a shell comment.
script_has_content() {
  local file="$1"
  local line stripped

  [ -f "$file" ] || return 1

  while IFS= read -r line; do
    stripped=${line%%#*}
    stripped=${stripped//[[:space:]]/}
    [ -n "$stripped" ] && return 0
  done < "$file"

  return 1
}

# Compute an image tag for a profile extension image.
#   $1 = base image name
#   $2 = path to extension.sh
#   $3 = profile name component (e.g. "common" or "webdev")
profile_extension_tag() {
  local base_image="$1"
  local script_path="$2"
  local name="$3"
  local base_id script_hash combined_hash tag_hash

  base_id=$(podman image inspect --format '{{.Id}}' "$base_image" 2>/dev/null || true)
  if [ -z "$base_id" ]; then
    printf 'podbay: base image "%s" not found; build it first (podman build -t %s base/)\n' \
      "$base_image" "$base_image" >&2
    return 1
  fi

  script_hash=$(sha256sum "$script_path" | awk '{print $1}')
  combined_hash=$(printf '%s%s' "$base_id" "$script_hash" | sha256sum | awk '{print $1}')
  tag_hash=${combined_hash:0:16}

  printf 'podbay-profile-%s:%s\n' "$name" "$tag_hash"
}

# Build a profile extension image from a base image and an extension script.
# The script is copied into the image at a fixed path and executed as root.
build_profile_image() {
  local tag="$1"
  local base_image="$2"
  local script_path="$3"
  local tmpdir

  tmpdir=$(mktemp -d)

  cp "$script_path" "$tmpdir/extension.sh"

  cat > "$tmpdir/Containerfile" <<EOF
FROM $base_image
COPY extension.sh /usr/local/share/podbay/extension.sh
RUN chmod +x /usr/local/share/podbay/extension.sh && /usr/local/share/podbay/extension.sh
EOF

  printf 'podbay: building profile image %s\n' "$tag" >&2
  if ! podman build -t "$tag" -f "$tmpdir/Containerfile" "$tmpdir" >&2; then
    rm -rf "$tmpdir"
    return 1
  fi
  rm -rf "$tmpdir"
}

# Resolve the image to use for a given profile selection.
#   $1 = base image name
#   $2 = selected profile directory (may be empty)
#   $3 = common profile directory (may be empty)
#
# Prints the resolved image name to stdout.
resolve_profile_image() {
  local base_image="$1"
  local profile_dir="$2"
  local common_dir="$3"
  local common_script="" profile_script=""
  local common_image="$base_image" final_image="$base_image"

  if [ -n "$common_dir" ] && [ -f "$common_dir/extension.sh" ] && script_has_content "$common_dir/extension.sh"; then
    common_script="$common_dir/extension.sh"
  fi

  if [ -n "$profile_dir" ] && [ -f "$profile_dir/extension.sh" ] && script_has_content "$profile_dir/extension.sh"; then
    profile_script="$profile_dir/extension.sh"
  fi

  if [ -z "$common_script" ] && [ -z "$profile_script" ]; then
    printf '%s\n' "$base_image"
    return 0
  fi

  if [ -n "$common_script" ]; then
    local tag
    tag=$(profile_extension_tag "$base_image" "$common_script" "common") || return 1
    if ! podman image exists "$tag" >/dev/null 2>&1; then
      build_profile_image "$tag" "$base_image" "$common_script" || return 1
    fi
    common_image="$tag"
    final_image="$tag"
  fi

  if [ -n "$profile_script" ]; then
    local name tag
    name=$(basename "$profile_dir")
    tag=$(profile_extension_tag "$common_image" "$profile_script" "$name") || return 1
    if ! podman image exists "$tag" >/dev/null 2>&1; then
      build_profile_image "$tag" "$common_image" "$profile_script" || return 1
    fi
    final_image="$tag"
  fi

  printf '%s\n' "$final_image"
}

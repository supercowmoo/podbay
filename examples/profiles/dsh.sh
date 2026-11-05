#!/bin/sh
# Image layer for the dsh profile: runs as root at build time.
#
# Node and dsh go in the image, read-only in sessions. dsh is a developer
# preview, so everything is pinned; change it deliberately:
# - Node: a fixed version, checked against its published SHA-256.
# - dsh: a fixed version. Its ~170 npm dependencies resolve only to versions
#   published before `before`, so a package published later, malicious or
#   not, is never picked up. npm checks every package's integrity hash.
# - No install scripts run (--ignore-scripts). On Linux they only pick
#   prebuilt binaries the packages already ship; installing without them
#   gives the same files.
set -eu

node=v24.22.2
dsh=0.2.1-rc.5
before=2026-11-02T20:00:00Z
case "$(uname -m)" in
  x86_64) arch=x64 sum=48cc0e21e048790e54c0cf5749c2725005313d652651c40b7a21feb9697aaf75 ;;
  aarch64) arch=arm64 sum=eb187837a2230d4047161ac0be008584e16b46f726e9c9490ee35dc6b17d9ba8 ;;
  *) echo "no Node build for $(uname -m)" >&2; exit 1 ;;
esac
curl -fsSL -o /tmp/node.tar.xz "https://nodejs.org/dist/$node/node-$node-linux-$arch.tar.xz"
echo "$sum  /tmp/node.tar.xz" | sha256sum -c -
mkdir -p /opt/dsh/node
# As root, tar would otherwise keep the owner recorded in the archive.
tar -xJf /tmp/node.tar.xz --no-same-owner --strip-components=1 -C /opt/dsh/node
rm -f /tmp/node.tar.xz
export PATH="/opt/dsh/node/bin:$PATH"
npm_config_cache=/tmp/npm-cache npm install -g --ignore-scripts --before="$before" --prefix /opt/dsh/dsh "@deepseek-ai/dsh@$dsh"
rm -rf /tmp/npm-cache
chmod -R a+rX /opt/dsh

#!/bin/sh
# Image layer for the rust profile: runs as root at build time.
#
# rustup and its cargo and rustc stand-ins go in the image, read-only in
# sessions. A pinned version, checked against its published SHA-256.
set -eu

rustup=1.29.2
case "$(uname -m)" in
  x86_64) target=x86_64-unknown-linux-gnu sum=5be191e8c70a5007074254cb74d78dbfdd25cd5a61c27de7b8e5410122ec24f4 ;;
  aarch64) target=aarch64-unknown-linux-gnu sum=a9c168dbea81a84671c6b7a5c1795cb0eb699020110d107ddfca427bd797e697 ;;
  *) echo "no rustup build for $(uname -m)" >&2; exit 1 ;;
esac
curl -fsSL -o /tmp/rustup-init "https://static.rust-lang.org/rustup/archive/$rustup/$target/rustup-init"
echo "$sum  /tmp/rustup-init" | sha256sum -c -
chmod +x /tmp/rustup-init
CARGO_HOME=/opt/rustup RUSTUP_HOME=/tmp/rustup-home /tmp/rustup-init -y --no-modify-path --default-toolchain none
rm -rf /tmp/rustup-init /tmp/rustup-home
chmod -R a+rX /opt/rustup

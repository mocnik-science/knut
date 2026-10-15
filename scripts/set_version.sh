#!/bin/sh
# Sets the version of all crates (and of the Python package, which takes it from Cargo.toml).
# Usage: scripts/set_version.sh 2026.11.0
set -e
cd "$(dirname "$0")/.."
new="$1"
echo "$new" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || { echo "usage: $0 X.Y.Z" >&2; exit 1; }
old=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
# the workspace version and the version requirements of the internal dependencies
sed -i.bak -e "s/^version = \"$old\"$/version = \"$new\"/" \
  -e "s/^\(knut[a-z-]* = { path = \"crates\/[a-z-]*\", version = \)\"[^\"]*\"/\1\"$new\"/" Cargo.toml
rm Cargo.toml.bak
cargo update --workspace --quiet
echo "$old -> $new"

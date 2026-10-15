#!/bin/sh
# Raises the version of all crates (and of the Python package, which takes it from Cargo.toml),
# commits it, tags it and pushes both.  Versions are YEAR.MONTH.N, with N counting the releases
# of the month from 1.
# Usage: scripts/set_version.sh [X.Y.Z]
set -e
cd "$(dirname "$0")/.."
old=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
month="$(date +%Y).$(expr "$(date +%m)" + 0)"
case "$old" in
  "$month".*) new="$month.$(expr "${old##*.}" + 1)" ;;
  *) new="$month.1" ;;
esac
new="${1:-$new}"
echo "$new" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || { echo "usage: $0 [X.Y.Z]" >&2; exit 1; }
git diff --quiet HEAD -- Cargo.toml Cargo.lock || { echo "Cargo.toml or Cargo.lock has uncommitted changes" >&2; exit 1; }
git rev-parse -q --verify "refs/tags/v$new" >/dev/null && { echo "tag v$new exists" >&2; exit 1; }
branch=$(git symbolic-ref --short HEAD)
remote=$(git config "branch.$branch.remote")
# the workspace version and the version requirements of the internal dependencies
sed -i.bak -e "s/^version = \"$old\"$/version = \"$new\"/" \
  -e "s/^\(knut[a-z-]* = { path = \"crates\/[a-z-]*\", version = \)\"[^\"]*\"/\1\"$new\"/" Cargo.toml
rm Cargo.toml.bak
cargo update --workspace --quiet
git commit --quiet -m "v$new" -- Cargo.toml Cargo.lock
git tag -a "v$new" -m "v$new"
git push --quiet --atomic "$remote" "$branch" "refs/tags/v$new"
echo "$old -> $new, pushed to $remote/$branch"

#!/bin/sh
# Publishes all crates on crates.io and the Python package on PyPI (wheels for macOS and Linux,
# plus the source distribution for all other platforms).
# Needs a crates.io login (`cargo login`) and a PyPI token in UV_PUBLISH_TOKEN.
# Usage: scripts/publish.sh
set -e
cd "$(dirname "$0")/.."
dist=target/pypi
rm -rf "$dist"
# build everything first, so that nothing is published if a build fails
rustup --quiet target add aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu
(
  cd crates/knut-py
  uvx maturin sdist -o "../../$dist"
  uvx maturin build --release --target universal2-apple-darwin -o "../../$dist"
  for t in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
    uvx --with ziglang maturin build --release --zig --target "$t" --compatibility manylinux2014 -o "../../$dist"
  done
)
# versions that are already published are skipped, so that the script can simply be run again
status=0
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
crates=$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(*[p["name"] for p in json.load(sys.stdin)["packages"] if p["publish"] != []])')
exclude=""
for c in $crates; do
  if [ "$(curl -s -o /dev/null -w '%{http_code}' -A 'knut publish script' "https://crates.io/api/v1/crates/$c/$version")" = 200 ]; then
    echo "$c $version is already on crates.io"
    exclude="$exclude --exclude $c"
  fi
done
if [ "$(echo $exclude | wc -w)" -lt "$(expr 2 \* $(echo $crates | wc -w))" ]; then
  cargo publish --workspace $exclude || status=1
fi
uv publish --check-url https://pypi.org/simple/ "$dist"/* || status=1
exit $status

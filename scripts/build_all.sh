#!/bin/sh
# Builds all downloaded history extracts into data/knut/ and verifies them against the PBF.
set -e
cd "$(dirname "$0")/.."
mkdir -p data/knut/logs
for r in bremen sweden austria germany; do
  pbf="data/dump/$r-internal.osh.pbf"
  [ -f "$pbf" ] || continue
  echo "== $r build $(date +%T)"
  /usr/bin/time -l ./target/release/knut build "$pbf" -o "data/knut/$r.knut" > "data/knut/logs/$r-build.log" 2>&1
  grep -E "^output|ratio|peak footprint [0-9]" "data/knut/logs/$r-build.log"
  echo "== $r verify $(date +%T)"
  ./target/release/knut verify "data/knut/$r.knut" --deep --against "$pbf" --exact 300 > "data/knut/logs/$r-verify.log" 2>&1
  tail -2 "data/knut/logs/$r-verify.log"
done
echo "== done $(date +%T)"

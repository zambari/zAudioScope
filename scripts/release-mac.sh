#!/usr/bin/env bash
# Build the macOS binary and publish it as a GitHub release.
# Publishing the release triggers .github/workflows/release-windows.yml,
# which builds the Windows zip and attaches it to the same release.
#
# Usage: scripts/release-mac.sh v0.1.0
set -euo pipefail

tag="${1:?usage: $0 <tag, e.g. v0.1.0>}"
cd "$(dirname "$0")/.."

if [ -n "$(git status --porcelain)" ]; then
  echo "Working tree is dirty; commit first." >&2
  exit 1
fi
git push origin HEAD

cargo build --release
arch="$(uname -m)"
zip="target/zAudioScope-${tag}-macos-${arch}.zip"
rm -f "$zip"
zip -j "$zip" target/release/zaudioscope LICENSE README.md

gh release create "$tag" "$zip" --target "$(git rev-parse HEAD)" --title "$tag" --generate-notes

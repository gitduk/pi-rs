#!/usr/bin/env bash
set -euo pipefail

# Local release: build the current version and publish it when it is not yet
# tagged on origin. Driven by the post-push hook; safe to run by hand too.
cd "$(git rev-parse --show-toplevel)"

TARGET="${TARGET:-x86_64-unknown-linux-gnu}"

# Anchored to the section: a workspace root's first `version = ` is only the
# package's by accident, and a dependency pinned above it would silently
# become the release number.
VERSION="v$(awk '
  /^\[workspace\.package\]/ { in_pkg = 1; next }
  /^\[/                     { in_pkg = 0 }
  in_pkg && /^version = /   { gsub(/version = "|"/, ""); print; exit }
' Cargo.toml)"

if [ "$VERSION" = "v" ]; then
  echo "error: no version under [workspace.package] in Cargo.toml" >&2
  exit 1
fi

# Nothing to do on a routine push: the version has not changed.
if git ls-remote --tags origin "refs/tags/$VERSION" | grep -q .; then
  echo "[release] $VERSION is already tagged on origin; nothing to publish"
  exit 0
fi

# The tag points at HEAD, so a dirty tree would publish a binary built from
# code the tag does not contain.
if [ -n "$(git status --porcelain)" ]; then
  echo "error: working tree is dirty; commit or stash before releasing $VERSION" >&2
  exit 1
fi

cargo test --locked --release --target "$TARGET"
cargo build --locked --release --target "$TARGET"

BIN="pi-$TARGET"
cp "target/$TARGET/release/pi" "$BIN"

# One call creates the tag, pushes it, and uploads the binary. Doing the tag
# here, after a successful build, keeps a failed build from leaving an orphan
# tag that would block the next push.
gh release create "$VERSION" "$BIN" --generate-notes --target "$(git rev-parse HEAD)"
echo "[release] published $VERSION with $BIN"

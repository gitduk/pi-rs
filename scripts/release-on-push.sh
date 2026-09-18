#!/usr/bin/env bash
set -euo pipefail

# Local release: build the current version and publish it when it is not yet
# tagged on origin. Run by hand after a push — git has no post-push hook for
# this to ride on.
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
AT_START="$(git rev-parse HEAD)"

# Nothing to do on a routine push: the version has not changed. Captured
# rather than piped into `grep -q`: grep quits on the first line, the writer
# eats a SIGPIPE, and pipefail would turn an existing tag into "untagged".
if [ -n "$(git ls-remote --tags origin "refs/tags/$VERSION")" ]; then
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

# The build took minutes; if a commit landed on HEAD meanwhile, the binary is
# not the code the tag would name.
if [ "$(git rev-parse HEAD)" != "$AT_START" ]; then
  echo "error: HEAD moved while building; rerun on the commit you meant to release" >&2
  exit 1
fi

# Inside target/, which git ignores: the old repo-root copy dirtied the tree
# and the dirty check above then blocked the very next run.
BIN="target/pi-$TARGET"
cp "target/$TARGET/release/pi" "$BIN"

# One call creates the tag, pushes it, and uploads the binary. Doing the tag
# here, after a successful build, keeps a failed build from leaving an orphan
# tag that would block the next push.
gh release create "$VERSION" "$BIN" --generate-notes --target "$AT_START"
echo "[release] published $VERSION with $BIN"

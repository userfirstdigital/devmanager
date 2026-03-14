#!/usr/bin/env bash
# Usage: npm run release [-- patch|minor|major|x.y.z]
#
# Defaults to "patch" if no argument given.
#   patch: 0.1.0 → 0.1.1
#   minor: 0.1.0 → 0.2.0
#   major: 0.1.0 → 1.0.0
#   x.y.z: explicit version

set -euo pipefail

# Read current version from package.json
CURRENT=$(node -p "require('./package.json').version")
BUMP="${1:-patch}"

# Calculate new version
if [[ "$BUMP" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  VERSION="$BUMP"
else
  IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT"
  case "$BUMP" in
    patch) PATCH=$((PATCH + 1)) ;;
    minor) MINOR=$((MINOR + 1)); PATCH=0 ;;
    major) MAJOR=$((MAJOR + 1)); MINOR=0; PATCH=0 ;;
    *) echo "Invalid argument: $BUMP (use patch, minor, major, or x.y.z)"; exit 1 ;;
  esac
  VERSION="${MAJOR}.${MINOR}.${PATCH}"
fi

echo "Releasing: v${CURRENT} → v${VERSION}"
echo ""

# Bump version in all config files
sed -i "s/\"version\": \"${CURRENT}\"/\"version\": \"${VERSION}\"/" package.json
sed -i "s/\"version\": \"${CURRENT}\"/\"version\": \"${VERSION}\"/" src-tauri/tauri.conf.json
sed -i "s/^version = \"${CURRENT}\"/version = \"${VERSION}\"/" src-tauri/Cargo.toml

# Update Cargo.lock
(cd src-tauri && cargo generate-lockfile 2>/dev/null || true)

# Get current branch
BRANCH=$(git rev-parse --abbrev-ref HEAD)

# Commit, tag, push
git add package.json src-tauri/tauri.conf.json src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "release: v${VERSION}"
git tag "v${VERSION}"
git push origin "${BRANCH}" --tags

echo ""
echo "Pushed v${VERSION} — GitHub Actions will build, sign, and publish the release."

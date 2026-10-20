#!/usr/bin/env bash
# Builds, signs and checks target/release-files/v<version>/, the files of a GitHub release.
# Usage: scripts/release-macos.sh <version> [--publish]
# --publish also tags v<version>, pushes the tag and publishes the GitHub release.
# The update key is $REQUIEM_UPDATE_KEY, or ~/.config/requiem/update-key.
set -euo pipefail

cd "$(dirname "$0")/.."

if [ "$(uname -s)" != "Darwin" ]; then
  echo "release-macos.sh: macOS only" >&2
  exit 1
fi

CARGO_BIN="${CARGO:-cargo}"

version=""
publish=0
for arg in "$@"; do
  case "$arg" in
    --publish) publish=1 ;;
    -*) echo "usage: $0 <version> [--publish]" >&2; exit 2 ;;
    *)
      [ -z "$version" ] || { echo "usage: $0 <version> [--publish]" >&2; exit 2; }
      version="$arg"
      ;;
  esac
done
[ -n "$version" ] || { echo "usage: $0 <version> [--publish]" >&2; exit 2; }

die() {
  echo "release-macos.sh: $*" >&2
  exit 1
}

workspace_version="$("$CARGO_BIN" metadata --no-deps --format-version 1 2>/dev/null | python3 -I -c '
import json, sys
for p in json.load(sys.stdin)["packages"]:
    if p["name"] == "requiem-gui":
        print(p["version"])
        break
' 2>/dev/null || true)"
if [ -z "$workspace_version" ]; then
  workspace_version="$(awk '/^\[workspace.package\]/ {on=1; next} /^\[/ {on=0} on && $1 == "version" {gsub(/"/, "", $3); print $3; exit}' Cargo.toml)"
fi
[ -n "$workspace_version" ] || die "cannot determine the workspace version"
[ "$version" = "$workspace_version" ] || die "$version is not the workspace version, $workspace_version"

[ -z "$(git status --porcelain)" ] || die "the working tree is not clean"

tag="v$version"
if [ "$publish" -eq 1 ]; then
  [ "$(git branch --show-current)" = main ] || die "--publish needs the main branch"
  git fetch --quiet origin main
  [ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || die "HEAD is not origin/main; push main first"
  ! git rev-parse --quiet --verify "refs/tags/$tag" >/dev/null || die "tag $tag exists locally"
  [ -z "$(git ls-remote --tags origin "refs/tags/$tag")" ] || die "tag $tag exists on origin"
  gh auth status >/dev/null 2>&1 || die "gh is not logged in"
fi

scripts/make-dmg.sh >/dev/null
"$CARGO_BIN" build --release --locked -p requiem-update --bin requiem-release
release_tool="${CARGO_TARGET_DIR:-target}/release/requiem-release"
[ -x "$release_tool" ] || die "$release_tool is missing"

app="target/Requiem.app"
dmg_name="Requiem-$version.dmg"
zip_name="Requiem-$version.zip"
dir="target/release-files/$tag"
rm -rf "$dir"
mkdir -p "$dir"
cp "target/$dmg_name" "$dir/$dmg_name"
/usr/bin/ditto -c -k --keepParent "$app" "$dir/$zip_name"

mnt="$(mktemp -d -t requiem-release-dmg)"
unz="$(mktemp -d -t requiem-release-zip)"
attached=0
cleanup() {
  if [ "$attached" -eq 1 ]; then
    hdiutil detach -quiet "$mnt" || hdiutil detach -quiet -force "$mnt" || true
  fi
  rmdir "$mnt" 2>/dev/null || true
  rm -rf "$unz"
}
trap cleanup EXIT

hdiutil attach -nobrowse -noautoopen -readonly -mountpoint "$mnt" "$dir/$dmg_name" >/dev/null ||
  die "cannot attach $dir/$dmg_name"
attached=1
/usr/bin/ditto -x -k "$dir/$zip_name" "$unz"
diff -r "$mnt/Requiem.app" "$unz/Requiem.app" >&2 || die "the app in the disk image differs from the app in the zip"
hdiutil detach -quiet "$mnt" || die "cannot detach it from $mnt"
attached=0

min_macos="$(/usr/libexec/PlistBuddy -c 'Print :LSMinimumSystemVersion' "$app/Contents/Info.plist")"
"$release_tool" manifest --zip "$dir/$zip_name" --version "$version" --min-macos "$min_macos"
"$release_tool" sign --key "${REQUIEM_UPDATE_KEY:-$HOME/.config/requiem/update-key}" --in "$dir/requiem-update.json"
"$release_tool" verify --dir "$dir"

(cd "$dir" && shasum -a 256 "$zip_name" "$dmg_name" >SHA256SUMS)

echo "$dir"
ls -l "$dir"

[ "$publish" -eq 1 ] || exit 0

git tag -a "$tag" -m "Requiem $version"
# Through gh's credentials, which the steps below need anyway: a non-interactive shell can't
# answer git's own credential prompt.
git -c credential.helper= -c 'credential.helper=!gh auth git-credential' push origin "$tag"

notes="Download $dmg_name, open it and drag Requiem onto Applications. The first time, macOS blocks it because it isn't notarized: click Done, then Open Anyway in System Settings → Privacy & Security.

Requiem then updates itself from GitHub releases; SHA256SUMS lists the zip's and the disk image's SHA-256."
gh release create "$tag" --draft --verify-tag --title "Requiem $version" --notes "$notes"
gh release upload "$tag" "$dir/$dmg_name" "$dir/$zip_name" "$dir/SHA256SUMS"
# The manifest goes last: a release stays a draft until every file is on it, and releases/latest never points at a draft.
gh release upload "$tag" "$dir/requiem-update.json" "$dir/requiem-update.json.sig"
gh release edit "$tag" --draft=false --latest
gh release view "$tag" --json url -q .url

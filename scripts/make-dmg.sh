#!/usr/bin/env bash
# Builds target/Requiem-<version>.dmg, a drag-to-Applications installer for target/Requiem.app.
# Usage: scripts/make-dmg.sh [--no-build]
# --no-build packages the existing target/Requiem.app instead of running scripts/bundle-macos.sh.
set -euo pipefail

cd "$(dirname "$0")/.."

if [ "$(uname -s)" != "Darwin" ]; then
  echo "make-dmg.sh: macOS only" >&2
  exit 1
fi

CARGO_BIN="${CARGO:-cargo}"

build=1
case "${1:-}" in
  "") ;;
  --no-build) build=0 ;;
  *) echo "usage: $0 [--no-build]" >&2; exit 2 ;;
esac

app="target/Requiem.app"
if [ "$build" -eq 1 ]; then
  scripts/bundle-macos.sh >/dev/null
fi
if [ ! -d "$app" ]; then
  echo "make-dmg.sh: $app is missing; run scripts/bundle-macos.sh or drop --no-build" >&2
  exit 1
fi

version="$("$CARGO_BIN" metadata --no-deps --format-version 1 2>/dev/null | python3 -I -c '
import json, sys
for p in json.load(sys.stdin)["packages"]:
    if p["name"] == "requiem-gui":
        print(p["version"])
        break
' 2>/dev/null || true)"
if [ -z "$version" ]; then
  version="$(awk '/^\[workspace.package\]/ {on=1; next} /^\[/ {on=0} on && $1 == "version" {gsub(/"/, "", $3); print $3; exit}' Cargo.toml)"
fi
[ -n "$version" ] || { echo "make-dmg.sh: cannot determine the version" >&2; exit 1; }

venv="target/dmgbuild-venv"
if [ ! -x "$venv/bin/dmgbuild" ]; then
  rm -rf "$venv"
  python3 -m venv "$venv"
  # dmgbuild's dependencies, pinned at their releases current on 2026-10-14: ds_store 1.3.3
  # and mac_alias 2.2.3.
  "$venv/bin/pip" install --quiet --disable-pip-version-check \
    dmgbuild==1.6.7 ds_store==1.3.3 mac_alias==2.2.3
fi

art="target/dmg"
rm -rf "$art"
mkdir -p "$art"
swift scripts/make-dmg-background.swift "$art" >/dev/null

dmg="target/Requiem-$version.dmg"
"$venv/bin/dmgbuild" -s scripts/dmg-settings.py \
  -D app="$app" -D icon=crates/requiem-gui/assets/AppIcon.icns -D background="$art/background.tiff" \
  Requiem "$dmg"

mnt="$(mktemp -d -t requiem-dmg)"
attached=0
cleanup() {
  if [ "$attached" -eq 1 ]; then
    hdiutil detach -quiet "$mnt" || hdiutil detach -quiet -force "$mnt" || true
  fi
  rmdir "$mnt" 2>/dev/null || true
}
trap cleanup EXIT

fail() {
  echo "make-dmg.sh: $dmg: $*" >&2
  exit 1
}

hdiutil attach -nobrowse -noautoopen -readonly -mountpoint "$mnt" "$dmg" >/dev/null || fail "cannot attach it"
attached=1

# ls leaves out the dot files Finder hides too.
entries="$(ls "$mnt" | tr '\n' ' ')"
[ "$entries" = "Applications Requiem.app " ] || fail "expected only Applications and Requiem.app, found: $entries"
{ [ -d "$mnt/Requiem.app" ] && [ ! -L "$mnt/Requiem.app" ]; } || fail "Requiem.app is not a directory"
{ [ -L "$mnt/Applications" ] && [ "$(readlink "$mnt/Applications")" = /Applications ]; } ||
  fail "Applications is not a link to /Applications"
codesign --verify --strict --deep "$mnt/Requiem.app" || fail "Requiem.app's signature does not verify"
diff -r "$app" "$mnt/Requiem.app" >&2 || fail "Requiem.app differs from $app"
# dmgbuild puts the background at the volume root, not in .background/.
[ -f "$mnt/.background.tiff" ] || fail "the background image is missing"
[ -f "$mnt/.DS_Store" ] || fail ".DS_Store is missing"

hdiutil detach -quiet "$mnt" || fail "cannot detach it from $mnt"
attached=0

echo "$dmg"

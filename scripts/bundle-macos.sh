#!/usr/bin/env bash
# Assembles target/Requiem.app from the release requiemd, requiem-gui and requiem (CLI) binaries.
# Usage: scripts/bundle-macos.sh [--no-build]
set -euo pipefail

cd "$(dirname "$0")/.."

if [ "$(uname -s)" != "Darwin" ]; then
  echo "bundle-macos.sh: macOS only" >&2
  exit 1
fi

CARGO_BIN="${CARGO:-cargo}"
BASE_MIN_OS="14.0"

build=1
case "${1:-}" in
  "") ;;
  --no-build) build=0 ;;
  *) echo "usage: $0 [--no-build]" >&2; exit 2 ;;
esac

if [ "$build" -eq 1 ]; then
  MACOSX_DEPLOYMENT_TARGET="$BASE_MIN_OS" "$CARGO_BIN" build --release -p requiem-daemon -p requiem-gui -p requiem-cli
fi

for bin in requiemd requiem-gui requiem; do
  if [ ! -x "target/release/$bin" ]; then
    echo "bundle-macos.sh: target/release/$bin is missing; build it or drop --no-build" >&2
    exit 1
  fi
done

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
[ -n "$version" ] || { echo "bundle-macos.sh: cannot determine the version" >&2; exit 1; }

app="target/Requiem.app"
rm -rf "$app"
# requiemd is a helper app with its own bundle id: inside the main app, LaunchServices
# would take the running daemon for the app itself and never start the GUI.
helper="$app/Contents/Helpers/requiemd.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Frameworks" "$app/Contents/Resources/licenses" \
  "$helper/Contents/MacOS"
cp target/release/requiem-gui target/release/requiem "$app/Contents/MacOS/"
cp target/release/requiemd "$helper/Contents/MacOS/"
cp crates/requiem-gui/assets/AppIcon.icns "$app/Contents/Resources/"
frameworks="$app/Contents/Frameworks"
licenses="$app/Contents/Resources/licenses"

# Non-system, absolute-path dependencies of a Mach-O file. @-relative ones are already ours.
deps() {
  otool -L "$1" | tail -n +2 | awk '{print $1}' | grep -v -E '^(/usr/lib/|/System/|@)' || true
}

copy_license() {
  local dylib="$1" name="$2" keg found=0 f
  keg="$(dirname "$(dirname "$(python3 -I -c 'import os, sys; print(os.path.realpath(sys.argv[1]))' "$dylib")")")"
  for f in "$keg"/COPYING* "$keg"/LICENSE*; do
    if [ -f "$f" ]; then
      cp "$f" "$licenses/$name-$(basename "$f")"
      found=1
    fi
  done
  [ "$found" -eq 1 ] || echo "warning: no license file found next to $dylib (looked in $keg)" >&2
}

# `ref` is how `target` reaches Contents/Frameworks.
embed() {
  local target="$1" ref="$2" dep name
  while IFS= read -r dep; do
    [ -n "$dep" ] || continue
    name="$(basename "$dep")"
    if [ ! -e "$frameworks/$name" ]; then
      cp -L "$dep" "$frameworks/$name"
      chmod u+w "$frameworks/$name"
      install_name_tool -id "@executable_path/../Frameworks/$name" "$frameworks/$name"
      copy_license "$dep" "$name"
      embed "$frameworks/$name" "@loader_path"
    fi
    install_name_tool -change "$dep" "$ref/$name" "$target"
  done < <(deps "$target")
}

embed "$helper/Contents/MacOS/requiemd" "@executable_path/../../../../Frameworks"
embed "$app/Contents/MacOS/requiem-gui" "@executable_path/../Frameworks"
embed "$app/Contents/MacOS/requiem" "@executable_path/../Frameworks"

# Every remaining dependency must be a system library or inside the bundle.
for f in "$app"/Contents/MacOS/* "$helper"/Contents/MacOS/* "$frameworks"/*; do
  [ -e "$f" ] || continue
  bad="$(otool -L "$f" | tail -n +2 | awk '{print $1}' | grep -v -E '^(/usr/lib/|/System/|@executable_path/\.\./Frameworks/|@executable_path/\.\./\.\./\.\./\.\./Frameworks/|@loader_path/)' || true)"
  if [ -n "$bad" ]; then
    echo "bundle-macos.sh: $f still references: $bad" >&2
    exit 1
  fi
done

# The bundle runs on the newest of the base target and what its dylibs were built for.
min_os="$BASE_MIN_OS"
for d in "$frameworks"/*.dylib; do
  [ -e "$d" ] || continue
  m="$(vtool -show-build "$d" | awk '$1 == "minos" {print $2; exit}')"
  [ -n "$m" ] || continue
  higher="$(python3 -I -c '
import sys
key = lambda v: [int(x) for x in v.split(".")]
print(sys.argv[2] if key(sys.argv[2]) > key(sys.argv[1]) else sys.argv[1])
' "$min_os" "$m")"
  if [ "$higher" != "$min_os" ]; then
    min_os="$higher"
    min_os_dylib="$(basename "$d")"
  fi
done
if [ "$min_os" != "$BASE_MIN_OS" ]; then
  echo "LSMinimumSystemVersion is $min_os, not $BASE_MIN_OS: $min_os_dylib targets macOS $min_os; a lower target needs it rebuilt with that deployment target." >&2
fi

cat >"$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>dev.blutarche.requiem</string>
  <key>CFBundleName</key><string>Requiem</string>
  <key>CFBundleDisplayName</key><string>Requiem</string>
  <key>CFBundleExecutable</key><string>requiem-gui</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>$min_os</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.music</string>
</dict>
</plist>
PLIST
cat >"$helper/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>dev.blutarche.requiem.player</string>
  <key>CFBundleName</key><string>Requiem</string>
  <key>CFBundleExecutable</key><string>requiemd</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>$min_os</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
plutil -lint "$app/Contents/Info.plist" "$helper/Contents/Info.plist" >/dev/null

cp LICENSE-MIT "$licenses/requiem-LICENSE-MIT"
cp LICENSE-APACHE "$licenses/requiem-LICENSE-APACHE"
while IFS= read -r f; do
  rel="${f#crates/}"
  cp "$f" "$licenses/$(echo "$rel" | tr '/' '_')"
done < <(find crates -path '*/assets/*' -type f \( -iname 'LICENSE*' -o -iname 'COPYING*' -o -iname 'OFL*' -o -iname 'NOTICE*' \) | sort)

for d in "$frameworks"/*.dylib; do
  [ -e "$d" ] || continue
  codesign --force --sign - --timestamp=none "$d"
done
codesign --force --sign - --timestamp=none "$helper"
codesign --force --sign - --timestamp=none "$app/Contents/MacOS/requiem"
codesign --force --sign - --timestamp=none "$app"
codesign --verify --deep --strict "$app"

echo "$app"

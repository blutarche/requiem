#!/usr/bin/env bash
# Builds and smoke-tests the Linux tarballs of target/release-files/v<version>/, in Docker.
# Usage: scripts/release-linux.sh <version>
# Needs Docker. Builds in rust:<rust-toolchain.toml channel>-bookworm (glibc 2.36); aarch64
# natively, x86_64 by cross-compiling from the same container when Docker runs linux/arm64.
# Adds requiem-<version>-linux-<arch>.tar.gz to the directory release-macos.sh writes, and
# replaces the Linux lines of its SHA256SUMS. Run it after release-macos.sh, which wipes the
# directory. It never publishes.
set -euo pipefail

cd "$(dirname "$0")/.."

die() {
  echo "release-linux.sh: $*" >&2
  exit 1
}

[ "$#" -eq 1 ] && [ "${1#-}" = "$1" ] || { echo "usage: $0 <version>" >&2; exit 2; }
version="$1"

CARGO_BIN="${CARGO:-cargo}"
workspace_version="$("$CARGO_BIN" metadata --no-deps --format-version 1 2>/dev/null | python3 -I -c '
import json, sys
for p in json.load(sys.stdin)["packages"]:
    if p["name"] == "requiem-daemon":
        print(p["version"])
        break
' 2>/dev/null || true)"
if [ -z "$workspace_version" ]; then
  workspace_version="$(awk '/^\[workspace.package\]/ {on=1; next} /^\[/ {on=0} on && $1 == "version" {gsub(/"/, "", $3); print $3; exit}' Cargo.toml)"
fi
[ -n "$workspace_version" ] || die "cannot determine the workspace version"
[ "$version" = "$workspace_version" ] || die "$version is not the workspace version, $workspace_version"

command -v docker >/dev/null || die "docker is not installed"
docker info >/dev/null 2>&1 || die "the docker daemon is not running"

toolchain="$(awk -F'"' '/^channel *=/ {print $2; exit}' rust-toolchain.toml)"
[ -n "$toolchain" ] || die "cannot read the channel from rust-toolchain.toml"

host_arch="$(docker info --format '{{.Architecture}}')"
case "$host_arch" in
  aarch64 | arm64) host_arch=aarch64 build_platform=linux/arm64 ;;
  x86_64 | amd64) host_arch=x86_64 build_platform=linux/amd64 ;;
  *) die "unsupported docker architecture $host_arch" ;;
esac

tag="v$version"
dir="target/release-files/$tag"
mkdir -p "$dir"

smoke='
set -eu
tarball="$1"; name="$2"; version="$3"
apt-get update -qq
apt-get install -y -qq --no-install-recommends libasound2 libgtk-4-1 ca-certificates dbus playerctl xvfb xauth fonts-dejavu-core >/dev/null
DBUS_SESSION_BUS_ADDRESS="$(dbus-daemon --session --fork --print-address)"
export DBUS_SESSION_BUS_ADDRESS
mkdir /opt/requiem
tar -xzf "/dist/$tarball" -C /opt/requiem
cd "/opt/requiem/$name"
ls
missing="$(ldd requiem requiemd requiem-gui | grep "not found" || true)"
[ -z "$missing" ] || { echo "missing libraries:" >&2; echo "$missing" >&2; exit 1; }
./requiem --help
[ "$(./requiem --version)" = "requiem $version" ]
# The window draws its demo library headless, without a player.
GSK_RENDERER=cairo GTK_A11Y=none timeout 120 xvfb-run -a ./requiem-gui --demo --snapshot /tmp/window.png
[ -s /tmp/window.png ] || { echo "the window drew nothing" >&2; exit 1; }
for s in 16 32 48 64 128 256 512; do
  [ -s "icons/hicolor/${s}x${s}/apps/io.github.blutarche.Requiem.png" ] || { echo "no ${s}px icon" >&2; exit 1; }
done
grep -qx "Exec=requiem-gui" io.github.blutarche.Requiem.desktop
out="$(printf "search daft punk\nawait results 30\n" | ./requiem --connect --script - 2>&1)" || {
  echo "$out" >&2
  echo "the search script failed" >&2
  exit 1
}
echo "$out"
grep -Eq "^ +1\. " <<<"$out" || { echo "no search results printed" >&2; exit 1; }
grep -lx requiemd /proc/[0-9]*/comm >/dev/null || { echo "requiemd is not running" >&2; exit 1; }
[ "$(playerctl -p requiem status)" = "Stopped" ]
printf "shutdown-daemon\n" | ./requiem --connect --script - >/dev/null
echo "smoke test passed"
'

built=()
for arch in x86_64 aarch64; do
  triple="$arch-unknown-linux-gnu"
  case "$arch" in
    x86_64) platform=linux/amd64 dpkg_arch=amd64 ;;
    aarch64) platform=linux/arm64 dpkg_arch=arm64 ;;
  esac
  name="requiem-$version-linux-$arch"
  tarball="$name.tar.gz"
  out="target/linux/$arch"
  image="requiem-linux-build:$toolchain-$arch"

  if [ "$arch" = "$host_arch" ]; then
    deps="pkg-config libasound2-dev libopus-dev libgtk-4-dev"
    env_lines=""
    stub=""
    method="native"
  else
    [ "$host_arch" = aarch64 ] || die "cross-compiling $arch from $host_arch is not supported"
    deps="pkg-config gcc-x86-64-linux-gnu libc6-dev-amd64-cross libasound2-dev:$dpkg_arch libopus-dev:$dpkg_arch libgtk-4-dev:$dpkg_arch"
    env_lines="ENV CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc \\
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \\
    PKG_CONFIG_ALLOW_CROSS=1 \\
    PKG_CONFIG_LIBDIR=/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig"
    # libgraphene-1.0-dev pulls in GObject introspection, whose amd64 build needs an amd64
    # Python beside the native one. Linking needs none of it, so an empty package stands in.
    stub="&& mkdir -p /tmp/gir/DEBIAN \\
    && printf 'Package: libgirepository1.0-dev\\nVersion: 1.74.0-3+stub\\nArchitecture: $dpkg_arch\\nMaintainer: requiem\\nDescription: stand-in for linking only\\n' >/tmp/gir/DEBIAN/control \\
    && dpkg-deb --build /tmp/gir /tmp/gir.deb >/dev/null && dpkg -i /tmp/gir.deb >/dev/null"
    method="cross-compiled from $host_arch"
  fi

  echo "== $arch ($method)" >&2
  docker build --quiet --platform "$build_platform" -t "$image" - >/dev/null <<EOF
FROM rust:$toolchain-bookworm
RUN dpkg --add-architecture $dpkg_arch \\
    && apt-get update -qq $stub \\
    && apt-get install -y -qq --no-install-recommends $deps \\
    && rustup target add --toolchain $toolchain $triple \\
    && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_TOOLCHAIN=$toolchain CARGO_TARGET_DIR=/target \\
    OPUS_LIB_STATIC=true OPUS_LIB_DIR=/usr/lib/$arch-linux-gnu
$env_lines
EOF

  rm -rf "$out"
  mkdir -p "$out"
  docker run --rm --platform "$build_platform" \
    -v "$PWD:/src:ro" -v "$PWD/$out:/out" \
    -v requiem-linux-cargo-registry:/usr/local/cargo/registry \
    -v requiem-linux-cargo-git:/usr/local/cargo/git \
    -v "requiem-linux-target-$arch:/target" \
    -w /src "$image" bash -euc '
      cargo build --release --locked --target "$1" -p requiem-cli -p requiem-daemon -p requiem-gui
      stage="/out/$2"
      mkdir -p "$stage"
      install -m 755 "/target/$1/release/requiem" "/target/$1/release/requiemd" \
        "/target/$1/release/requiem-gui" "$stage"
      install -m 644 README.md LICENSE-MIT LICENSE-APACHE \
        crates/requiem-gui/assets/linux/io.github.blutarche.Requiem.desktop "$stage"
      for s in 16 32 48 64 128 256 512; do
        install -D -m 644 "crates/requiem-gui/assets/linux/io.github.blutarche.Requiem-$s.png" \
          "$stage/icons/hicolor/${s}x${s}/apps/io.github.blutarche.Requiem.png"
      done
      tar --owner=0 --group=0 --numeric-owner --sort=name -czf "/out/$3" -C /out "$2"
    ' build "$triple" "$name" "$tarball" >&2

  [ -s "$out/$tarball" ] || die "$out/$tarball is missing"

  docker run --rm --platform "$platform" -v "$PWD/$out:/dist:ro" debian:bookworm-slim \
    bash -c "$smoke" smoke "$tarball" "$name" "$version" >&2

  cp "$out/$tarball" "$dir/$tarball"
  built+=("$tarball")
done

sums="$dir/SHA256SUMS"
{
  if [ -f "$sums" ]; then
    grep -v -E '  requiem-.*-linux-.*\.tar\.gz$' "$sums" || true
  fi
  (cd "$dir" && shasum -a 256 "${built[@]}")
} >"$sums.new"
mv "$sums.new" "$sums"

echo "$dir"
ls -l "$dir"

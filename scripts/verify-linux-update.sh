#!/usr/bin/env bash
# Runs library and release-tool checks; does not claim daemon integration coverage.
set -euo pipefail
cd "$(dirname "$0")/.."
log_dir="${REQUIEM_VERIFY_LOG_DIR:-/tmp/requiem-linux-update-checks}"
mkdir -p "$log_dir"
export CARGO_BUILD_JOBS=2
cargo test --locked -p requiem-update >"$log_dir/macos-update.log" 2>&1
docker run --rm -v "$PWD:/work" -w /work \
  -e CARGO_TARGET_DIR=/work/target/linux-update -e CARGO_BUILD_JOBS=2 \
  requiem-gtk-dev cargo test --locked -p requiem-update \
  >"$log_dir/linux-update.log" 2>&1
cargo fmt -p requiem-update -p requiem-cli -p requiem-daemon -p requiem-gui -- --check >"$log_dir/format.log" 2>&1
git diff --check >"$log_dir/diff.log" 2>&1
printf 'Library and release-tool checks passed; logs: %s\n' "$log_dir"

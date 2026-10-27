# Development

## Build from source

Needs Rust (see `rust-toolchain.toml`) and libopus: `brew install opus` on macOS, `libopus-dev libasound2-dev libgtk-4-dev pkg-config` on Debian/Ubuntu, `opus-devel alsa-lib-devel gtk4-devel` on Fedora. GTK 4.8 is the floor, the version Debian 12 has.

```sh
cargo build --release          # target/release/{requiemd,requiem-gui,requiem}
scripts/bundle-macos.sh        # macOS: target/Requiem.app (ad-hoc signed), CLI at Contents/MacOS/requiem
scripts/make-dmg.sh            # macOS: bundles, then target/Requiem-<version>.dmg, the installer
scripts/release-macos.sh 0.1.0 # macOS: signed release files in target/release-files/v0.1.0/ (--publish tags and uploads)
scripts/release-linux.sh 0.1.0 # Linux tarballs (window, player, CLI, .desktop file, icons), built and smoke-tested in Docker
```

`make-dmg.sh` installs [dmgbuild](https://github.com/dmgbuild/dmgbuild) from PyPI into `target/dmgbuild-venv` on its first run.

The bundle embeds Homebrew's libopus. Its minimum macOS version is whatever that libopus was built for (the script prints it), so a bundle built on macOS 26 needs macOS 26.

## Snapshots

`requiem-gui --demo --snapshot out.png --view <view>` renders a view from fixed fake data without showing a window (on Linux it draws an undecorated window, so run it under `xvfb-run` or a headless compositor); `requiem-gui --help` lists the views (`home`, `search`, `album`, `palette` and more). For `home` and `library`, `--snapshot` also works without `--demo`: it asks the running player (starting one if needed), waits up to 20 s for the page and its covers, then writes the PNG, exiting 1 if the page fails to load.

## Footprint

Measured on an Apple Silicon Mac while playing (physical footprint):

- player (`requiemd`, with the menu-bar icon): 20–24 MiB, 0.7–0.8% CPU — mostly Opus decoding; under 0.01% paused or stopped;
- window (`requiem-gui`) while open and visible: 18–20 MiB, about 0.3% CPU.

Home loaded, a window that is never shown (`requiem-gui --snapshot out.png --view home`) peaked at about 20 MiB. Home keeps only the cards near the visible area, and artwork stays within its 8 MiB cache.

`scripts/perf-check.sh [--daemon]` re-runs the gate (it plays audio at 5% volume).


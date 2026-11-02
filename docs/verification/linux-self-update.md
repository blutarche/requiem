# Linux self-update verification

The daemon checks signed architecture-specific Linux manifests, stages bounded archives, tests each executable, and replaces only the three Requiem executable paths. GTK Settings provides check and automatic-check controls. Installation at quit and Restart to Update use the same replacement startup health check. A failed startup restores binaries and saved state before restarting the old daemon.

The signed format-1 macOS manifest endpoint remains unchanged. Writable user-owned Linux tarball installations are supported; system prefixes require manual installation. Three-path replacement uses a durable recovery journal; existing launchers from releases without recovery gates cannot participate in that recovery. Old Linux releases need one manual installation to acquire the updater.

Commands run by the parent:

- `CARGO_BUILD_JOBS=2 cargo test --workspace`: exit 0, `/tmp/requiem-final-macos-tests.log`.
- `cargo test -p requiem-update`: exit 0, 66 tests, `/tmp/requiem-final-updater-tests.log`.
- Docker `cargo test --locked -p requiem-update`: exit 0, 56 tests, `/tmp/requiem-final-linux-unit.log`.
- Docker build of CLI, daemon, GUI and release tool: exit 0, `/tmp/requiem-connected-linux-build.log`. Restored recovery source rebuilt successfully in `/tmp/linux-daemon-restored.log`.
- GUI Linux update-message regression: exit 101 before enabling Linux status messages, exit 0 afterwards, `/tmp/requiem-gtk-gate-red.log` and `/tmp/requiem-gtk-gate-green.log`.
- Real signed 0.3.12 → 0.3.13 loopback update and failed-start rollback: exit 0, `/tmp/requiem-live-update-verified.log`. All three executable versions, IPC health, volume 37, automatic-check persistence and unrelated-file preservation were checked. Connected Settings Ready/final screenshots were viewed in `target/update-evidence-verified/gui/`.
- Complete signed loopback run including automatic scheduled check and installation at daemon quit: exit 0, `/tmp/requiem-live-update-all.log`. Manual, rollback, and automatic cases each verified all three versions, IPC health, preserved volume/auto settings, and nonblank connected GTK screens. Final screenshots: `target/update-evidence-all/gui/`.
- Targeted Cargo formatting, shell syntax, Python compilation and `git diff --check`: exit 0.

The actual executable tests run `scripts/test-linux-self-update.py` under `dbus-run-session` and Xvfb, with disposable signing keys, isolated HOME/config/state/cache/socket paths and ALSA null output. The disposable higher version is built from copied sources, leaving the working copy version unchanged. Stripped test executables reduce temporary disk use.

Scope: actual Linux binary runs use aarch64. x86_64 metadata validation is covered by unit tests; a new x86_64 release build was skipped to avoid rebuilding release images. A cross-model architecture review was attempted but its provider returned an API rate limit; implementation and primary verification proceeded without that external review. No release publication has been performed for these changes.

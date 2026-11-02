# Updates

On macOS, Requiem updates itself from signed GitHub releases when it runs from Applications, or another folder you can write to. A downloaded update installs when the player next quits, or at once through **Restart to Update** in the window or the menu-bar menu. **Check for Updates…** and **Check for Updates Automatically** are in the Requiem menu; `requiem update` shows the status. Design: [`docs/self-update-design.md`](self-update-design.md).

The player fetches two small files from github.com at most once every six hours, to learn whether a newer release exists, and downloads it from GitHub when there is one. GitHub sees your IP address, the time and the file fetched. Nothing else is sent. Turning off Check for Updates Automatically stops these requests.


Linux release metadata tooling: `requiem-release linux-manifest --archive <tar.gz> --version <version> --arch <aarch64|x86_64>` creates a separate architecture-specific manifest. Use `sign --key <key-path> --in <manifest>` and `linux-verify --dir <directory> --arch <arch>` to authenticate its archive digest. Verification does not extract or execute the archive.


On Linux, Settings → Updates offers **Check for Updates** and **Check for updates automatically**. The daemon checks signed metadata for this machine’s architecture, downloads and verifies the tarball, and tests all three executables before showing **Restart to Update**. Updates also install when the player next quits; the replacement daemon starts paused to verify its health, then follows its normal idle-exit policy.

Self-update requires the three tarball executables (`requiem`, `requiemd`, and `requiem-gui`) together in a directory owned and writable by your user. System installations require a manual update; the app never asks for sudo. It replaces only its executables and keeps recovery files in a private `.requiem-update` directory beside them. Failed startup restores the previous binaries and saved application state. Installation uses a recovery journal because three separate entry paths cannot be replaced atomically. Launchers from releases before Linux self-update do not participate in recovery.

Older Linux releases have no updater and need one manual installation of a release containing this feature. Linux manifests use separate architecture-specific filenames so existing macOS clients continue using their current signed manifest.

Local verification: `scripts/verify-linux-update.sh` checks updater libraries. `scripts/test-linux-self-update.py --help` describes the signed two-version installation and failed-start rollback test; run it under a session D-Bus and Xvfb with two prebuilt Linux debug releases.

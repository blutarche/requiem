# How it fits together

- **`requiemd`** — the player. Plays audio, owns the queue, and keeps running when the window is closed. On macOS it shows a menu-bar icon (playing / paused, with controls), publishes Now Playing, and answers the media keys. It exits after 5 minutes stopped with no window or CLI connected and no sign-in waiting for its code. It remembers the queue, the track Play resumes, and the volume across restarts, in `~/Library/Application Support/requiem/state.json` (`$XDG_STATE_HOME/requiem/state.json` on Linux).
- **`requiem-gui`** — the window: Home, search, your YouTube Library, queue, player bar, and a Cmd+K command palette. It starts the player if needed and quits when its window closes; playback continues. macOS only in v1.
- **`requiem`** — the CLI. `requiem --connect` drives the running player (starting it if needed); plain `requiem` plays in-process.

They talk over a per-user Unix socket. "Quit and Stop Music" in the menu bar stops the player and closes the window.

On Linux, `requiemd` shows a StatusNotifierItem tray icon (play/pause state, a menu with Play/Pause, Next, Previous and Quit, click to toggle) and publishes MPRIS2 as `org.mpris.MediaPlayer2.requiem`, so media keys, desktop widgets and `playerctl` control it. Both use the session D-Bus through pure-Rust crates: no GTK, appindicator or libdbus, so no new system libraries. GNOME shows tray icons only with the AppIndicator extension; without it the tray is skipped and MPRIS still works, and with no session bus the player runs headless. Either way it logs one line. Check with `playerctl -l` and `playerctl -p requiem status`.


Design: [`docs/plans/2026-10-11-v1-design.md`](plans/2026-10-11-v1-design.md).

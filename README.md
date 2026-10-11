# requiem

A lightweight music app that uses about as many resources as a dead person 💀

Native desktop client for **YouTube Music**, built to stay open all day without hogging RAM or CPU. Spotify support is deferred (see [`docs/plans/2026-10-10-spotify-spike.md`](docs/plans/2026-10-10-spotify-spike.md)).

> **Status:** v1 — YouTube Music on macOS (menu-bar player + native window), plus a CLI. Linux runs the player and CLI headless. Design: [`docs/plans/2026-10-11-v1-design.md`](docs/plans/2026-10-11-v1-design.md).

## How it fits together

- **`requiemd`** — the player. Plays audio, owns the queue, and keeps running when the window is closed. On macOS it shows a menu-bar icon (playing / paused, with controls), publishes Now Playing, and answers the media keys. It exits after 5 minutes stopped with no window or CLI connected. It remembers the queue, the track Play resumes, and the volume across restarts, in `~/Library/Application Support/requiem/state.json` (`$XDG_STATE_HOME/requiem/state.json` on Linux).
- **`requiem-gui`** — the window: search, queue, player bar, and a Cmd+K command palette. It starts the player if needed and quits when its window closes; playback continues. macOS only in v1.
- **`requiem`** — the CLI. `requiem --connect` drives the running player (starting it if needed); plain `requiem` plays in-process.

They talk over a per-user Unix socket. "Quit and Stop Music" in the menu bar stops the player and closes the window.

## Build

Needs Rust (see `rust-toolchain.toml`) and libopus: `brew install opus` on macOS, `libopus-dev libasound2-dev pkg-config` on Debian/Ubuntu.

```sh
cargo build --release          # target/release/{requiemd,requiem-gui,requiem}
scripts/bundle-macos.sh        # macOS: target/requiem.app (ad-hoc signed)
open target/requiem.app
```

The bundle embeds Homebrew's libopus. Its minimum macOS version is whatever that libopus was built for (the script prints it), so a bundle built on macOS 26 needs macOS 26.

## Use

- **Window:** type a query and press Enter; Return or a double-click plays a result, Cmd+Return (or the row's +) adds it to the queue. In the queue, Delete removes the selected track and Option+Cmd+Up / Option+Cmd+Down move it; right-click a queue row for Remove from Queue, Move Up, Move Down or Clear Queue (its toast offers Undo). Cmd+Z undoes a replaced or cleared queue until the queue changes again. Space toggles play/pause, Left / Right seek 5 s, Cmd+Left / Cmd+Right skip, Cmd+Up / Cmd+Down change the volume, Cmd+F focuses search, Cmd+1 / Cmd+2 switch between Search and Queue, Cmd+K opens the palette. Cmd+Q closes the window and the music keeps playing; Option+Cmd+Q ("Quit and Stop Music") stops the player too.
- **CLI:** `requiem --connect`, then `help`. Commands: `search <query>`, `play <n>`, `add <n>`, `queue`, `remove <n>`, `move <from> <to>`, `clear`, `next`, `prev`, `pause`, `resume`, `toggle`, `seek <secs|+secs|-secs>`, `vol <0-100>`, `status`, `shutdown-daemon`, `quit`. `--script <file>` runs commands from a file.

## Footprint

Measured on an Apple Silicon Mac while playing (physical footprint):

- player (`requiemd`, with the menu-bar icon): 20–24 MiB, 0.7–0.8% CPU — mostly Opus decoding; under 0.01% paused or stopped;
- window (`requiem-gui`) while open and visible: 18–20 MiB, about 0.3% CPU.

`scripts/perf-check.sh [--daemon]` re-runs the gate (it plays audio at 5% volume).

## Notes

- requiem is an unofficial client. It is not affiliated with or endorsed by Spotify, YouTube, or Google.
- YouTube Music playback is anonymous and ad-free. That breaks YouTube's Terms of Service; you use it at your own risk, and YouTube may break it at any time.

## License

requiem is MIT OR Apache-2.0. The app bundle also ships:

- [Inter](https://rsms.me/inter/) — SIL Open Font License 1.1;
- [Lucide](https://lucide.dev) icons — ISC;
- [libopus](https://opus-codec.org) — BSD-3-Clause.

Their license texts are in `requiem.app/Contents/Resources/licenses/`.

# requiem

A lightweight music app that uses about as many resources as a dead person 💀

Native desktop client for **YouTube Music**, built to stay open all day without hogging RAM or CPU. Spotify support is deferred (see [`docs/plans/2026-10-10-spotify-spike.md`](docs/plans/2026-10-10-spotify-spike.md)).

> **Status:** v1 — YouTube Music on macOS (menu-bar player + native window), plus a CLI. Linux runs the player and CLI, with a tray icon and MPRIS. Design: [`docs/plans/2026-10-11-v1-design.md`](docs/plans/2026-10-11-v1-design.md).

## How it fits together

- **`requiemd`** — the player. Plays audio, owns the queue, and keeps running when the window is closed. On macOS it shows a menu-bar icon (playing / paused, with controls), publishes Now Playing, and answers the media keys. It exits after 5 minutes stopped with no window or CLI connected and no sign-in waiting for its code. It remembers the queue, the track Play resumes, and the volume across restarts, in `~/Library/Application Support/requiem/state.json` (`$XDG_STATE_HOME/requiem/state.json` on Linux).
- **`requiem-gui`** — the window: search, queue, player bar, and a Cmd+K command palette. It starts the player if needed and quits when its window closes; playback continues. macOS only in v1.
- **`requiem`** — the CLI. `requiem --connect` drives the running player (starting it if needed); plain `requiem` plays in-process.

They talk over a per-user Unix socket. "Quit and Stop Music" in the menu bar stops the player and closes the window.

On Linux, `requiemd` shows a StatusNotifierItem tray icon (play/pause state, a menu with Play/Pause, Next, Previous and Quit, click to toggle) and publishes MPRIS2 as `org.mpris.MediaPlayer2.requiem`, so media keys, desktop widgets and `playerctl` control it. Both use the session D-Bus through pure-Rust crates: no GTK, appindicator or libdbus, so no new system libraries. GNOME shows tray icons only with the AppIndicator extension; without it the tray is skipped and MPRIS still works, and with no session bus the player runs headless. Either way it logs one line. Check with `playerctl -l` and `playerctl -p requiem status`.

## Build

Needs Rust (see `rust-toolchain.toml`) and libopus: `brew install opus` on macOS, `libopus-dev libasound2-dev pkg-config` on Debian/Ubuntu.

```sh
cargo build --release          # target/release/{requiemd,requiem-gui,requiem}
scripts/bundle-macos.sh        # macOS: target/requiem.app (ad-hoc signed), CLI at Contents/MacOS/requiem
open target/requiem.app
```

The bundle embeds Homebrew's libopus. Its minimum macOS version is whatever that libopus was built for (the script prints it), so a bundle built on macOS 26 needs macOS 26.

## Use

- **Window:** type a query and press Enter; Return or a double-click plays a result, Cmd+Return (or the row's +) adds it to the queue. In the queue, Delete removes the selected track and Option+Cmd+Up / Option+Cmd+Down move it; right-click a queue row for Remove from Queue, Move Up, Move Down or Clear Queue (its toast offers Undo). Cmd+Z undoes a replaced or cleared queue until the queue changes again. Space toggles play/pause, Left / Right seek 5 s, Cmd+Left / Cmd+Right skip, Cmd+Up / Cmd+Down change the volume, Cmd+F focuses search, Cmd+1 / Cmd+2 switch between Search and Queue, Cmd+K opens the palette. Cmd+Q closes the window and the music keeps playing; Option+Cmd+Q ("Quit and Stop Music") stops the player too.
- **CLI:** `requiem --connect`, then `help`. Commands: `search <query>`, `home`, `library`, `open <n>`, `play <n>`, `add <n>`, `play-track <json>`, `add-track <json>`, `queue`, `jump <n>`, `remove <n>`, `move <from> <to>`, `clear`, `next`, `prev`, `pause`, `resume`, `toggle`, `seek <secs|+secs|-secs>`, `vol <0-100>`, `status`, `signin`, `signout`, `account`, `shutdown-daemon`, `quit`. `home` and `library` print numbered items under each shelf, numbered across shelves; `open <n>` lists the tracks of playlist or album n. `play <n>`, `add <n>` and `open <n>` take n from whichever listing printed last: search results, a browse page or an opened playlist. `--script <file>` runs commands from a file; `--script -` reads them from stdin.

### JSON output

`requiem --connect --json` takes the same commands but prints one JSON object per line on stdout, each with a `"type"` field. Arguments that fail to parse still go to stderr as text; a failure to reach the daemon is a `error` object with exit code 1. Positions (`index`, `current`) are 1-based, as the commands take them. `play-track` and `add-track` take one track object on one line, as a `result` line or a track `item` line prints it (`id`, `title` and `artists` are required), and act like `play` and `add` for that exact track. Search and browse results arrive asynchronously, so a script that needs them uses `await results`.

| `type` | Fields |
| --- | --- |
| `result` | `index`, `id` (`provider`, `id`), `title`, `artists`, `album`, `duration_ms`, `artwork_url` (the last three may be `null`) |
| `no_results`, `queue_empty` | none |
| `search_failed` | `error` |
| `shelf` | `title`: the `item` lines that follow belong to it |
| `item` | `index`, `kind` (`track`, `playlist` or `album`), `id` (`provider`, `id`), `title`, `subtitle`, `artwork_url`; a track also carries the track fields above, and its `subtitle` is its artists |
| `browse_failed` | `error`, `sign_in_required` (bool: `signin` would let it succeed) |
| `account` | `provider`, `state`: `unavailable` (with `detail`, what to set up), `signed_out`, `pending` (with `verification_url`, `user_code`, `expires_at_unix`) or `signed_in` (with `name`, possibly `null`) |
| `account_error` | `provider`, `error` |
| `no_accounts` | none |
| `queue_item` | `index`, `current` (bool), plus the track fields above |
| `queue_changed` | `count`, `current` (`null` if none) |
| `now_playing` | the track fields above |
| `status` | `state`, `track` (the track fields, or `null`), `position_ms`, `duration_ms`, `volume` |
| `state` | `state`: `playing`, `paused`, `buffering` or `stopped` |
| `volume` | `volume` |
| `track_failed` | `title`, `error` |
| `provider_broken` | `detail` |
| `ack` | `command`: a command was sent (search, home, library, open, play, add, play-track, add-track, jump, remove, move, clear, next, prev, pause, resume, toggle, seek, vol, signin, signout, shutdown-daemon) |
| `mark` | `name`, `unix_millis` |
| `help` | `text` |
| `error` | `message` |

## YouTube library

`home` shows YouTube Music's home page as a signed-out visitor sees it. Signing in to Google adds `library`: your playlists and your liked videos.

Sign-in uses your own Google Cloud OAuth client, set up once:

1. In the [Google Cloud console](https://console.cloud.google.com/), create a project.
2. Enable the **YouTube Data API v3** for it.
3. Configure its OAuth consent screen (Google Auth Platform) with an **External** audience, and add the scope `https://www.googleapis.com/auth/youtube.readonly`.
4. Publish the app so its status is **In production**: while it is in **Testing**, Google expires the sign-in after 7 days. Google may then warn that the app is unverified; as its only user, you can continue past the warning.
5. Create an OAuth client of type **TVs and Limited Input devices** and download its JSON.
6. Save that JSON as `~/Library/Application Support/requiem/google-client.json` on macOS, or `$XDG_CONFIG_HOME/requiem/google-client.json` (default `~/.config/requiem/google-client.json`) on Linux. `REQUIEM_GOOGLE_CLIENT_FILE` overrides the path.

Then run `signin` in the CLI. It prints `Go to https://www.google.com/device and enter XXXX-XXXX`; open that page on any device, enter the code and allow access. `signin` waits until you do or the code expires. The player keeps the sign-in in `google-token.json`, readable only by you, next to `state.json` (`REQUIEM_GOOGLE_TOKEN_FILE` overrides the path). `account` shows the state; `signout` revokes the sign-in and deletes the file.

What a sign-in gets you:

- **Available:** your playlists (the first 200) and Liked videos, each opened up to its first 1000 videos. These come from the YouTube Data API, so a project gets 10,000 requests a day, and each request returns up to 50 items.
- **Not available:** your personalized home, Quick picks, saved albums and history. YouTube Music's own API rejects this kind of sign-in. Home always shows the public shelves.

Playback stays anonymous whether or not you are signed in.

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

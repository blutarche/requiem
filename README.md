# Requiem

A lightweight music app that uses about as many resources as a dead person 💀

Native desktop client for **YouTube Music** and **Spotify**, built to stay open all day without hogging RAM or CPU.

> **Status:** v1 — YouTube Music on macOS (menu-bar player + native window), plus a CLI. Linux runs the player and CLI, with a tray icon and MPRIS. Design: [`docs/plans/2026-10-11-v1-design.md`](docs/plans/2026-10-11-v1-design.md).

## How it fits together

- **`requiemd`** — the player. Plays audio, owns the queue, and keeps running when the window is closed. On macOS it shows a menu-bar icon (playing / paused, with controls), publishes Now Playing, and answers the media keys. It exits after 5 minutes stopped with no window or CLI connected and no sign-in waiting for its code. It remembers the queue, the track Play resumes, and the volume across restarts, in `~/Library/Application Support/requiem/state.json` (`$XDG_STATE_HOME/requiem/state.json` on Linux).
- **`requiem-gui`** — the window: Home, search, your YouTube Library, queue, player bar, and a Cmd+K command palette. It starts the player if needed and quits when its window closes; playback continues. macOS only in v1.
- **`requiem`** — the CLI. `requiem --connect` drives the running player (starting it if needed); plain `requiem` plays in-process.

They talk over a per-user Unix socket. "Quit and Stop Music" in the menu bar stops the player and closes the window.

On Linux, `requiemd` shows a StatusNotifierItem tray icon (play/pause state, a menu with Play/Pause, Next, Previous and Quit, click to toggle) and publishes MPRIS2 as `org.mpris.MediaPlayer2.requiem`, so media keys, desktop widgets and `playerctl` control it. Both use the session D-Bus through pure-Rust crates: no GTK, appindicator or libdbus, so no new system libraries. GNOME shows tray icons only with the AppIndicator extension; without it the tray is skipped and MPRIS still works, and with no session bus the player runs headless. Either way it logs one line. Check with `playerctl -l` and `playerctl -p requiem status`.

## Install

On macOS, download `Requiem-<version>.dmg` from the [latest release](https://github.com/blutarche/requiem/releases/latest), open it and drag Requiem onto Applications. Then open Requiem from Applications.

The first time you open Requiem, macOS blocks it because it isn't notarized: click **Done**, then in **System Settings → Privacy & Security** click **Open Anyway**.

## Build from source

Needs Rust (see `rust-toolchain.toml`) and libopus: `brew install opus` on macOS, `libopus-dev libasound2-dev pkg-config` on Debian/Ubuntu.

```sh
cargo build --release          # target/release/{requiemd,requiem-gui,requiem}
scripts/bundle-macos.sh        # macOS: target/Requiem.app (ad-hoc signed), CLI at Contents/MacOS/requiem
scripts/make-dmg.sh            # macOS: bundles, then target/Requiem-<version>.dmg, the installer
scripts/release-macos.sh 0.1.0 # macOS: signed release files in target/release-files/v0.1.0/ (--publish tags and uploads)
```

`make-dmg.sh` installs [dmgbuild](https://github.com/dmgbuild/dmgbuild) from PyPI into `target/dmgbuild-venv` on its first run.

The bundle embeds Homebrew's libopus. Its minimum macOS version is whatever that libopus was built for (the script prints it), so a bundle built on macOS 26 needs macOS 26.

## Use

- **Window:** type a query and press Enter; Return or a double-click plays a result, Cmd+Return (or the row's +) adds it to the queue. In the queue, Delete removes the selected track and Option+Cmd+Up / Option+Cmd+Down move it; right-click a queue row for Remove from Queue, Move Up, Move Down or Clear Queue (its toast offers Undo). Cmd+Z undoes a replaced or cleared queue until the queue changes again. Space toggles play/pause, Left / Right seek 5 s, Cmd+Left / Cmd+Right skip, Cmd+Up / Cmd+Down change the volume, Cmd+F focuses search, Cmd+1 to Cmd+4 switch between Home, Search, Library and Queue, Cmd+R refreshes the page, Cmd+K opens the palette. Cmd+Q closes the window and the music keeps playing; Option+Cmd+Q ("Quit and Stop Music") stops the player too.
- **Home and Library:** the window opens on Home, your own music from every service you are signed in to: your most recently liked songs, the playlists Spotify made for you, and your playlists. A shelf that both services have takes from each in turn, and so do search results and the Library. Where a list, or Home, holds music from both services, each cover carries a small badge for its service, and so does the playing track's while the queue mixes them. Signed out, Home offers a sign-in to each service. A track card plays its shelf from that track and offers Undo like a search result; a playlist or album card opens its tracks (Play, Add to Queue, and Cmd+[ or the back control to return where you were). Library lists your playlists once you sign in: for Spotify, the Library page shows the device code with Copy Code, Open and Cancel while a sign-in waits; for YouTube Music, Settings opens its browser-session form. "Sign Out" asks first. The palette and the View menu have Sign in and Sign out for each service while they apply. With both services signed in, the control above Settings (or "Show Spotify Only" and its siblings in the palette) shows All, Spotify or YouTube Music on Home, the Library and search results, and Requiem remembers the choice.
- **CLI:** `requiem --connect`, then `help`. Commands: `search <query>`, `home`, `library`, `open <n>`, `play <n>`, `add <n>`, `play-track <json>`, `add-track <json>`, `queue`, `jump <n>`, `remove <n>`, `move <from> <to>`, `clear`, `next`, `prev`, `pause`, `resume`, `toggle`, `seek <secs|+secs|-secs>`, `vol <0-100>`, `status`, `signin`, `signout`, `account`, `update`, `update check`, `update install`, `shutdown-daemon`, `quit`. `home` and `library` print numbered items under each shelf, numbered across shelves; `open <n>` lists the tracks of playlist or album n. `play <n>`, `add <n>` and `open <n>` take n from whichever listing printed last: search results, a browse page or an opened playlist. `--script <file>` runs commands from a file; `--script -` reads them from stdin. `requiem update`, `requiem update check` and `requiem update install` run one update command against the player (see [Updates](#updates)). `requiem signin-browser <provider> <file|->` signs the player in with a browser session (see [Sign in with your browser session](#sign-in-with-your-browser-session)).

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
| `page_changed` | `page` (`home` or `library`): the listing printed is of that page, which has changed since; running the command again prints it anew |
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
| `update` | `current`, `auto` (bool), `state`: `idle`, `checking`, `up_to_date`, `downloading` (with `version`), `ready` (with `version`), `failed` (with `version`, `reason`), `error` (with `reason`: a failed check, or a failed install that is tried again), `not_updatable` (with `reason`) or `unsupported` |
| `update_downloading`, `update_restarting` | `version` |
| `ack` | `command`: a command was sent (search, home, library, open, play, add, play-track, add-track, jump, remove, move, clear, next, prev, pause, resume, toggle, seek, vol, signin, signout, signin-browser, update check, update install, shutdown-daemon); `signout` acks only once the provider reports it signed out, and `signin-browser` once the provider has taken the session; a failed sign-out or session import prints `account_error` instead and exits 2 in script mode |
| `mark` | `name`, `unix_millis` |
| `help` | `text` |
| `error` | `message` |

## YouTube Music

Search and playback work without signing in. Signing in adds your own YouTube Music to Home and the Library, and to `home` and `library` in the CLI.

### Sign in with your browser session

Requiem signs in to YouTube Music with the session of a browser where you are signed in. Home shows the first 6 shelves of YouTube Music's own Home, Library shows your Liked music and your playlists, and your private playlists and mixes open. Requiem sends the session only to YouTube Music's own API, for Home, Library, the playlists you open and your account's name. Search and playback stay anonymous, and plays aren't reported to Google.

1. In a browser where you are signed in, open [music.youtube.com](https://music.youtube.com).
2. Open the browser's developer tools with Option+Cmd+I and go to the **Network** tab. In Safari, first turn on **Show features for web developers** in **Safari → Settings… → Advanced**.
3. Type `browse` in the Network tab's filter, then click **Explore** or **Library** in YouTube Music's sidebar (or scroll Home) so a `browse` request appears. Right-click it (in Safari, Control-click).
   - **Chrome:** choose **Copy → Copy as cURL**.
   - **Firefox:** in the **Copy** submenu, choose **Copy Request Headers** or **Copy as cURL**.
   - **Safari:** choose **Copy as cURL**.
4. In Requiem, choose **Sign in to YouTube Music** (on Home, the Library or in the palette), or open **Settings** and click **Sign In…** under YouTube Music. Settings shows these steps; click **Paste from Clipboard**, or press Cmd+V.

Requiem checks the session with YouTube Music before it keeps it. Once it does, it clears this Mac's clipboard, unless you copied something else since. Clear the clipboard yourself on a computer you copied from.

**What Requiem keeps:** the request's cookie and its `X-Goog-AuthUser`, `X-Goog-PageId` and `X-Goog-Visitor-Id` headers, with your account's name, in `ytmusic-session.json` next to `state.json` (on macOS, `~/Library/Application Support/requiem/`). The rest of the paste is dropped. The file is readable only by you and, on macOS, excluded from Time Machine backups. A new session replaces it: **Use Browser Session…** in Settings, while you are signed in, takes another. `account` in the CLI shows the state.

**The risks:** the cookie is your Google sign-in, not just YouTube Music's. Anyone who has the file, or the copied text, can use your Google account until that browser signs out. Using it is against YouTube's terms, and Google could flag the account.

**Signing out:** **Sign Out…** in Settings, or `signout ytmusic` in the CLI, deletes the file. If the file can't be deleted, it says why, and the session stays until you sign out again. Requiem can't sign your browser out: the copied cookie keeps working until you sign out of Google in that browser profile, which ends it. When Google ends a session, Requiem signs YouTube Music out and asks for a new one in Settings.

**From another computer:** over ssh, the CLI reads the paste from stdin with `-`, or from a file. Copy the headers on the computer with the browser, then send them from that computer's clipboard, so they never land on the Mac's disk or in a shell history:

```sh
pbpaste | ssh my-mac 'requiem signin-browser ytmusic -'
```

On Linux, use `wl-paste` or `xclip -o -selection clipboard` in place of `pbpaste`. Or save them to a file, send that, and delete it afterwards:

```sh
ssh my-mac 'requiem signin-browser ytmusic -' < headers.txt
```

Pasting into `requiem signin-browser ytmusic -` at a prompt on the Mac doesn't work: macOS passes a program at most 1023 characters of a typed or pasted line, and the cookie line is longer. In the app bundle the CLI is `/Applications/Requiem.app/Contents/MacOS/requiem`. It returns once YouTube Music has taken the session; if YouTube Music refuses it, it prints why and exits 2. With `--json` it prints an `ack` or an `account_error`. `signin ytmusic` doesn't sign in; it says to use `signin-browser`.

**From an earlier version:** Requiem used to sign in to YouTube Music with your own Google Cloud client. That sign-in, `google-token.json`, is deleted when the player starts, so YouTube Music reads as signed out until you sign in with a browser session. Requiem leaves a `google-client.json` alone; delete it if you no longer want it, and remove Requiem's access at <https://myaccount.google.com/permissions>.

## Spotify

Spotify is off until you sign in: `signin spotify` in the CLI, or **Sign in to Spotify** in the app. Enter the code it shows at <https://www.spotify.com/pair>. Playback needs Spotify Premium.

Requiem plays Spotify through [librespot](https://github.com/librespot-org/librespot), the way Spotify's own apps connect. Spotify doesn't allow unofficial players, so your Spotify account could be suspended.

Once signed in, search lists Spotify results first, Home shows your 20 most recently liked songs, the playlists Spotify made for you and your playlists, and Library shows Liked Songs and your playlists. The player keeps the sign-in in `spotify-credentials.json`, readable only by you, next to `state.json` (`REQUIEM_SPOTIFY_CREDENTIALS_FILE` overrides the path). `signout spotify` deletes it. Design: [`docs/plans/2026-10-16-spotify-provider-design.md`](docs/plans/2026-10-16-spotify-provider-design.md).

## Snapshots

`requiem-gui --demo --snapshot out.png --view <view>` renders a view from fixed fake data without showing a window: `home`, `home-signed-out`, `search`, `library`, `queue`, `settings`, `palette`, `collection`, `library-signed-out`, `library-unavailable`, `signin`, `settings-session`, `empty`, `disconnected`, `toast`, `out-of-date`, `update-ready`, `update-failed`, `not-updatable` or `up-to-date`. For `home` and `library`, `--snapshot` also works without `--demo`: it asks the running player (starting one if needed), waits up to 20 s for the page and its covers, then writes the PNG, exiting 1 if the page fails to load.

## Footprint

Measured on an Apple Silicon Mac while playing (physical footprint):

- player (`requiemd`, with the menu-bar icon): 20–24 MiB, 0.7–0.8% CPU — mostly Opus decoding; under 0.01% paused or stopped;
- window (`requiem-gui`) while open and visible: 18–20 MiB, about 0.3% CPU.

Home loaded, a window that is never shown (`requiem-gui --snapshot out.png --view home`) peaked at about 20 MiB. Home keeps only the cards near the visible area, and artwork stays within its 8 MiB cache.

`scripts/perf-check.sh [--daemon]` re-runs the gate (it plays audio at 5% volume).

## Updates

On macOS, Requiem updates itself from signed GitHub releases when it runs from Applications, or another folder you can write to. A downloaded update installs when the player next quits, or at once through **Restart to Update** in the window or the menu-bar menu. **Check for Updates…** and **Check for Updates Automatically** are in the Requiem menu; `requiem update` shows the status. Design: [`docs/self-update-design.md`](docs/self-update-design.md).

The player fetches two small files from github.com at most once every six hours, to learn whether a newer release exists, and downloads it from GitHub when there is one. GitHub sees your IP address, the time and the file fetched. Nothing else is sent. Turning off Check for Updates Automatically stops these requests.

## Notes

- Requiem is an unofficial client. It is not affiliated with or endorsed by Spotify, YouTube, or Google.
- YouTube Music playback is anonymous and ad-free. That breaks YouTube's Terms of Service; you use it at your own risk, and YouTube may break it at any time.

## License

Requiem is MIT OR Apache-2.0. The app bundle also ships:

- [Inter](https://rsms.me/inter/) — SIL Open Font License 1.1;
- [Lucide](https://lucide.dev) icons — ISC;
- [libopus](https://opus-codec.org) — BSD-3-Clause.

Their license texts are in `Requiem.app/Contents/Resources/licenses/`.

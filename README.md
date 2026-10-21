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
- **Home and Library:** the window opens on Home, your own music from every service you are signed in to: your most recently liked songs, the playlists Spotify made for you, and your playlists. A shelf that both services have takes from each in turn, and so do search results and the Library. Where a list, or Home, holds music from both services, each cover carries a small badge for its service, and so does the playing track's while the queue mixes them. Signed out, Home offers a sign-in to each service. A track card plays its shelf from that track and offers Undo like a search result; a playlist or album card opens its tracks (Play, Add to Queue, and Cmd+[ or the back control to return where you were). Library lists your playlists once you sign in: the Library page shows the device code with Copy Code, Open and Cancel while a sign-in waits, and "Sign Out" asks first. The palette and the View menu have Sign in and Sign out for each service while they apply.
- **CLI:** `requiem --connect`, then `help`. Commands: `search <query>`, `home`, `library`, `open <n>`, `play <n>`, `add <n>`, `play-track <json>`, `add-track <json>`, `queue`, `jump <n>`, `remove <n>`, `move <from> <to>`, `clear`, `next`, `prev`, `pause`, `resume`, `toggle`, `seek <secs|+secs|-secs>`, `vol <0-100>`, `status`, `signin`, `signout`, `signin-client <provider> <path>`, `account`, `update`, `update check`, `update install`, `shutdown-daemon`, `quit`. `home` and `library` print numbered items under each shelf, numbered across shelves; `open <n>` lists the tracks of playlist or album n. `play <n>`, `add <n>` and `open <n>` take n from whichever listing printed last: search results, a browse page or an opened playlist. `--script <file>` runs commands from a file; `--script -` reads them from stdin. `requiem update`, `requiem update check` and `requiem update install` run one update command against the player (see [Updates](#updates)). `requiem signin-browser <provider> <file|->` signs the player in with a browser session (see [Sign in with your browser session](#sign-in-with-your-browser-session)).

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
| `update` | `current`, `auto` (bool), `state`: `idle`, `checking`, `up_to_date`, `downloading` (with `version`), `ready` (with `version`), `failed` (with `version`, `reason`), `error` (with `reason`: a failed check, or a failed install that is tried again), `not_updatable` (with `reason`) or `unsupported` |
| `update_downloading`, `update_restarting` | `version` |
| `ack` | `command`: a command was sent (search, home, library, open, play, add, play-track, add-track, jump, remove, move, clear, next, prev, pause, resume, toggle, seek, vol, signin, signout, signin-client, signin-browser, update check, update install, shutdown-daemon); `signout` acks only once the provider reports it signed out, and `signin-client` and `signin-browser` once the provider has taken the client or session; a failed sign-out or client import prints `account_error` instead and exits 2 in script mode |
| `mark` | `name`, `unix_millis` |
| `help` | `text` |
| `error` | `message` |

## YouTube Music

Signing in to Google adds YouTube Music to `home` and `library`: your playlists and your liked videos.

Sign-in uses your own Google Cloud OAuth client, set up once in the [Google Cloud console](https://console.cloud.google.com/), or the session of a browser where you are signed in (see [Sign in with your browser session](#sign-in-with-your-browser-session)). Until there is one, **Sign in to YouTube Music** in the app shows these steps for the client:

1. **Create a Google Cloud project and enable the YouTube Data API v3.** Create the project from the project picker at the top of the console. Then, in **APIs & Services → Library**, find **YouTube Data API v3** and click **Enable**.
2. **Set up the OAuth consent screen and publish the app.** In **Google Auth Platform**, choose an **External** audience, and under **Data Access** add the scope `https://www.googleapis.com/auth/youtube.readonly`. Then, under **Audience**, publish the app so its status is **In production**: while it is in **Testing**, Google expires the sign-in after 7 days. Before Google lets you publish, the **Branding** page needs a home page and a privacy policy link, and their domain under **Authorized domains**. `https://github.com/blutarche/requiem`, `https://github.com/blutarche/requiem/blob/main/PRIVACY.md` and `github.com` work. Google may then warn that the app is unverified; as its only user, you can continue past the warning (**Advanced**, then **Go to … (unsafe)**).
3. **Create an OAuth client of type "TVs and Limited Input devices" and download its JSON.** Under **Clients**, create a client of that type, then download the JSON from the dialog that shows the new client.
4. **Choose that file, or type in the client.** In the app, click **Choose Client File…** under the steps and pick the JSON. Or, with no file on this Mac, click **Enter Client ID…** under the steps, or open **Settings** (**Requiem → Settings…**) and click **Use Google Client…** under YouTube Music. Then enter the **Client ID** (it ends in `.apps.googleusercontent.com`) and the **Client secret** from the same dialog, then click **Save**. Either way, the sign-in starts as soon as Requiem takes the client. In the CLI, run `signin-client ytmusic <path>`, then `signin`.

Requiem keeps its own copy of the client, readable only by you, as `~/Library/Application Support/requiem/google-client.json` on macOS, or `$XDG_CONFIG_HOME/requiem/google-client.json` (default `~/.config/requiem/google-client.json`) on Linux. `REQUIEM_GOOGLE_CLIENT_FILE` overrides the path. Saving the JSON there yourself also works: `signin` reads it, and the app sees it once the player restarts. Choosing a client with another client ID ends the current sign-in, so you sign in again with the new client; in the app, **Change Google Client…** in Settings does that.

Signing in shows a code: the app shows it on the Library page, or in Settings when you signed in there, and the CLI's `signin` prints `Go to https://www.google.com/device and enter XXXX-XXXX`. Open that page on any device, enter the code and allow access. `signin` waits until you do or the code expires. The player keeps the sign-in in `google-token.json`, readable only by you, next to `state.json` (`REQUIEM_GOOGLE_TOKEN_FILE` overrides the path). `account` shows the state; `signout` revokes the sign-in, deletes the file and returns once both are done.

What a sign-in gets you:

- **Available:** your playlists (the first 200) and Liked videos, each opened up to its first 1000 videos. These come from the YouTube Data API, so a project gets 10,000 requests a day, and each request returns up to 50 items.
- **Not available:** YouTube Music's personalized home, Quick picks, saved albums and history. YouTube Music's own API rejects this kind of sign-in, so Home shows your 20 most recently liked videos and your playlists instead.

Playback stays anonymous whether or not you are signed in.

### Sign in with your browser session

Instead of a Google client, Requiem can use the sign-in of a browser where you are signed in to YouTube Music. That needs no Google Cloud project, and it gets what the client can't: Home shows the first 6 shelves of YouTube Music's own Home, Library shows your Liked music and your playlists, and your private playlists and mixes open. Requiem sends the session only to YouTube Music's own API, for Home, Library, the playlists you open and your account's name. Search and playback stay anonymous, and plays aren't reported to Google.

1. In a browser where you are signed in, open [music.youtube.com](https://music.youtube.com).
2. Open the browser's developer tools on the **Network** tab, then reload the page.
   - **Chrome** (and Edge or Brave): **View → Developer → Developer Tools**, or Option+Cmd+I.
   - **Firefox:** **Tools → Browser Tools → Web Developer Tools**, or Option+Cmd+I.
   - **Safari:** first turn on **Show features for web developers** in **Safari → Settings… → Advanced**. Then **Develop → Show Web Inspector**, or Option+Cmd+I.
3. Type `browse` in the Network tab's filter and right-click a request named `browse`.
   - **Chrome:** choose **Copy → Copy request headers**, or **Copy → Copy as cURL**.
   - **Firefox:** choose **Copy Value → Copy Request Headers**, or **Copy Value → Copy as cURL**.
   - **Safari:** choose **Copy as cURL**.
4. In Requiem, open **Settings** and click **Use Browser Session…** under YouTube Music (the Library's setup sheet has it too), then **Paste from Clipboard**, or press Cmd+V.

Requiem checks the session with YouTube Music before it keeps it. Once it does, it clears this Mac's clipboard, unless you copied something else since. Clear the clipboard yourself on a computer you copied from.

**What Requiem keeps:** the request's cookie and its `X-Goog-AuthUser`, `X-Goog-PageId` and `X-Goog-Visitor-Id` headers, with your account's name, in `ytmusic-session.json` next to `google-token.json` (on macOS, `~/Library/Application Support/requiem/`). The rest of the paste is dropped. The file is readable only by you and, on macOS, excluded from Time Machine backups. A new session replaces it and leaves a Google client and its sign-in alone; while there is a session, Home and Library use it.

**The risks:** the cookie is your Google sign-in, not just YouTube Music's. Anyone who has the file, or the copied text, can use your Google account until that browser signs out. Using it is against YouTube's terms, and Google could flag the account.

**Signing out:** **Sign Out…** in Settings, or `signout ytmusic` in the CLI, deletes the file, and also ends the Google client's sign-in if there is one. Requiem can't sign your browser out: the copied cookie keeps working until you sign out of Google in that browser profile, which ends it. When Google ends a session, Requiem signs YouTube Music out and asks for a new one in Settings.

**From another computer:** over ssh, the CLI reads the paste from stdin with `-`, or from a file. Copy the headers on the computer with the browser, then either run this on the Mac and paste them in place of the placeholder line:

```sh
requiem signin-browser ytmusic - <<'EOF'
<paste the request headers here>
EOF
```

or save them to a file and send it from that computer, so it never lands on the Mac's disk:

```sh
ssh my-mac 'requiem signin-browser ytmusic -' < headers.txt
```

The quoted `'EOF'` keeps the shell from changing the paste. In the app bundle the CLI is `/Applications/Requiem.app/Contents/MacOS/requiem`. It returns once YouTube Music has taken the session; if YouTube Music refuses it, it prints why and exits 2. With `--json` it prints an `ack` or an `account_error`, as `signin-client` does. Delete the copied file afterwards.

## Spotify

Spotify is off until you sign in: `signin spotify` in the CLI, or **Sign in to Spotify** in the app. Enter the code it shows at <https://www.spotify.com/pair>. Playback needs Spotify Premium.

Requiem plays Spotify through [librespot](https://github.com/librespot-org/librespot), the way Spotify's own apps connect. Spotify doesn't allow unofficial players, so your Spotify account could be suspended.

Once signed in, search lists Spotify results first, Home shows your 20 most recently liked songs, the playlists Spotify made for you and your playlists, and Library shows Liked Songs and your playlists. The player keeps the sign-in in `spotify-credentials.json`, readable only by you, next to `state.json` (`REQUIEM_SPOTIFY_CREDENTIALS_FILE` overrides the path). `signout spotify` deletes it. Design: [`docs/plans/2026-10-16-spotify-provider-design.md`](docs/plans/2026-10-16-spotify-provider-design.md).

## Snapshots

`requiem-gui --demo --snapshot out.png --view <view>` renders a view from fixed fake data without showing a window: `home`, `home-signed-out`, `search`, `library`, `queue`, `settings`, `palette`, `collection`, `library-signed-out`, `library-unavailable`, `signin`, `signin-setup`, `settings-setup`, `settings-session`, `empty`, `disconnected`, `toast`, `out-of-date`, `update-ready`, `update-failed`, `not-updatable` or `up-to-date`. For `home` and `library`, `--snapshot` also works without `--demo`: it asks the running player (starting one if needed), waits up to 20 s for the page and its covers, then writes the PNG, exiting 1 if the page fails to load.

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

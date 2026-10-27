# The CLI

`requiem --connect`, then `help`. Commands: `search <query>`, `home`, `library`, `open <n>`, `play <n>`, `add <n>`, `play-track <json>`, `add-track <json>`, `queue`, `jump <n>`, `remove <n>`, `move <from> <to>`, `clear`, `next`, `prev`, `pause`, `resume`, `toggle`, `seek <secs|+secs|-secs>`, `vol <0-100>`, `status`, `signin`, `signout`, `account`, `update`, `update check`, `update install`, `shutdown-daemon`, `quit`. `home` and `library` print numbered items under each shelf, numbered across shelves; `open <n>` lists the tracks of playlist or album n. `play <n>`, `add <n>` and `open <n>` take n from whichever listing printed last: search results, a browse page or an opened playlist. `--script <file>` runs commands from a file; `--script -` reads them from stdin. `requiem update`, `requiem update check` and `requiem update install` run one update command against the player (see [Updates](updates.md)). `requiem signin-browser <provider> <file|->` signs the player in with a browser session (see [Sign in with your browser session](youtube-music.md#sign-in-with-your-browser-session)).

## JSON output

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


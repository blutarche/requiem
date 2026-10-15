# Spotify provider: design (2026-10-16)

Status: approved for build. The user chose "Try librespot playback" on 2026-10-15, accepting the account-ban risk, and "Both, Spotify first" for the UI on 2026-10-16. Spike notes: `2026-10-10-spotify-spike.md`.

## What the user sees

- **Search** shows Spotify results first and YouTube Music's below them, in one list. Each row shows its source.
- **Library** lists Spotify's Liked Songs and playlists first, then YouTube Music's.
- **Home** stays YouTube Music.
- Each track plays from the service it came from. Routing by `TrackId.provider` already does this.
- Spotify is off until the user signs in (`requiem signin spotify`, or Sign in to Spotify in the app). Sign-in shows one sentence of notice: "Spotify doesn't allow unofficial players, so your Spotify account could be suspended."

## Verified facts this rests on (live probe, 2026-10-15, user's Premium account)

- librespot dev rev `4a005903e` builds on macOS and Linux (docker, rust 1.96, `rustls-tls-webpki-roots`). crates.io 0.8.0 does not build (#1760).
- Device-authorization sign-in (spotify.com/pair with a code) works with the default desktop client ID and scope `streaming`. The stored `Credentials` JSON reconnects without a browser.
- Audio key per track takes ~70 ms. CDN fetch of OGG_VORBIS_320, decrypt, skipping 0xa7 bytes, symphonia decode and a seek to 60 s all work.
- Internal spclient APIs work:
  - `get_context("spotify:search:<q>")` returns 20 track URIs.
  - `get_context("spotify:user:<u>:collection")` returns the 247 liked tracks in one page.
  - `get_rootlist` returns the playlists as protobuf, which `librespot_metadata::playlist` parses.
- The public Web API (`api.spotify.com`) answers 429 on the first call. It is never used.
- Some tracks list no files at all (Bohemian Rhapsody `4u7EnebtmKWzUH433cf5Qv`). They play through `AudioItem.alternatives`, as librespot's player does in `find_available_alternative`.

## Architecture: a helper process, same binary

The release profile has `panic = "abort"` (Cargo.toml). An in-process librespot panic (`SessionWeak::upgrade`'s expect, unwraps in its HTTP client) would kill requiemd. librespot also needs tokio, and the daemon has no async runtime. So librespot runs in a **helper process**: the same `requiemd` binary started as `requiemd --spotify-helper <socket>`. One file to sign and package, the version always matches, and the bundle zip still updates atomically.

- **Daemon side.** `SpotifyProvider` implements `Provider` with plain blocking std I/O and never starts a runtime. It starts the helper lazily on the first call that needs it (search, browse, resolve, sign-in), not at daemon start.
- **Helper side.** It owns a 2-worker tokio runtime, the one `Session`, and every librespot object. It handles each connection on its own task.
- **Start-up.** requiemd checks `--spotify-helper` right after the self-test argument, before `Instance::lock()`, audio, persistence or the updater. Helper mode takes no instance lock.
- **Spawn.** The daemon spawns `ui::own_exe()` with:
  - stdin piped and held open by the daemon; the helper exits when stdin reads EOF, so it dies with the daemon;
  - stdout null;
  - stderr inherited, so it lands in daemon.log.
- **Ready check.** The daemon waits up to 5 s for the socket to accept connections. Otherwise it returns `Network("Spotify helper did not start")`.
- **Socket.** The path is `<daemon socket dir>/spotify.sock`; that dir is already 0700. The helper removes a stale file, binds, and sets the socket to 0600. It accepts only peers with its own uid (`getpeereid` on macOS, `SO_PEERCRED` on Linux).
- **Helper death.** If the helper dies, the next call respawns it, once per call. Reads in flight fail with an I/O error, so the engine reports a Stream failure and the session's existing retry and advance logic applies.
- **Logging.** The helper's logger is fixed at `warn` for librespot crates and ignores `RUST_LOG`. Our code never logs tokens, the credentials blob or audio keys.

### Wire protocol

There is one JSON request line per connection, answered by one JSON header line. `read` also has a body.

| op | request | reply |
|---|---|---|
| `search` | `{"op":"search","q":..,"limit":n}` | `{"tracks":[Track..]}` |
| `browse` | `{"op":"browse","page":Page}` | `{"shelves":[Shelf..]}` |
| `prepare` | `{"op":"prepare","track":"<base62>"}` | `{"stream":"<id>"}` |
| `attach` | `{"op":"attach","stream":"<id>"}` | `{"ok":true,"len":n}`, then the connection carries `read` lines |
| `read` (on an attached conn) | `{"op":"read","offset":o,"len":n}` | `{"ok":m}` then exactly m bytes (m < n only at EOF) |
| `sign_in` | `{"op":"sign_in"}` | one line per state: `{"pending":{url,code,expires_at_unix}}`, then `{"signed_in":{}}`, or an error line |
| `sign_out` | `{"op":"sign_out"}` | `{"ok":true}` |

- **Errors.** Any reply can instead be `{"err":{"kind":"network|unplayable|client_broken|parse|sign_in_required","msg":".."}}`, which maps 1:1 onto `ProviderError`.
- **Message types.** `Track`, `Shelf` and `Page` are the serde types from requiem-core.
- **`resolve(track)`** sends `prepare`. The helper:
  1. resolves the file and audio key;
  2. keeps `(file_id, key)` under a fresh stream id;
  3. returns the id.

  `prepare` opens no CDN connection, so the length is not known yet: only the CDN's first response gives the file size. The returned `ByteSource` holds only the socket path and stream id, and opens its connection with `attach` on the first read, `SeekFrom::End` or `byte_len()`; `attach` replies with the logical length. Only the decoder calls `byte_len()`, at playback, so a pre-resolved source still does no I/O. Seeking from the start or the current offset only moves the source's offset.
- **Attach failure.** The source keeps the error: `byte_len()` returns `None` and every read returns that error, without attaching again. The decoder then fails at probe, before it could seek without a length.
- **Attach and drop.** `attach` opens the `AudioFile` (the CDN fetch starts) wrapped in `AudioDecrypt`. Logical offset `o` reads decrypted offset `o + 0xa7`, and every seek becomes `SeekFrom::Start`. Closing the connection drops the file.
- **Prepared entries** expire 30 minutes after creation if never attached. Re-attaching an expired id fails with `Network`, so the session re-resolves.
- **Read timeout.** The daemon sets a 15 s read timeout on stream connections. librespot's own read blocks for up to 8 s.

### Spotify behaviour (helper)

- **Ids.** `ProviderId("spotify")`. Track ids are base62 ids. Collection ids are `collection` (Liked Songs) and `playlist:<base62>`.
- **Format.** OGG_VORBIS_320, else 160, else 96. If the track has none, try each alternative in order. If nothing has a file, return `Unplayable("no playable version of this track on Spotify")`. Unavailability (embargo, region) returns `Unplayable` with the reason. The hint is a new `FormatHint::OggVorbis`; requiem-audio enables symphonia's `ogg` and `vorbis` features and maps the hint to extension `ogg`.
- **Audio keys** (#1649 rate limits):
  - At most one key request in flight, at least 2 s apart.
  - A failed request retries after 3 s, then after 10 s, then gives up with `Unplayable("Spotify refused the audio key")`.
  - After 5 consecutive failed requests, the helper sends no key requests for 10 minutes and answers `Unplayable("Spotify is refusing audio keys; try again in a few minutes")`.
  - Keys are cached in memory per (track, file) for the helper's lifetime, up to 512 entries, so replays and re-resolves never ask again.
- **Search.** Run `get_context("spotify:search:<url-encoded q>")` and take the first `limit` track URIs. Fetch track metadata with at most 8 requests in flight, and keep the URI order.
  - `Track` fields: title, artist names, album name, duration, and artwork `https://i.scdn.co/image/<cover file id hex>` (the largest album cover).
  - Signed out → `SignInRequired`.
- **Library** (`browse(Library)`). One shelf titled "Spotify":
  - First a `Collection{kind: Playlist, title: "Liked Songs", id: "collection"}`.
  - Then one collection per rootlist playlist, in rootlist order, with title, subtitle "<n> songs" when known, and artwork when present.
  - Folders in the rootlist are flattened.
- **Collections** (`browse(Collection)`). Resolve the context (`spotify:user:<u>:collection` or `spotify:playlist:<id>`), following `next_page_url` pages. Return one shelf of tracks, fetching metadata as in search. Skip non-track items (episodes, local files).
- **Account** (`account()`, on the daemon side and non-blocking):
  - `SignedIn{name: None}` if the credentials file exists and parses.
  - `SignedOut` if it's missing.
  - `Unavailable(...)` if no file location can be worked out.
- **Sign-in.** The helper runs librespot-oauth's device flow (`DeviceAuthClientBuilder`, the same client ID and scope as the probe). It reports `Pending{verification_url: <url with code>, user_code, expires_at_unix}`, then connects a Session with the token and writes the reusable credentials.
- **Sign-out.** Drop the Session and delete the file.

### Credentials file

- **Location:** `spotify-credentials.json`, next to `google-token.json`, using the same `default_files` logic:
  - macOS: `~/Library/Application Support/requiem/`.
  - Linux: `$XDG_STATE_HOME/requiem/`.
- **Override:** `REQUIEM_SPOTIFY_CREDENTIALS_FILE`, which must be absolute.
- **Format:** librespot's `Credentials` serde JSON at rev 4a005903e. The file the probe wrote is already at the macOS path, and the provider must accept it as-is.
- **Writing:** create it 0600 (write a temp file 0600 in the same dir, then rename). Never print or log its contents.
- **Sign-out** deletes the local file only: librespot offers no way to revoke the sign-in on Spotify's side.
- **Why not the Keychain:** for the same reason as the Google token. The ad-hoc signed bundle would raise a Keychain prompt after every update.
- **Updates:** `update.rs` Restart to Update must save and restore this file alongside `token_file()`. The daemon self-test parses it when present.

## Multi-provider changes (core, daemon, CLI, Raycast)

- **Provider order.** The daemon builds `vec![spotify, ytmusic]`, and the order is the display order.
- **Home.** New trait method `fn has_home(&self) -> bool { false }`, which YTM overrides to `true`. `Page::Home` goes to the first provider with `has_home()`. Fix the doc comment at types.rs:62.
- **Search fans out** to every provider in parallel, each with `config.search_limit`, and waits for all of them. `SearchResults` carries the concatenation in provider order, so the event shape is unchanged and IPC needs no protocol bump.
  - A provider returning `SignInRequired` is skipped silently; other errors are logged.
  - `SearchFailed` only when no provider succeeded. It carries the first error that isn't a sign-in error, or a sign-in error if every provider needed sign-in.
- **Library fans out** to every provider whose account is `SignedIn` and concatenates shelves in provider order.
  - If none is signed in, send `BrowseFailed{sign_in_required: true}` as today.
  - If some succeed, return their shelves and log the failures.
- **Sign-in to an unknown provider id** emits `AccountError{provider, error: "no provider <id>"}` at once, instead of being ignored and leaving the CLI waiting 60 s.
- **CLI.**
  - `signin`/`signout` with no argument pick the first provider that isn't signed in (signin) or is signed in (signout).
  - Help lists `spotify` and `ytmusic` and drops the Google-only wording.
  - Text output of search rows gains the source name.
- **Raycast.** Copy says "Search music" instead of YouTube Music. Nothing else changes, because the merged search is still one event.

## GUI (separate task, after the core change)

- **Search table.** Add a narrow Source column showing "Spotify" or "YouTube Music". There are no section header rows, which keeps the row-to-result mapping 1:1.
- **Library list.** Merged; the details column says the source.
- **Accounts.** The GUI tracks every provider's account, not just the first:
  - Library fetch runs when any account is signed in.
  - The empty state offers one sign-in button per signed-out provider.
  - The menu and command palette get "Sign in to Spotify…"/"Sign out of Spotify" next to the Google items.
  - The pending panel names the service and, for Spotify, shows the notice sentence.
- **Strings.** `provider_name` maps `spotify` to "Spotify". The search placeholder says "Search".

## Risks

- **Ban and terms.** The user accepted the account risk. Shipping a librespot-based player in a public repo and public releases also exposes other users who sign in; the sign-in notice covers that. Whether distribution itself raises legal issues is a question for a lawyer, not settled here.
- **Spotify can break librespot at any time** (#1649 keys, #1737 client IDs). If it does, Spotify tracks fail as `Unplayable` and YouTube Music keeps working.
- **Pinned to a git revision.** A fix means bumping the pin and re-running the live test.

## Checks

- Unit tests:
  - the protocol framing, including `read` at EOF and short reads;
  - offset mapping across 0xa7;
  - the key-policy timing and circuit breaker, with a fake clock;
  - the merge order and failure rules in the session;
  - `has_home` routing;
  - the unknown-provider sign-in error;
  - that credentials files are created 0600.
- An ignored live test in the Spotify crate, run by hand with the real credentials file:
  1. search "daft punk";
  2. browse Library and Liked Songs;
  3. resolve one track plus Bohemian Rhapsody (the alternatives path);
  4. decode 10 s and seek to 60 s through `ByteSource`, with no audio output.
- A crash test: kill the helper during a read. The read must fail within the timeout, and the next call must respawn the helper.
- A Linux docker build and tests, as for the rest of the workspace.
- A security review of the credentials path and the socket before merge.

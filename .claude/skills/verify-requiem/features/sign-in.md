# Sign in

A new user signs in to Spotify with a pairing code they enter at spotify.com/pair, and to
YouTube Music with a Google device code. They can cancel either sign-in while its code is
showing and get back to the sign-in buttons.

## Sub-features

- `signin-spotify-start`: Sign in to Spotify shows a code, a link, the expiry and the suspension warning.
- `signin-cancel`: Cancel ends a pending sign-in, keeps the app running and shows the sign-in buttons again.
- `signin-restart`: a second sign-in after a cancel shows a new code.
- `signin-copy`: Copy Code puts the code on the clipboard.
- `signin-ytm-setup`: with no Google client file, `Sign in to YouTube Music` opens the setup sheet,
  and its Cancel closes it.
- `signin-ytm-import`: a client file imported from the CLI (`signin-client`) is saved and makes
  YouTube Music signed out, ready to sign in.

## How to get to it (user POV)

- Library, then the `Sign in to Spotify` button (or `Sign in to YouTube Music`).
- The command palette (⌘K), then `Sign in to Spotify…` or `Sign in to YouTube Music…`.
- The CLI: `requiem --connect`, then `signin spotify`, or `signin-client ytmusic <file>` for the
  YouTube Music client.

## Driving it with rqv

Preconditions:

- A fresh `rqv launch`, with `doctor: ok` showing Spotify signed out.

Steps:

- **Open Library.** Click Library in the sidebar: `rqv press "$RUN" '^\s*\d+ button Library$'`,
  then `rqv tree "$RUN" 01-library`. The tree shows `text Sign in to see your Library` and
  `button Sign in to Spotify`.
- **Start Spotify sign-in.** `rqv press "$RUN" 'button Sign in to Spotify$'`, wait 3 s, then
  `rqv tree "$RUN" 02-signin`. The tree shows `text Sign in to Spotify`, a line
  `On any device, go to spotify.com/pair?code=XXXXXX…`, `Code expires in 60 min`, and the buttons
  `Copy Code`, `Open in Browser` and `Cancel`.
- **Cancel.** `rqv press "$RUN" 'button Cancel$'`, wait 3 s, then run `rqv crashed "$RUN"`, which
  must print `running`. `rqv tree "$RUN" 03-cancelled` shows `button Sign in to Spotify` again,
  and `rqv cli "$RUN" account` shows `spotify: signed out`.
- **Sign in again.** Repeat the start step. A new code shows, then Cancel.
- **YouTube Music setup sheet.** In the Library, `rqv press "$RUN" 'button Sign in to YouTube Music$'`,
  then `rqv tree "$RUN" 05-setup`. The tree shows `text Set up YouTube Music sign-in`, the four
  steps, and the buttons `Choose Client File…`, `Setup Guide` and `Cancel`. Don't press
  `Choose Client File…` or `Setup Guide`: one opens a file panel, the other a browser. Press
  `rqv press "$RUN" 'button Cancel$'`, then `rqv crashed "$RUN"` prints `running` and
  `rqv tree "$RUN" 06-setup-cancelled` shows the sign-in buttons again.
- **Client import (CLI).** Write a fake client, never the real one:
  `printf '%s' '{"installed":{"client_id":"rqv-fake.apps.googleusercontent.com","client_secret":"rqv-fake"}}' > "$RUN/fake.json"`,
  then `rqv cli "$RUN" "signin-client ytmusic $RUN/fake.json"` prints `signed out`,
  `rqv cli "$RUN" account` shows `ytmusic: signed out`, and
  `$RUN/home/Library/Application Support/requiem/google-client.json` exists with mode `-rw-------`.
  A file that isn't a client prints `not a Google OAuth client`.
- **Palette entry.** `rqv key "$RUN" CmdOrCtrl+K`, then `rqv set "$RUN" 'text field .*Command' 'sign in'`,
  then `rqv tree "$RUN" 04-palette`. The list shows `button (selected) Sign in to Spotify…`, and
  it should also show `Sign in to YouTube Music…`. Close the palette with `rqv key "$RUN" Escape`.

## Gotchas

- 0.3.0 crashed on Cancel with `panicked at crates/requiem-gui/src/app/ui.rs:1327:28: RefCell
  already borrowed`. The Cancel step is the regression check for that crash.
- Cancel shows a `sign-in was cancelled` error toast. The user cancelled on purpose, so that toast
  is a defect, not the expected result.
- No Google client ships with Requiem. Each user imports their own "TVs and Limited Input
  devices" client, so a new Mac always starts at the setup sheet.
- After a fake client import, don't press `Sign in to YouTube Music`: it would send the fake
  client to Google. Stop the run instead.
- The CLI's `signin` exits 2 with `error: sign-in was cancelled` when a `signout` ends it. The
  window shows no error for the same cancel.
- Never enter a pairing code: that signs the throwaway copy into the user's real account.
- `orca` often reports `has no on-screen window` right after a click, even though the click
  landed. Trust the next `rqv tree`.

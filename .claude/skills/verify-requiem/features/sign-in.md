# Sign in

A new user signs in to Spotify with a pairing code they enter at spotify.com/pair, and to
YouTube Music with a Google device code. They can cancel either sign-in while its code is
showing and get back to the sign-in buttons.

## Sub-features

- `signin-spotify-start`: Sign in to Spotify shows a code, a link, the expiry and the suspension warning.
- `signin-cancel`: Cancel ends a pending sign-in, keeps the app running and shows the sign-in buttons again.
- `signin-restart`: a second sign-in after a cancel shows a new code.
- `signin-copy`: Copy Code puts the code on the clipboard.
- `signin-ytm-unavailable`: with no Google client file, YouTube Music sign-in must still be visible and say what's missing.

## How to get to it (user POV)

- Library, then the `Sign in to Spotify` button (or `Sign in to YouTube Music`).
- The command palette (⌘K), then `Sign in to Spotify…` or `Sign in to YouTube Music…`.
- The CLI: `requiem --connect`, then `signin spotify` (not driven here).

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
- **Palette entry.** `rqv key "$RUN" CmdOrCtrl+K`, then `rqv set "$RUN" 'text field .*Command' 'sign in'`,
  then `rqv tree "$RUN" 04-palette`. The list shows `button (selected) Sign in to Spotify…`, and
  it should also show `Sign in to YouTube Music…`. Close the palette with `rqv key "$RUN" Escape`.

## Gotchas

- 0.3.0 crashed on Cancel with `panicked at crates/requiem-gui/src/app/ui.rs:1327:28: RefCell
  already borrowed`. The Cancel step is the regression check for that crash.
- Cancel shows a `sign-in was cancelled` error toast. The user cancelled on purpose, so that toast
  is a defect, not the expected result.
- On a new Mac, the Library shows no YouTube Music button at all. YouTube Music sign-in needs
  `google-client.json`, which only exists on the Mac mini. The palette leaves it out too. The
  CLI's `account` command explains it; the window doesn't.
- Never enter a pairing code: that signs the throwaway copy into the user's real account.
- `orca` often reports `has no on-screen window` right after a click, even though the click
  landed. Trust the next `rqv tree`.

# Settings

Settings (⌘, or the sidebar's Settings button) lists the accounts. Spotify has Sign In… or
Sign Out…. YouTube Music can be signed in with a session pasted from a browser
(Use Browser Session…) or with the user's own Google Cloud client (Use Google Client…, typed or
chosen as a file).

## Sub-features

- `settings-open`: ⌘, and the sidebar button open Settings, with a row per service.
- `settings-session-form`: Use Browser Session… opens the steps, the risk disclosure and the
  buttons Paste from Clipboard, How to Copy and Cancel.
- `settings-session-refused`: an empty clipboard, a paste with no Cookie header and a well-formed
  fake session each show their own red error, and nothing is written to disk.
- `settings-session-keys`: ⌘V and Return paste, as Paste from Clipboard does.
- `settings-client-form`: Use Google Client… shows Client ID and Client secret fields, with Save
  disabled until both are filled, plus Choose Client File…, Setup Guide and Cancel.
- `settings-cancel`: Cancel closes either form and brings back the row's two buttons.
- `settings-cli-session`: `requiem signin-browser ytmusic <file|->` refuses a fake session with
  `account error: …` and exit 2.

## How to get to it (user POV)

- ⌘, from anywhere in the window, or the Settings button at the bottom of the sidebar.
- Library's `Sign in to YouTube Music` sheet also has Use Browser Session….
- The CLI: `requiem signin-browser ytmusic <file|->` reads the paste from a file or stdin.

## Driving it with rqv

Preconditions:

- A fresh `rqv launch`, with `doctor: ok`.
- Fake pastes only, such as a cURL line with `-b 'SID=fake; __Secure-3PAPISID=fake/AbC'`. Never a
  real cookie.

Steps:

- **Open.** `rqv key "$RUN" CmdOrCtrl+,`, then `rqv tree "$RUN" 01-settings`. The tree shows
  `group YouTube Music, Sign-in isn't set up` with `button Use Browser Session…` and
  `button Use Google Client…`.
- **Session form.** `rqv press "$RUN" 'button Use Browser Session'`, then tree. It shows the steps,
  the disclosure and `button Paste from Clipboard`.
- **Refused pastes.** Put fake text on the clipboard, press `button Paste from Clipboard` (or send
  ⌘V or Return), wait 8 s, then tree. Expect `text That has no Cookie header…` for a paste without
  a cookie, and `text Google didn't accept that session…` for a fake well-formed one. Then check
  that `$RUN/home/Library/Application Support/requiem/` has no `ytmusic-session.json`.
- **Client form.** `rqv press "$RUN" 'button Cancel$'`, then `button Use Google Client`, then tree.
  Expect `text field Client ID`, `text field Client secret` and `button (disabled) Save`.
- **CLI.** `rqv cli "$RUN" "signin-browser ytmusic /path/to/fake.txt"`, with the whole command as
  one argument because `rqv cli` sends one line per argument. Expect `account error: …` and exit 2.

## Gotchas

- The paste comes from the Mac's general clipboard, so writing fake text with `pbcopy` replaces
  whatever the user has copied. Ask first, or use a driver that gives the app a private
  pasteboard.
- A well-formed fake session is sent to music.youtube.com for checking. That is expected, and
  Google refuses it.
- Return is handled by the app's key monitor, which runs from `NSApp sendEvent:`. A driver that
  sends keys straight to the window misses it.
- Never press Sign Out… on a signed-in copy that holds a real account, and never paste a real
  session.

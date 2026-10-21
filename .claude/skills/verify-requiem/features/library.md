# Library

The Library lists the user's playlists and albums from every signed-in service, Spotify first.
Signed out, it shows a sign-in button per service that can sign in.

## Sub-features

- `library-signed-out`: the heading `Sign in to see your Library` and one button per service.
- `library-setup`: YouTube Music with no client still offers its button, which opens the setup
  sheet (driven in [sign-in.md](sign-in.md)).
- `library-list`: signed in, the playlists show with covers and song counts (checked read-only on
  the installed app).

## How to get to it (user POV)

- The sidebar's `Library` button, or ⌘3.

## Driving it with rqv

Preconditions:

- A fresh `rqv launch`.

Steps:

- **Open Library.** `rqv press "$RUN" '^\s*\d+ button Library$'`, then `rqv tree "$RUN" 01-library`.
  The tree shows `text Sign in to see your Library`, `text Your playlists and albums appear here.`
  and `button Sign in to Spotify` and `button Sign in to YouTube Music`.
- **Second view.** `rqv cli "$RUN" account` shows `spotify: signed out` and the YouTube Music
  `sign-in unavailable` reason.
- **Signed-in list (read-only, installed app).** `printf 'library\nawait results 60\n' |
  /Applications/Requiem.app/Contents/MacOS/requiem --connect --script -` lists the real rows.
  Never sign that app out.

## Gotchas

- Before 0.3.1 a new Mac showed no YouTube Music button at all. Its absence now is a regression.
- Signed-in Library rows can't be driven in the throwaway copy: that would need the real
  credentials, which never get copied.

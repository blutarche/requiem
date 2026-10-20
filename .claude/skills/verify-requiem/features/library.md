# Library

The Library lists the user's playlists and albums from every signed-in service, Spotify first.
Signed out, it shows a sign-in button per service that can sign in.

## Sub-features

- `library-signed-out`: the heading `Sign in to see your Library` and one button per service.
- `library-unavailable`: a service that can't sign in on this Mac must say why.
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
  and `button Sign in to Spotify`.
- **Second view.** `rqv cli "$RUN" account` shows `spotify: signed out` and the YouTube Music
  `sign-in unavailable` reason.
- **Signed-in list (read-only, installed app).** `printf 'library\nawait results 60\n' |
  /Applications/Requiem.app/Contents/MacOS/requiem --connect --script -` lists the real rows.
  Never sign that app out.

## Gotchas

- On a new Mac there's no YouTube Music button at all, and nothing tells the user why (see
  [sign-in.md](sign-in.md)).
- Signed-in Library rows can't be driven in the throwaway copy: that would need the real
  credentials, which never get copied.

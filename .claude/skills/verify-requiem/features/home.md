# Home

Home is the first page the window opens on. Signed in, it shows the user's own music merged
from every signed-in service, Spotify first: `Liked songs` (a track grid), `Made for you`
(Spotify's mixes) and `Your playlists` (cards). Shelves with the same title from both services
become one, with their items alternating. Signed out, Home shows a sign-in panel instead.

## Sub-features

- `home-signed-out`: the heading `Sign in to see your music`, the line
  `Search works without signing in.`, and one sign-in button per service the Library offers.
- `home-signin`: a Home sign-in button starts the same sign-in as the Library's.
- `home-load`: signed in, the shelves load with titles, cards and covers (checked read-only on
  the installed app).
- `home-page`: each shelf's `Show more` button pages it, and `Show previous` pages back.
- `home-open`: clicking a playlist card opens that collection.
- `home-refresh`: Home fetches again when an account signs in or out, and on ⌘R.

## How to get to it (user POV)

- The window opens on Home.
- The sidebar's `Home` button, or ⌘1.

## Driving it with rqv

Preconditions:

- A fresh `rqv launch`.

Steps:

- **Signed out.** `rqv tree "$RUN" 01-home`. The tree shows `text Sign in to see your music`,
  `text Search works without signing in.`, `button Sign in to Spotify` and
  `button Sign in to YouTube Music`.
- **Sign in from Home.** `rqv press "$RUN" 'button Sign in to Spotify$'`, wait 3 s, then
  `rqv tree "$RUN" 02-code`. The pairing code shows, as in [sign-in.md](sign-in.md). Press
  `Cancel`, then run `rqv crashed "$RUN"`, which prints `running`.
- **Signed-in shelves (read-only, installed app).**
  `printf 'home\nawait results 60\n' | /Applications/Requiem.app/Contents/MacOS/requiem --connect --script -`
  lists the real shelves. Report the shelf titles and counts only. Never sign that app out.

## Gotchas

- Signed-in Home can't be driven in the throwaway copy: that needs the real credentials, which
  never get copied. `home-page` and `home-open` therefore show only in `--demo`, so report them
  as skipped in the window, or drive them on the installed app if the user is fine with that.
- `Made for you` is every rootlist playlist whose id starts with `37i9dQZF1`, meaning the ones
  Spotify owns. That includes editorial playlists the user saved, such as `Sleep`, not just mixes.
- Pressing a track in `Liked songs` starts playback, which makes sound. Don't press one.
- Before 0.3.1, Home showed anonymous YouTube Music shelves (`Quick picks`,
  `Trending community playlists`). Those are gone on purpose.

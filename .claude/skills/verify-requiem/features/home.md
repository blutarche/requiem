# Home

Home is the first page the window opens on. It shows shelves of music: today these are the
anonymous YouTube Music shelves (Quick picks, Trending community playlists and others), with
cover art and ‹ › paging per shelf.

## Sub-features

- `home-load`: the shelves load, with their titles, cards and covers.
- `home-page`: each shelf's `Show more` button pages it, and `Show previous` pages back.
- `home-open`: clicking a playlist card opens that collection.
- `home-refresh`: ⌘R reloads Home.

## How to get to it (user POV)

- The window opens on Home.
- The sidebar's `Home` button, or ⌘1.

## Driving it with rqv

Preconditions:

- A fresh `rqv launch`. Home needs network access to YouTube Music.

Steps:

- **Load.** `rqv tree "$RUN" 01-home`. The tree shows `text Quick picks` and at least one more
  shelf title, each with `button Show more in <shelf>` and card buttons. The `.png` shows covers,
  not grey placeholders.
- **Page.** `rqv press "$RUN" 'button Show more in Quick picks$'`, then `rqv tree "$RUN" 02-paged`.
  `Show previous in Quick picks` is now enabled (no `(disabled)`).
- **Open a playlist.** `rqv press "$RUN" 'button .*views$'` clicks the first community playlist
  card. Then `rqv tree "$RUN" 03-collection` shows a collection with a track list and a
  `Play All`-style button. Don't press it.
- **Back.** `rqv key "$RUN" 'CmdOrCtrl+['`. Home shows again.

## Gotchas

- Home is anonymous: it isn't personal, even when signed in. The user has called these shelves
  useless ("trending community list? who cares"), and a Spotify + YouTube Music personal Home is
  planned.
- Pressing a track card in Quick picks starts playback, which makes sound. Don't press one.
- Covers load late. Wait 2–3 s before the `tree` that serves as proof of covers.

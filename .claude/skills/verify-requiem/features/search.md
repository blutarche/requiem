# Search

Search finds songs, albums, artists and playlists, and lists them with a Source column:
Spotify results first when Spotify is signed in, then YouTube Music. YouTube Music search works
while signed out.

## Sub-features

- `search-run`: typing a query and pressing Return lists results.
- `search-ytm-anon`: signed out, YouTube Music results still show.
- `search-focus`: ⌘F focuses the search field from anywhere.

## How to get to it (user POV)

- The sidebar's `Search` button, or ⌘2.
- ⌘F (Edit → Find).

## Driving it with rqv

Preconditions:

- A fresh `rqv launch` with network access.

Steps:

- **Open Search.** `rqv press "$RUN" '^\s*\d+ button Search$'`, then `rqv tree "$RUN" 01-search`.
  The tree shows a search field. Note its exact role and label in the tree.
- **Query.** Run `rqv set "$RUN" '<search field line regex>' 'daft punk'`, then
  `rqv key "$RUN" Return`. Wait 3 s, then `rqv tree "$RUN" 02-results`. Result rows mention
  Daft Punk, and their source is YouTube Music.
- **Crash check.** `rqv crashed "$RUN"` prints `running`.

## Gotchas

- Double-clicking a result, or pressing Return with a row selected, plays it, which makes sound.
  Select rows only through the tree.
- `set` writes the field's value through accessibility. If the app ignores that, focus the field
  with `rqv press`, then use `orca computer type-text --app "pid:$PID"`.

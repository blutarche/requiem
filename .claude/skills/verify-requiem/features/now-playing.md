# Now Playing

⌘L shows the Now Playing page: the current track's artwork, title and artists with the
transport, or an empty state when nothing plays.

## Sub-features

- `np-empty`: with nothing playing, the page shows `Nothing playing` and
  `Pick a song from Home or Search.`
- `np-track`: with a track loaded, the page shows its title and artists (needs playback; not
  driven).

## How to get to it (user POV)

- ⌘L, the palette's `Show now playing`, or a click on the player bar's track.

## Driving it with rqv

Preconditions:

- A fresh `rqv launch`.

Steps:

- **Empty state.** `rqv key "$RUN" CmdOrCtrl+L`, then `rqv tree "$RUN" 01-now-playing`. The tree
  shows `text Nothing playing` and `text Pick a song from Home or Search.`
- **Crash check.** `rqv crashed "$RUN"` prints `running`.

## Gotchas

- `np-track` needs a loaded track, and loading one plays it. Report it as skipped unless the user
  said sound is OK.

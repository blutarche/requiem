# Queue

The Queue holds what plays next. Users add tracks or whole collections from Home, Search or a
collection page with the "+" button or Add to Queue, then reorder, remove or clear them. The
queue is saved across restarts.

## Sub-features

- `queue-empty`: a new install shows an empty queue.
- `queue-add`: "+" on a row adds the track and confirms it.
- `queue-edit`: Move Up and Move Down (⌥⌘↑ and ⌥⌘↓), Remove, and Clear Queue.
- `queue-persist`: the queue survives quitting and relaunching the same run.

## How to get to it (user POV)

- The sidebar's `Queue` button, or ⌘4.
- The "+" on a track row in Search, a collection or Quick picks.

## Driving it with rqv

Preconditions:

- A fresh `rqv launch`, with search results showing (see [search.md](search.md)).

Steps:

- **Add.** Find the "+" button on a result row in `rqv tree` and `rqv press` it. The tree or
  toast confirms "Added to queue".
- **Add a whole playlist.** On a collection page (see [home.md](home.md)), use
  `rqv press "$RUN" '^\s*\d+ button Add to Queue$'`. `rqv cli "$RUN" status` shows
  `queue: <n> tracks, current none` and `state: stopped`.
- **Open the queue.** `rqv press "$RUN" '^\s*\d+ button Queue(, \d+)?$'`, then
  `rqv tree "$RUN" 02-queue`. The sidebar label carries the count (`Queue, 88`), and the page
  shows `<n> tracks` and the rows.
- **Clear.** `rqv key "$RUN" CmdOrCtrl+K`, then `rqv set "$RUN" 'text field .*Command' 'clear'`,
  then `rqv key "$RUN" Return`. The page shows `Queue is empty` and a `Queue cleared` toast with
  `Undo`, and `rqv cli "$RUN" status` shows `queue: 0 tracks`.

## Gotchas

- The `Queue cleared` toast and its `Undo` disappear after a few seconds. Press `Undo` in the
  same command as the clear, or the button is gone.
- `queue-persist` needs a quit and relaunch with the same `HOME`, which `rqv` can't do yet. Report
  it as skipped.
- Adding to an empty queue doesn't start playback. Any Play control does, so never press one.
- The queue is saved under `$RUN/home/Library/Application Support/requiem/state.json`. That file
  is the persistence evidence.

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
- **Open the queue.** `rqv press "$RUN" '^\s*\d+ button Queue$'`, then `rqv tree "$RUN" 02-queue`.
  The added track is listed.
- **Second view.** `rqv cli "$RUN" queue` lists the same track.
- **Clear.** Use Clear Queue from the Controls menu. `rqv cli "$RUN" queue` then shows 0 tracks.

## Gotchas

- Adding to an empty queue doesn't start playback. Any Play control does, so never press one.
- The queue is saved under `$RUN/home/Library/Application Support/requiem/state.json`. That file
  is the persistence evidence.

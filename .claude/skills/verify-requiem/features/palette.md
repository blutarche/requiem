# Command palette

⌘K opens a palette with a search field and a filtered list of actions: playback, the four views,
Now Playing, Refresh, volume, Clear queue, and Sign in or Sign out per service. Typing filters
the labels by substring. Return runs the selected action.

## Sub-features

- `palette-open`: ⌘K shows the field `Command` and the full list.
- `palette-filter`: typing narrows the list; the first match is selected.
- `palette-run`: Return runs the selected action and closes the palette.
- `palette-account`: Sign in and Sign out entries are listed only while they apply.
- `palette-close`: Escape closes it without running anything.

## How to get to it (user POV)

- ⌘K from anywhere in the window.

## Driving it with rqv

Preconditions:

- A fresh `rqv launch`.

Steps:

- **Open.** `rqv key "$RUN" CmdOrCtrl+K`, then `rqv tree "$RUN" 01-palette`. The tree shows a
  `text field` labelled `Command`.
- **Filter and run.** `rqv set "$RUN" 'text field .*Command' 'show library'`, then
  `rqv key "$RUN" Return`, then `rqv tree "$RUN" 02-ran`. `button (selected) Library` shows in the
  sidebar.
- **Clear queue.** See [queue.md](queue.md): the filter `clear` runs `Clear queue`.
- **Account entries.** With the filter `sign in`, the list shows `Sign in to Spotify…` and, on a
  build with the own-client setup, `Sign in to YouTube Music…`.
- **Close.** `rqv key "$RUN" Escape`. The `Command` field is gone from the next tree.

## Gotchas

- `Play/Pause`, `Next`, `Previous` and `Play selected` make sound. Never run them.
- Labels are in `crates/requiem-gui/src/vm.rs` (`PaletteAction::label`). Check there when a
  filter matches nothing.

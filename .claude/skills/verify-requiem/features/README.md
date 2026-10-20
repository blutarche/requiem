# Requiem verification map

This directory holds the recipes for checking what a Requiem user can do in the app window. Read
this index first, then follow the feature file that matches what you're checking.

## Baseline preconditions

- Build a bundle with `scripts/bundle-macos.sh`, or unzip a release zip with `ditto -x -k`.
- Start it with `rqv launch [<bundle>]` and keep the printed `RUN`.
- `rqv doctor "$RUN"` prints `doctor: ok`, and its accounts show `spotify: signed out` and
  `ytmusic: sign-in unavailable: no Google OAuth client…`.
- Never drive the installed `/Applications/Requiem.app`. It holds the user's real accounts.

## Driving conventions

- Start each recipe from a fresh `launch` unless its preconditions say otherwise.
- Match tree lines by role and label (`button Cancel$`), never by index or coordinates.
- Run `rqv tree "$RUN" NN-step` after each action, and `rqv crashed "$RUN"` whenever the window
  doesn't answer.
- Never complete a sign-in or press anything that plays sound.

## Proof and skip reporting

- UI proof is the `.png` and `.tree.txt` pair from `rqv tree`, taken after the action.
- State proof is a second view through `rqv cli "$RUN" account` (or `status`, `queue`).
- A crash is proof of a defect: keep `gui.log` and any `.ips` file from the evidence dir.
- Name every sub-feature or entry point you didn't drive, and say why.

## Features

- [sign-in.md](sign-in.md): sign in to Spotify or YouTube Music from a new install, and cancel.
- [home.md](home.md): the Home page's shelves and paging.
- [library.md](library.md): the signed-out Library and its sign-in buttons.
- [search.md](search.md): searching both services and reading the results.
- [queue.md](queue.md): adding to the queue and editing it, without playback.

Not mapped yet: the command palette (⌘K), Now Playing (⌘L), the menu-bar mini player, Raycast,
self-update and the CLI. Map them before calling a release fully checked.

## Feature entry contract

Each feature file starts with an H1 title and one paragraph describing the user-visible
behavior. Then come four H2 sections, in this order:

1. `Sub-features`
2. `How to get to it (user POV)`
3. `Driving it with rqv`
4. `Gotchas`

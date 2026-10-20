---
name: verify-requiem
description: Drive the real Requiem macOS app window as a fresh install (no accounts, no Google client file, empty queue) in a throwaway HOME and socket dir, and capture screenshots, accessibility trees and logs as proof. Use before any release, after any requiem-gui, daemon or provider change, and whenever a user reports something broken in the app. Fake-provider unit tests and offscreen --snapshot renders do not count as this check.
---

# Verify Requiem

Requiem is a native AppKit music player (`requiem-gui`) that talks over a Unix socket to a
background player (`requiemd`, inside the bundle at `Contents/Helpers/requiemd.app`). It has a
CLI (`requiem --connect`) and a menu-bar mini player. The window is the primary surface; that is
what users touch and what this skill drives.

`scripts/rqv` starts a bundle through Launch Services, as a double-click does. It gives the copy
its own `HOME` and `REQUIEM_SOCKET_DIR` under `/tmp/rqv.XXXXXX`, so the copy behaves like a
first launch on a new Mac:

- Spotify is signed out.
- YouTube Music sign-in is unavailable, because there's no `google-client.json`.
- Home shows the anonymous YouTube Music shelves.

It drives the window with `orca computer` against the copy's pid. It never reads or writes the
real app's files in `~/Library/Application Support/requiem`, and `doctor` and `stop` check their
modification times to prove that. The installed `/Applications/Requiem.app` and its daemon keep
running untouched.

Every `rqv` call needs the Bash sandbox off (`dangerouslyDisableSandbox: true`), because it uses
the window server, `open`, orca and Unix sockets. The Mac's screen must be unlocked.

## Launch

```bash
RQV=.claude/skills/verify-requiem/scripts/rqv
scripts/bundle-macos.sh                 # builds target/Requiem.app from the working tree (~5 min)
$RQV launch                             # or: $RQV launch /path/to/Requiem.app  (e.g. an unzipped release)
```

`launch` prints `RUN=/tmp/rqv.XXXXXX` and `pid=… version=… evidence=…`. It is ready once that
line is printed: the copy's window is on screen. Pass extra `requiem-gui` flags after `--`,
for example `$RQV launch -- --view library`.

To verify what users download, unzip the release zip with `ditto -x -k` and launch that bundle.
Don't stop at the working-tree build.

## Doctor

```bash
$RQV doctor "$RUN"
```

`doctor` is read-only. It reports:

- the window process;
- the build path and version;
- the copy's daemon socket;
- the account states, as the CLI sees them;
- `real app files: untouched`.

Anything other than `doctor: ok` means don't drive this copy. Stop it and launch a new one.

## Drive

Read the screen, then act on what it shows:

```bash
$RQV tree "$RUN" 01-library          # saves 01-library.png + .tree.txt; prints the tree
$RQV press "$RUN" 'button Sign in to Spotify$'
$RQV set "$RUN" 'search text field' 'daft punk'
$RQV key "$RUN" Return               # or CmdOrCtrl+K, CmdOrCtrl+3, Escape
$RQV crashed "$RUN"                  # "running" (exit 1) or why it died (exit 0)
$RQV cli "$RUN" status account       # requiem CLI against the copy's daemon
```

How the tree works:

- Tree lines look like `<index> <role> [(state)] <label>`, for example `14 button (selected) Library`.
- `press` and `set` take a regex for a tree line, not an index. Indexes change after every action.
- The sidebar's buttons are `Home`, `Search`, `Library` and `Queue`. Match them anchored, as in
  `'^\s*\d+ button Library$'`, because a shelf card can also contain the word "Library".
- Menu shortcuts work through `key`: ⌘1–⌘4 for the sidebar views, ⌘K for the palette, ⌘L for
  Now Playing, ⌘R to refresh, ⌘[ to go back.

After every action:

1. `tree` the new state.
2. Run `crashed` if anything looks off.

A crash shows up in the copy's `gui.log` as a Rust `panicked at <file>:<line>` line.

The feature map in `features/` is the list of what to drive. A release check covers every file
in it, through every entry point listed there.

## Evidence

Evidence lives in `$RQV_EVIDENCE/<run id>/`, which defaults to `$TMPDIR/requiem-verify/<run id>/`.
`stop` never deletes it. A run leaves these files:

- `NN-name.png` and `NN-name.tree.txt`: what the window showed and what it exposed, one pair per
  step;
- `gui.log`: the window's stdout and stderr, which hold panics, and with `REQUIEM_LOG=debug`, the
  backend lines;
- `daemon.log`: the copy's player log, copied at `stop`;
- `version`: the bundle version;
- any `requiem-gui*.ips` crash report written during the run.

Proof standards:

- Click what a user clicks: sidebar, buttons, keys. `--demo`, `--snapshot` and the CLI don't
  prove the window works. Use the CLI only as a second view of state, for example
  `rqv cli "$RUN" account` after a sign-in step.
- Capture the action and the state after it, not only the final screen.
- Check side effects too: account state, files under `$RUN/home/Library/Application Support/requiem`,
  and the daemon log.
- Report every feature or entry point you did not drive as skipped, with the reason. Never count
  it as covered through another path.

## Hard limits

- Never complete a real sign-in. Starting one only shows a pairing code, and Cancel ends it.
  Entering the code would sign the copy into the user's real account.
- Never copy the real `spotify-credentials.json`, `google-token.json` or `update-key` into a run.
  Signed-in features are checked read-only on the installed app, or not at all.
- No audio: don't press Play, a track, or Play All unless the user said sound is OK. Everything up
  to that click is fair game.
- By default the copy's update checks go to a dead URL (`http://127.0.0.1:9`), so it can never
  download or install an update. Set `RQV_UPDATES=1` to test real update checks.

## Cleanup

```bash
$RQV stop "$RUN"
```

`stop` ends only what this run started:

1. the copy's window process, by its recorded pid;
2. its daemon, through the copy's socket;
3. anything still holding the run's lock or socket, and that process's children.

Then it copies `daemon.log` into the evidence, re-checks the real app's files and deletes the run
dir. It never kills by process name. Run it after every attempt, including failed ones.

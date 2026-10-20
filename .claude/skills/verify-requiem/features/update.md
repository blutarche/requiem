# Self-update

The player checks GitHub's latest release once a day, and on Check for Updates… in the app menu.
It downloads a newer signed build, then the window shows `Requiem <version> is ready.` with
`Restart to Update`. With nothing newer, a manual check shows `Requiem <version> is up to date.`
for 4 seconds.

## Sub-features

- `update-check`: a check against the real release feed reports up to date or a newer version.
- `update-ready`: an older build finds the newer one and shows Restart to Update.
- `update-install`: Restart to Update replaces the bundle and relaunches (not driven).

## How to get to it (user POV)

- Requiem menu, then Check for Updates…, or the automatic daily check.
- The CLI: `update` shows the state, and `update check` checks now.

## Driving it with rqv

Preconditions:

- `RQV_UPDATES=1 rqv launch [<bundle>]`. Without `RQV_UPDATES=1` the copy's checks go to a dead
  URL on purpose.

Steps:

- **Check.** `rqv cli "$RUN" 'update check'`, then `rqv cli "$RUN" update`. The latest release
  prints `Requiem <version> is up to date.`
- **Older build.** Launch the previous release's zip the same way. `update` then reports the newer
  version downloading or ready, and `rqv tree "$RUN" 01-ready` shows the banner with
  `Restart to Update`. Don't press it.

## Gotchas

- The menu item can't be driven: System Events menu clicks don't reach the copy, because it
  shares its bundle id with the installed app. View > Library left Home selected. Report the
  window's Check for Updates… as skipped.
- The up-to-date banner lasts 4 seconds, which is shorter than one `rqv tree`.
- `update-install` would overwrite the bundle under test. Never press Restart to Update.

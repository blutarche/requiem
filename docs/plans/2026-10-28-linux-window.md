# Linux window (requiem-gui on GTK4)

Decided 2026-10-28: the Linux window is GTK4 through gtk4-rs. A measured spike on Fedora 44 KDE
(headless kwin, 1.5 scale, 300-row list with covers) put GTK4's default renderer at 42 MiB PSS
and 8% CPU while scrolling, with mature text input, clipboard, tables and AT-SPI. Slint's
software renderer was lighter (25 MiB PSS) but scrolled at 29% CPU and needs attribution;
iced had no accessibility; egui was no lighter than GTK. No libadwaita.

## Shape

- `crates/requiem-gui/src/vm.rs`, `demo.rs`, `args.rs` stay platform-neutral and shared. New
  `crates/requiem-gui/src/gtk/` (cfg linux) is the Linux front-end; `src/app/` stays macOS.
- The window is a thin client of `requiemd` over `requiem-ipc`, like the macOS window: it starts
  the player if needed, quits when its window closes, playback continues.
- GTK 4.8 is the floor (Debian 12's version, where release tarballs are built); no API newer
  than 4.8 without a runtime check.
- Application id `io.github.blutarche.Requiem`; a second launch raises the existing window.

## Done means

Feature parity with the macOS window's pages and keys where GTK allows, `--demo`/`--snapshot`
working headless for every view, the release tarball carrying `requiem-gui`, a `.desktop`
file and an icon, and README install steps for it.

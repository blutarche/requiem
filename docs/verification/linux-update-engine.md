# Linux install engine handoff

The engine is implemented in `requiem_update::linux` and connected through the
Linux daemon adapter. See `linux-self-update.md` for final integration evidence.
The phase-specific commands below document the engine tests.

Public API (all operations return `std::io::Result`, except path getters):

- `EXECUTABLES: [&str; 3]` lists requiem, requiemd, requiem-gui.
- `Installation::detect() -> Result<Installation>` uses current_exe.
- `Installation::from_executable(&Path) -> Result<Installation>` resolves user
  entry symlinks and pinned generation paths to the original sibling install.
  The handle holds a nonblocking exclusive directory flock until dropped.
- `Installation::recover(&self) -> Result<()>` restores a pending activation.
  Call immediately after detection, BEFORE loading persisted application state.
- `Installation::prepare(&self, archive: &Path, manifest: &linux_manifest::Manifest)
  -> Result<Generation>` checks metadata, exact compressed length and SHA256,
  extracts only three binaries into a fresh private generation, then executes
  each with --update-self-test and --version (exact `<name> <version>` output).
  Authenticate manifest bytes with linux_manifest::verify BEFORE calling this.
  Download transport remains the caller's responsibility.
- `Installation::installed(&self) -> Result<Generation>` copies the current three
  files into a pinned generation. Call after recover, before launching siblings.
- `Installation::activate(&self, &Generation) -> Result<()>` saves the old three,
  durably journals their generation, then replaces each entry using rename and
  directory fsync. Replacement errors trigger rollback. Success remains pending.
- `Installation::rollback(&self) -> Result<()>` restores the journaled three;
  idempotent, including interrupted rollback. Same operation as recover.
- `Installation::confirm(&self) -> Result<()>` durably removes the rollback journal.
  Call ONLY after the daemon has verified startup/IPC and saved-state handling.
- `Generation: Debug + Clone`; `cli()`, `daemon()`, `gui()` return PathBufs into
  that private generation. Fields are private. Generation storage is retained;
  this phase does not prune it.

Ordering: acquire handle, recover, restore any caller-owned pending saved state,
load state, prepare, persist/fsync saved state, activate, start/check pinned new
executables, confirm. On failed health: rollback and restore saved state before
restarting old executables. Drop the handle before a child tries to acquire it.
A child encountering a live lock must retry or report busy, never bypass recovery.

The engine replaces only three entry paths. Initial conversion is NOT atomic
across all three. Uncooperative/old launchers can observe mixed versions during
replacement. Every forthcoming CLI/daemon/GUI launch path needs the recovery gate;
existing processes must use pinned sibling paths. User symlinks pointing into a
regular side-by-side installation survive; symlink siblings inside the actual
installation are rejected. System prefixes, foreign ownership, shared writable
directories, unsafe private stores and read-only writes fail without sudo.

Limits: signed manifest compressed-size bound; at most 256 raw tar entries,
1 GiB declared ordinary-file data and 1 GiB decoded stream. Exact release root
is archive filename without .tar.gz. Raw entry iteration rejects extension
records, links, special files, duplicate paths and traversal. Only binaries and
known docs/desktop/icons are accepted; only binaries are written. No archive
permissions/ownership are restored.

Critical fresh-context self-review (cross-model unavailable per task): directory
selector migration would leave unconverted launchers unprotected. Chosen journaled
replacement requires explicit recovery and makes no atomic-migration claim.
Journal/backups are durable before the first rename; recovery never consumes
backups and clears the journal only after all three restoration fsyncs. The journal is written to a scratch file, fsynced, renamed and directory-fsynced
before replacement, so a crash during journal construction leaves entry files untouched. Pinned-generation detection
was corrected to lock the original install. tar was absent from Cargo.lock,
so it is a new direct dependency, with filetime/xattr transitive dependencies;
flate2 was already present. Library APIs checked against docs.rs tar 0.4.46 and
flate2 1.1.10. No generic tar unpack operation is used.

Verification uses shell executables in temporary installations, not release builds.
Tests execute all three new entry files, verify exact versions, confirm, roll back,
force replacement failure, reopen after partial replacement, preserve unrelated
files, and reject malicious archives, bad self-tests and wrong binary versions.
Recovery regression red-green: temporarily replacing recover with a no-op made
`cargo test -p requiem-update --lib linux::tests::executable_activation_rollback_and_interrupted_recovery`
exit 101 (requiem 3.0.0 vs expected 2.0.0). Restoring implementation passes.

Evidence logs (local): /tmp/requiem-linux-engine-red.log,
/tmp/requiem-linux-engine-macos-full.log,
/tmp/requiem-linux-engine-docker-full.log,
/tmp/requiem-linux-engine-entrypoints.log,
/tmp/requiem-linux-engine-linux-entrypoints.log.

Final checks, all exit 0 unless explicitly marked red:

```
cargo test -p requiem-update
# macOS: 64 passed; release CLI and doc tests also passed

docker run --rm -v /Users/blut/proj/requiem:/work -w /work \
  -e CARGO_BUILD_JOBS=2 -e CARGO_TARGET_DIR=/work/target/linux-update \
  requiem-gtk-dev cargo test -p requiem-update
# Linux: 53 passed; release CLI and doc tests also passed

cargo check -p requiem-cli -p requiem-daemon -p requiem-gui
# macOS: finished dev profile

docker run --rm -v /Users/blut/proj/requiem:/work -w /work \
  -e CARGO_BUILD_JOBS=2 -e CARGO_TARGET_DIR=/work/target/linux-update \
  requiem-gtk-dev cargo check -p requiem-cli -p requiem-daemon -p requiem-gui
# Linux: finished dev profile

cargo fmt --check -p requiem-update -p requiem-cli -p requiem-daemon -p requiem-gui
git diff --check
```

Skipped: release/dual-architecture builds (explicit task constraint), real daemon
handoff/GUI health and saved-state rollback (parent integration phase), full workspace
checks (verification scoped to updater and changed executable entry points).
Linux target/linux-update is retained. No commits, pushes, publication or version bump.

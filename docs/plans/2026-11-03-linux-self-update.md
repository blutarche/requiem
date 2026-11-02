# Linux Self-Update Implementation Plan

**Goal:** Signed Linux updates for user-writable three-binary tarball installations, preserving macOS clients.

**Architecture:** Separate architecture-specific signed Linux manifests preserve the macOS format-1 endpoint. A private version directory with a single atomic current selector is preferable to exchanging a shared bin directory. Initial migration must preserve existing executables until all stable entry paths are established; interrupted migration must recover before serving IPC. Never enable installation until migration, crash recovery, and saved-state rollback are verified.

**Tech stack:** Rust, Ed25519, SHA-256, gzip/tar, existing updater policy and IPC.

- [x] Add strict Linux manifest validation and release-tool generation/verification without changing macOS manifest output.
- [x] Extract signed archives into a new private directory with bounded output, exact executable names, no links, duplicate entries, special files, or traversal. Run all executable preflight checks before installation.
- [x] Implement private generation migration and atomic activation; persist and fsync a recovery journal and saved player/auth/update state before activation. Recover interrupted activation before state loading. Keep the previous generation until replacement IPC health succeeds.
- [x] Connect Linux daemon scheduling, persisted auto setting, idle/quit installation and GUI/CLI restart to existing IPC. Pin sibling executable discovery to one generation.
- [x] Verify signed two-version loopback updates in Docker, all three binary versions, CLI and GTK controls, invalid signatures/architectures/traversal, failed startup, and recovery. Record red-green commands for regressions.
- [x] Run macOS workspace build/tests, Linux build/tests, and release-tool verification; document exact evidence and all skips.

Critical self-review: an atomic selector alone does not make initial conversion of three regular files atomic. Launchers must gate on migration/recovery and select a complete generation. A running old daemon must not launch new-generation siblings before handoff. Saved-state recovery must precede deserialization by the replacement. These are release blockers. Cross-model design review was attempted by the parent and failed due to a Claude API rate limit; no cross-model review conclusion is available.

Completed integration evidence and verification limits: [linux-self-update.md](../verification/linux-self-update.md).

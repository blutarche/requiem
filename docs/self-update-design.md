# Self-update

Requiem installs from a disk image once. After that it keeps itself current: it never interrupts music to update, and nobody opens Terminal.

Requiem is ad hoc signed, not signed with an Apple Developer ID, and not notarized. The first install costs one **Open Anyway** in System Settings. Updates should cost nothing more: macOS checks downloads a person opens, and the updater writes its files without the quarantine attribute. Section 6 says what has been tested.

The model is Sensorium's (`blutarche/sensorium`, `docs/self-update-design.md`) and limusic's Tauri updater: a signed manifest on the project's GitHub release, and the app installs what it names.

---

## 1. What the person sees

### 1.1 First install

1. Download `Requiem-<version>.dmg` from https://github.com/blutarche/requiem/releases/latest.
2. Open it and drag Requiem onto the Applications shortcut.
3. Open Requiem. macOS refuses it. Click **Done**, then **Open Anyway** in System Settings → Privacy & Security.

Requiem must run from Applications, or from another folder this user can write to. If it runs from the disk image, or from the temporary copy macOS makes when an app is opened straight from Downloads, it cannot update and says so (section 1.3).

### 1.2 Updates

The player, `requiemd`, checks for updates. It runs while music plays or a Requiem window is open, and it quits on its own after five idle minutes. A found update downloads and is checked in the background. Then:

- **When the player next quits** (idle exit, **Quit and Stop Music**, or the menu-bar **Quit**), it installs the update on its way out and opens nothing. The next time Requiem opens, it is the new version. Most updates happen this way, unnoticed.
- **Restart to Update**, offered in the window and the menu-bar menu while an update is ready, installs at once. Music stops, both processes restart on the new version, and the queue comes back paused on the same track. The window reopens if it was open.

Requiem never installs while music plays unless the person chooses **Restart to Update**.

### 1.3 Lines in the window

One slim, non-error banner at a time, in the existing banner area:

| State | Banner |
| --- | --- |
| Ready | *Requiem `<version>` is ready.* **Restart to Update** |
| Failed | *Requiem couldn't update: `<reason>`. It keeps running `<version>`.* |
| Not updatable | *Move Requiem to Applications to get updates.* |
| Manual check, nothing new | *Requiem `<version>` is up to date.* (clears itself after a few seconds) |

A failed *check* (network down, a bad signature, the release page going backwards) shows no banner; only a failed *install* does. Both are logged and appear in `requiem update`.

Dev builds, which have no outer app, show nothing and never check.

### 1.4 Menus and settings

| Where | Item |
| --- | --- |
| App menu, below **About Requiem** | **Check for Updates…** and **Check for Updates Automatically** (a checkmark item, on by default) |
| Menu-bar menu | **Check for Updates…**; **Restart to Update (`<version>`)** while one is ready |
| CLI | `requiem update` prints the status. `requiem update check` checks now and prints the result. `requiem update install` restarts to update. Script mode gets the same three as `update`, `update check` and `update install`. |

With automatic checks off, Requiem makes no update request until **Check for Updates…** is chosen. **Check for Updates…** also clears retry counts, but not versions marked bad (section 3.6).

---

## 2. Release

### 2.1 Files

Each GitHub release `v<version>` carries:

| File | Contents |
| --- | --- |
| `Requiem-<version>.dmg` | The installer people download. The updater never uses it. |
| `Requiem-<version>.zip` | `Requiem.app` alone, made with `ditto -c -k --keepParent`. The updater downloads this. |
| `requiem-update.json` | The manifest below |
| `requiem-update.json.sig` | Base64 of the 64-byte Ed25519 signature over the manifest's exact bytes, on one line |
| `SHA256SUMS` | SHA-256 of the zip and the disk image, for people to compare |

```json
{
  "format": 1,
  "version": "0.2.0",
  "minimumMacOS": "26.0",
  "architectures": ["arm64"],
  "bundleIdentifier": "dev.blutarche.requiem",
  "file": "Requiem-0.2.0.zip",
  "size": 12345678,
  "sha256": "<64 lowercase hex characters>"
}
```

`minimumMacOS` is the bundle's `LSMinimumSystemVersion`; `scripts/bundle-macos.sh` raises it to what the embedded libopus needs. The build is arm64 only.

### 2.2 Release script

`scripts/release-macos.sh <version> [--publish]` runs on the maintainer's machine, never in CI, because CI would need the update key.

1. Refuse unless the working tree is clean and on `main`, `<version>` equals the workspace version in `Cargo.toml`, and tag `v<version>` exists neither locally nor on `origin`.
2. Build the bundle and the disk image with `scripts/make-dmg.sh`.
3. Zip `target/Requiem.app`, and check that the app in the disk image is the app in the zip.
4. `requiem-release manifest` writes the manifest. `requiem-release sign` signs it with the key file. `requiem-release verify` then checks the result exactly as an installed app would: the signature against the compiled-in public key, then sections 3.1 to 3.3 against the local files, self-tests included.
5. Write `SHA256SUMS`.
6. With `--publish`: tag `v<version>` and push the tag. `gh release create v<version> --draft`. Upload the disk image, the zip and `SHA256SUMS`, then the manifest and its signature. Then `gh release edit v<version> --draft=false --latest`.

The release stays a draft until every file is on it. `releases/latest` never points at a draft, so no app ever reads a manifest whose zip is missing.

---

## 3. How the player updates

All of it lives in a new crate, `requiem-update`, compiled for macOS only. Linux reports *Self-update is macOS only* and makes no request.

### 3.1 Check

1. Fetch `https://github.com/blutarche/requiem/releases/latest/download/requiem-update.json` and its `.sig`. GitHub documents this address as a stable link to the latest release's asset, and it does not count against the REST API rate limit. In debug builds only, `REQUIEM_UPDATE_BASE` replaces `https://github.com/blutarche/requiem/releases` for tests, and `REQUIEM_UPDATE_PUBKEY` replaces the compiled-in key. Release builds ignore both, so injected environment can't point a shipped app at old signed manifests.
2. Use a `ureq` agent: HTTPS only, redirects followed only to HTTPS, no cookies, a 30 s timeout, at most 64 KiB per file, and `User-Agent: Requiem-Update/1`.
3. Verify the signature with `ring` (`ED25519`, `UnparsedPublicKey`) against the 32-byte public key compiled into the crate. Stop on any failure.
4. Parse. Stop unless `format` is 1, `bundleIdentifier` is `dev.blutarche.requiem`, `architectures` contains this machine's, this macOS is at least `minimumMacOS`, and `file` is exactly `Requiem-<version>.zip`.
5. Compare `version` (semver) with the running version and with the highest version seen in any valid manifest, which is stored. The first matching row decides:

| `version` is | Result |
| --- | --- |
| Lower than the highest seen | A failed check: the release page went backwards |
| Lower than or equal to the running version | Up to date |
| Greater than the running version | An update |

The version is inside the signed bytes, so an old signed manifest served again can never install a release older than the running one. Without a server, two things can't be stopped. Whoever controls the release page can stop offering a newer release, or serve an older signed manifest that is still newer than the running version.

**When.** 30 s after the player starts, then hourly while it runs, a check runs only if the last successful check is at least 6 h old. The player restarts often, so this caps traffic at one check per 6 h per machine. **Check for Updates…** always checks.

### 3.2 Download

1. Build the address from the manifest: `<base>/download/v<version>/Requiem-<version>.zip`. Never follow an address taken whole from the manifest.
2. Stream to a temporary file in the staging folder (section 3.3). Stop past `size`, and refuse any `size` over 256 MiB. Timeout: 10 min.
3. Compare the file's SHA-256 (`ring::digest`) with the manifest's.

### 3.3 Stage

1. Get a staging folder on the installed app's volume from `NSFileManager` `URLForDirectory:NSItemReplacementDirectory inDomain:NSUserDomainMask appropriateForURL:<installed app> create:YES`. Set it to `0700`.
2. Unzip with `/usr/bin/ditto -x -k --noqtn --noacl <zip> <staging>/unzipped`.
3. `unzipped` must hold exactly one entry, `Requiem.app`: a real directory, not a symbolic link. No symbolic link inside it may resolve outside it. No regular file may have more than one link, and nothing may be a device, FIFO or socket.
4. The staged `Contents/Info.plist` has `CFBundleIdentifier` `dev.blutarche.requiem` and `CFBundleShortVersionString` equal to the manifest's `version`. The staged `Contents/Helpers/requiemd.app/Contents/Info.plist` has `CFBundleIdentifier` `dev.blutarche.requiem.player`.
5. `codesign --verify --strict` passes on each expected code item on its own: the app, `Contents/Helpers/requiemd.app`, `Contents/MacOS/requiem` and `Contents/Frameworks/libopus.0.dylib`. Then `--deep` runs once more over the whole app. The app is ad hoc signed, so this proves integrity, not origin. The origin comes from the manifest signature, which was checked against the zip's SHA-256 before anything was extracted (section 4).
6. Self-tests: the staged `requiemd --update-self-test` and `requiem-gui --update-self-test` each exit 0 within 10 s, and the staged `requiem --version` prints the manifest's version. `requiemd --update-self-test` reads, without changing, the state file and the Google token file when they exist, and fails if it can't parse them. It binds no socket, opens no audio device and writes nothing. `requiem-gui --update-self-test` exits 0 before creating the application. The point is that the binaries load, their frameworks resolve, and the new version reads the old version's files.

A staged update stays in memory as `Ready { version, staging }`. The staging path also goes into the update state, so a player that dies before installing deletes the stale folder at its next start. It deletes the folder only if, after `realpath`, it is still a real directory, not a symbolic link, owned by this user with mode `0700`, and inside the temporary-items folder that `NSItemReplacementDirectory` returns for that volume.

### 3.4 Install

The installed app is the `.app` that holds the running `requiemd` (the existing `gui_app` lookup). It is **not updatable** when there is no outer app (a dev build), its path contains `/AppTranslocation/`, its volume is read-only (`statfs` `MNT_RDONLY`, as when it runs from the disk image), or its parent folder is not writable by this user.

**Swap** (both modes):
1. Take an exclusive `flock` on the folder that holds the installed app, for example `/Applications`. Every Requiem player on the machine, for any user, takes the same lock, so two swaps never interleave.
2. Read the installed app's `CFBundleShortVersionString`. Swap only if the staged version is greater. Otherwise another user's player has already installed this version or a newer one: discard the staged copy and release the lock. This stops a stale download from downgrading the app.
3. Swap the staged app and the installed app with `renamex_np(staged, installed, RENAME_SWAP)`. The swap is atomic, so there is never a moment with no app installed, and the installed path keeps its name even if the person renamed it. The old app now sits in the staging folder.
4. Check the installed path again with section 3.3 steps 4 and 5. On any failure, swap back.
5. Release the lock. Register the installed app with LaunchServices (`LSRegisterURL`, forced), so LaunchServices sees the new version. Every launch in this section uses an exact path, never a bundle identifier.

**At exit.** When the player is about to exit for any reason and an update is `Ready`, it waits for `server.shutdown()` and the state writer to finish, then swaps. On success it deletes the staging folder, holding the old app. It opens nothing.

**Restart to Update.**
1. Note whether any GUI client is connected.
2. Stop serving the way **Quit and Stop Music** does: clients receive the goodbye, so the window quits. Close the listener but **keep the single-instance lock**. A client that tries to start a player meanwhile starts one that exits at once with `AlreadyRunning`, and the client keeps retrying as it already does. Wait for the state writer.
3. Copy the state file and the Google token file into the staging folder: `0600` files in the `0700` folder.
4. Swap.
5. Release the single-instance lock. Start the new player from the installed path, detached, the way the GUI starts it.
6. Within 30 s, connect as a CLI client. A `Welcome` must arrive whose `version` is the new version. Note the socket peer's pid (`LOCAL_PEERPID`): a client may have started the new player before this process did, so the peer pid, not the child pid, is the new player.
7. On success: if a GUI was connected, open the app at its installed path (the existing `launch_gui`). Delete the staging folder. Exit.
8. On failure: send the peer SIGTERM, then SIGKILL after 10 s if it is still running. Swap back, and copy the saved state and token files back in place, because the new version may have rewritten them. Record the failure (section 3.6), start the old player from the installed path, reopen the window if it was open, and exit.

Between the swap and its exit, the old process reads nothing from its bundle by path. Its code and libraries are already mapped, and the swap moves its files without deleting them. Sensorium's host runs the same way from a swapped bundle, through four real releases on macOS 26.6.2.

### 3.5 IPC

The protocol goes from 3 to 4. Window, CLI and player always ship together, and a protocol mismatch already produces the *older player is still running* path.

- `ClientMsg::CheckForUpdates`, `ClientMsg::InstallUpdate`, `ClientMsg::SetAutoUpdate(bool)`.
- `ServerMsg::Update(UpdateStatus)`, sent after `Welcome` and on every change. `UpdateStatus { current, auto, state }`, where `state` is one of `Idle`, `Checking`, `UpToDate`, `Downloading { version }`, `Ready { version }`, `Failed { version, reason }`, `NotUpdatable { reason }` and `Unsupported`.

### 3.6 Failures

| Kind | Failures | Then | Reason shown |
| --- | --- | --- | --- |
| Bad release | The download matched the signed manifest, then failed section 3.3 steps 3 to 6 | That version with that SHA-256 is never tried again; a release with a different zip is | *the new version is not built like this one* or *the new version did not start* |
| Could pass on another try | Network error or timeout, size or SHA-256 mismatch, a self-test timeout, a swap failure, or a restart handoff failure | Tried again at the next due check, up to three tries for that version; after the third it counts as bad | *the download did not match the release* or *the new version did not start in time* |

The *Failed* banner shows after a bad release or a third try. **Check for Updates…** clears retry counts and checks at once. It never clears a version marked bad, because the same zip would fail the same way; a re-published zip has a new SHA-256 and is tried.

### 3.7 State

`~/Library/Application Support/requiem/update-state.json`: mode `0600` in a `0700` folder, written atomically. It holds `format`, `auto`, `highestSeen`, `lastCheck` (Unix seconds), `staging` (a path or null) and `failures` (`version`, `sha256`, `tries`, `bad`).

---

## 4. Trust

An update installs only when the manifest's Ed25519 signature checks out against the compiled-in public key, and the zip's size and SHA-256 match the signed manifest.

| Secret | Kept | If lost |
| --- | --- | --- |
| Ed25519 update key: a 32-byte seed, base64 on one line | `~/.config/requiem/update-key` on the maintainer's Mac, mode `0600`, outside the repository, no passphrase | Installed apps accept no further update. Everyone installs once by hand a release carrying a new public key. |

Shipping an update takes the key file plus write access to the GitHub release. Sensorium also requires the new app to satisfy the running app's designated requirement, tied to a release certificate. Requiem doesn't, because:
- Requiem needs no TCC grant that a certificate would keep: no Screen Recording, no Accessibility, no Keychain items. Its Google token is a `0600` file.
- An ad hoc designated requirement is the code's own hash, so no new version could ever satisfy the old one's.
- Signing with a keychain identity on a Mac the maintainer reaches only remotely risks keychain prompts that no one sees.

A release certificate can be added later without changing the manifest.

`requiem-release keygen` makes the key. It refuses to overwrite an existing one, and prints the public key to paste into `crates/requiem-update/src/key.rs`. Key rotation is out of scope.

---

## 5. Privacy

The README gains: *The player fetches two small files from github.com at most once every six hours, to learn whether a newer release exists, and downloads it from GitHub when there is one. GitHub sees your IP address, the time and the file fetched. Nothing else is sent. Turning off Check for Updates Automatically stops these requests.*

---

## 6. Tested before building (2026-10-14, macOS 26.6.2)

A throwaway ad hoc signed app was installed in `/Applications`, with a helper app inside it under its own bundle identifier, like `requiemd.app`. The helper copied the outer app into a staging folder on the same volume, changed its version, re-signed it ad hoc, and swapped it with the installed app using `renamex_np(..., RENAME_SWAP)`.

| Helper launched | Result |
| --- | --- |
| As a child of the outer app, which was opened by LaunchServices (how the GUI starts `requiemd`) | Swap rc 0. The installed version became 2.0, `codesign --verify --strict --deep` passed, and nothing appeared in the TCC or syspolicyd logs. |
| Directly by LaunchServices, from the swapped bundle | Same result. The re-signed new version launched with no Gatekeeper prompt. |

Not yet tested: an app installed from a quarantined disk image and approved with **Open Anyway**, then updating itself. Sensorium tested that case with its throwaway app and it passed.

---

## 7. Cross-model review (2026-10-14)

A model from another family (codex, high reasoning effort) was given this design without other context and told to attack it. It said to reject it.

**Changed because of it:**
- `REQUIEM_UPDATE_BASE` works in debug builds only.
- One machine-wide `flock` around the swap, plus a staged-greater-than-installed check before swapping. This also fixes a downgrade found in our own review: two users, one stale download.
- The handoff keeps the single-instance lock through the swap, and identifies the new player by its socket peer pid.
- State and token files are saved before a restart and restored on rollback.
- The installed app is registered with LaunchServices after a swap, and every launch uses an exact path.
- A manual check no longer clears versions marked bad.
- Stale staging folders are checked for owner, mode and location before deletion.
- Each code item is verified on its own, not only with `--deep`.
- The Gatekeeper claim is now worded as untested for the quarantined case.

**Rejected, with the reason:**
- *Use Sparkle 2 instead.* Sparkle updates and relaunches the bundle it is hosted in. Requiem's long-lived process is a helper daemon inside the bundle, which Sparkle doesn't know about, so it would need the same daemon glue anyway, plus a prebuilt Objective-C framework with no Rust bindings. The user pointed at limusic and Sensorium; both use a custom signed manifest on GitHub releases. Sensorium's updater runs on this macOS version through real releases.
- *Developer ID and notarization.* They need a paid Apple account. Requiem needs no TCC grant that a stable identity would preserve (section 4).
- *Don't run staged binaries before the swap.* Their bytes already match the signed manifest. A malicious signed update would run with the same rights moments later anyway, so the self-test adds no exposure, and it catches broken bundles before they replace a working one.
- *Validate zip entries before extraction.* The zip's SHA-256 is checked against the signed manifest before extraction, so a hostile archive means the key is already lost. The post-extraction checks stay as defense in depth.
- *Use an external installer process outside the bundle.* The old process keeps its mapped code after the swap (section 3.4), and Sensorium's host runs the same way.
- *Journal every install phase.* A crash after the swap leaves a pre-verified new version installed. If that version starts, it is fine; if it can't start, nothing else could have run either.
- *Key rotation now.* It stays out of scope; losing the key costs one manual reinstall.

## 8. Out of scope

Linux self-update, Intel Macs, delta updates, beta channels, key rotation, and automatic moves to Applications.

## 9. Build order

1. The app icon, the disk image and the README install section.
2. `requiem-update`: manifest, signature, version policy, state, check, download, stage, swap. Plus the `requiem-release` binary (`keygen`, `manifest`, `sign`, `verify`). Unit tested against a local fake of the release page, with no network.
3. Integration: `--update-self-test` in `requiemd` and `requiem-gui`, the schedule, both install modes, IPC protocol 4, the CLI, the menu-bar menu and the window.
4. Generate the key, write `scripts/release-macos.sh`, publish `v0.2.0` and install it. Then publish `v0.2.1` and watch an installed `v0.2.0` update itself both ways: at exit and through **Restart to Update**.

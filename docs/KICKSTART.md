# requiem — Kickstart

Context for anyone (human or coding agent) picking up this project. Last updated: 2026-10-10.

## Why

The official Spotify desktop app routinely uses 400 MB – 1 GB+ of RAM because it ships its web UI inside Chromium. YouTube Music has no desktop app at all and needs a browser tab open. requiem is a music player you can leave running permanently with a near-zero footprint and a minimal-but-good UX.

## Locked decisions

| Decision | Choice |
|---|---|
| Distribution | Open source |
| Sources | Spotify **and** YouTube Music, behind a common provider abstraction |
| UI | **Native** toolkit — no Electron, no Tauri/webview. Minimal footprint is the point. |
| Platforms | Linux and macOS are primary. Windows is a later afterthought. |

## Proposed (not yet locked)

- **Rust core.** Both source libraries are Rust: [librespot](https://github.com/librespot-org/librespot) (Spotify) and [rustypipe](https://codeberg.org/ThetaDev/rustypipe) (YouTube / YouTube Music via Innertube).
- **Provider trait + one shared audio engine.** Each source implements something like `Provider { search, library, resolve_track -> AudioSource }`. All audio decodes through a single pipeline (e.g. symphonia + cpal) so mixed-source queues, gapless playback, and consistent volume work. Per-provider players are easier initially but harder to unify later.

Rough workspace layout to start from:

```
crates/
  requiem-core/       # domain types, queue, Provider trait
  requiem-audio/      # decode + output pipeline
  provider-spotify/   # librespot-backed
  provider-ytmusic/   # rustypipe-backed
  requiem-ui/         # native UI binary
```

## Open decisions

1. **UI toolkit.** Candidates:
   - **iced** (recommended so far) — MIT, Elm architecture, reactive rendering since 0.14 (near-zero idle CPU), tiny-skia software renderer avoids GPU memory overhead, used by COSMIC (strong Linux story). Cost: still "experimental", frequent breaking releases.
   - **Slint** — smallest footprint, stable 1.x API. UI written in `.slint` markup; open-source use is GPLv3 or royalty-free with a "Made with Slint" attribution.
   - **GPUI** (Zed) — excellent for dense, keyboard-driven UI with a command palette. Pre-1.0, thin docs, GPU-only.
2. **Spotify metadata path.** The official Web API in Development Mode is capped at 5 users per Client ID and requires Premium; Extended Quota needs a registered business and 250k MAU. Options: fetch metadata through the librespot session, or have each user supply their own Client ID.
3. **YouTube PO tokens.** rustypipe delegates PO-token generation to a separate `rustypipe-botguard` binary. Bundle it, or require users to install it?
4. **License.** Check the licenses of rustypipe, librespot (MIT), and any audio deps (e.g. libmpv if used) before choosing. A GPL dependency forces GPL.
5. **Performance budget.** Define concrete idle RAM / CPU targets. Reference point: ncspot reports roughly 22–46 MiB vs the official Spotify client's 400 MiB – 1 GB.

## Known constraints and risks

- **Spotify Premium is required** for any Spotify playback via librespot.
- **Neither source is officially supported.** librespot and Innertube are reverse-engineered. Using them likely violates Spotify's and YouTube's terms; document this in the README and let users opt in.
- **Breakage is routine.** Spotify backend changes have broken librespot before (e.g. Nov 2025, fixed in librespot 0.8.0). YouTube changes (signature ciphers, PO tokens) break stream extraction periodically. Budget for ongoing maintenance; pin and track upstream releases closely.

## Prior art

| Project | Source | UI | Notes |
|---|---|---|---|
| [ncspot](https://github.com/hrkfdn/ncspot) | Spotify | Terminal (ncurses) | Mature, librespot-based |
| [spotify-player](https://github.com/aome510/spotify-player) | Spotify | Terminal | librespot-based, lyrics |
| [Psst](https://github.com/jpochyla/psst) | Spotify | Native (Rust, Druid) | Early, unpolished |
| [Limusic](https://alternativeto.net/software/limusic/about/) | YouTube Music | Tauri + libmpv | Closest in spirit for YTM |
| SimpMusic Desktop | YouTube Music | Native | Alpha |
| ytmdesktop, th-ch/youtube-music | YouTube Music | Electron | Same resource problem requiem avoids |

**Gap requiem fills:** no lightweight native GUI client covers both Spotify and YouTube Music.

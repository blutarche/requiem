# requiem — Slice 1 design: playback core + YouTube Music

Date: 2026-10-10 · Status: draft for review, revised after the cross-model council · Sub-project 1 of 5

## Where this sits

requiem splits into five sub-projects, each with its own design → plan → build:

1. **Playback core + YouTube Music** (this doc)
2. Spotify provider (librespot) on the same core
3. Native GUI
4. Desktop integration (media keys, MPRIS, macOS Now Playing, tray)
5. Packaging and legal (installers, ToS opt-in notice)

## Goal and done check

A CLI that searches YouTube Music anonymously and plays tracks through requiem's own audio engine, on a core that the GUI can later drive unchanged.

Slice 1 is done when all of these hold on macOS (arm64):

- `cargo test --workspace` passes, including the out-of-order and failure-classification tests below.
- In a manual CLI session you can search, play, pause, resume, seek forward 60 s, seek back, go next, go prev, change volume and quit. Each step works and you can hear it, on both an Opus track and a forced AAC track.
- `scripts/perf-check.sh` passes on a release build. The gate is defined under Testing.
- The live smoke tests (`cargo test -- --ignored live_`) pass over both IPv4 and IPv6 where available. They are defined under Testing.

On Linux, the workspace builds and the non-live tests pass in a container. Linux playback is unverified unless Linux hardware is available.

Scope note: the YouTube path is verified only on the developer's network (one host, both address families). Regions, accounts and A/B buckets are untested. The live tests are the gate to re-run before every release.

## Non-goals (this slice)

- Login or user library.
- Gapless playback. Symphonia 0.6.1 marks MKV, MP4 and AAC gapless as "No", and the probes showed no `delay`/`padding`.
- Radio or auto-queue.
- Persistence: queue, volume and settings are lost on exit.
- The GUI and the daemon split.
- `rustypipe-botguard` and PO tokens. The chosen client needs no token, which closes kickstart open decision 3 for this slice.
- Loudness normalisation and crossfade.
- A second shipped `StreamClient` or a yt-dlp fallback (see Risks).

## Decisions

| # | Decision | Why |
|---|---|---|
| D1 | Own Innertube client; **no rustypipe** | rustypipe 0.11.4 stream URLs return 403 past byte 1,048,575, and its last release was 2025-04-23. It is GPL-3.0-only and the maintainer has not replied to issue #75. The user rejected it as stale. |
| D2 | License **MIT OR Apache-2.0** | With rustypipe dropped, nothing forces GPL. The remaining dependencies are MPL-2.0, Apache-2.0, MIT and BSD. |
| D3 | Search via `WEB_REMIX` client, playback via `VISIONOS` client | Both verified anonymously on 2026-10-10 over IPv4 and IPv6 (see Evidence). |
| D4 | visitorData taken from the search response, cached, and refreshed once whenever the player returns `LOGIN_REQUIRED` | The player call returns `LOGIN_REQUIRED` without it and `OK` with it, via either header or context field. There's no watch-page scrape and no cookies. |
| D5 | Stream resolution behind a `StreamClient` seam inside the YTM provider; only `VISIONOS` ships | The seam lowers the cost of the next fix. It does not shorten an outage: when YouTube kills the client, playback is down until users update. |
| D6 | Format preference: itag 251 → 250 → 249 (Opus/WebM) → 140 (AAC-LC/M4A) | Opus is higher quality at 48 kHz. 140 is the pure-Rust fallback, and both ranged to end of file with VISIONOS. HE-AAC (139) is skipped because symphonia can't decode it. |
| D7 | Engine: symphonia 0.6 + `symphonia-adapter-libopus` 0.3 + cpal 0.18, our own pipeline | It keeps a single symphonia version, lets us inject the Opus decoder, and leaves room for librespot PCM. Seeking was verified on real YouTube files (see Evidence). |
| D8 | libopus linked from the system (Homebrew `opus`, distro `libopus-dev`) | The bundled build needs cmake, which this machine lacks. A system libopus is present at `/opt/homebrew/opt/opus/lib`. Shipping a self-contained binary is sub-project 5's problem. |
| D9 | No async runtime: std threads, channels, and blocking HTTP (`ureq`) with explicit timeouts | The workload is few, sequential requests, and symphonia wants blocking `Read + Seek`. It is the smallest footprint. Workers that can't be cancelled are detached, and their results are dropped by generation checks. |
| D10 | In-process core behind a serde `Command` / `Event` boundary | Chosen by the user. The CLI and the future GUI link the core directly, and the same messages can cross IPC later. |

## Architecture

Crates, with dependencies pointing down:

```
requiem-cli ──► requiem-core ◄── requiem-audio
     │               ▲
     └──► requiem-provider-ytmusic
```

- **requiem-core.** Domain types, the `Provider` and `Engine` traits, the queue, the `Session` actor, and `Command`/`Event`. It has no audio, HTTP or symphonia dependencies.
- **requiem-audio.** Implements `Engine`: decode thread, resampler, ring buffer and cpal output.
- **requiem-provider-ytmusic.** Implements `Provider`: the Innertube client, the response parsers, and `HttpRangeSource`.
- **requiem-cli.** Line REPL over stdin. It wires a provider and the engine into a `Session`.

Threads at runtime:

```
CLI (stdin) ──Command──► Session actor ──Event──► subscribers (CLI printer; later GUI, MPRIS)
                             │   ▲
          worker threads ◄───┘   │  search/resolve results, stamped with a generation
                             │   │
           Engine::* (enqueue only) │ EngineEvent (sender given to the engine at construction)
                             ▼   │
                       Decode thread: probe, read, seek, refresh, resample ──f32 ring (~3 s)──► cpal callback
                             ▲                                                                (pop, atomic volume, silence on underrun)
                       HttpRangeSource (blocking ranged GETs, 512 KiB chunks, timeouts)
```

**Threading contract:**

- The Session actor never blocks on network or decode. Every `Engine` method only enqueues a message to the decode thread.
- The decode thread does all probing, reading, seeking and URL refreshing.
- The cpal callback only pops samples, applies the atomic volume, writes silence on underrun and bumps an atomic underrun counter. It never sends on a channel or takes a lock.
- The decode thread watches the underrun counter and emits `Buffering`.
- The URL-refresh path holds an `Arc` to the provider's Innertube client and never goes through the actor.
- **Quit:** `Shutdown` stops the engine and returns within about 1 s. Network workers are detached and die with the process.

## Components and interfaces

### requiem-core

```rust
pub struct ProviderId(pub String);                    // "ytmusic"
pub struct TrackId { pub provider: ProviderId, pub id: String }
pub struct Track { pub id: TrackId, pub title: String, pub artists: Vec<String>,
                   pub album: Option<String>, pub duration: Option<Duration>,
                   pub artwork_url: Option<String> }

/// Seekable bytes with a known length. Symphonia's mkv reader fails on truncated
/// WebM ("missing segment element"), so sources must be seekable and length-aware.
/// Must not do I/O until first read: pre-resolved sources hold no sockets.
pub trait ByteSource: Read + Seek + Send + Sync { fn byte_len(&self) -> Option<u64>; }

pub enum AudioSource {
    Encoded { bytes: Box<dyn ByteSource>, hint: FormatHint },   // FormatHint: WebmOpus | Mp4Aac
    // Slice 2 adds a push-model PCM variant; see "Slice 2 contract".
}

pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Track>, ProviderError>;
    fn resolve(&self, track: &TrackId) -> Result<AudioSource, ProviderError>;
}
pub enum ProviderError {
    Network(String),               // timeout, connect, DNS: retryable
    Unplayable(String),            // this track only: removed, private, region/age-restricted, live
    ClientBroken(String),          // the client path no longer works: needs a requiem update
    Parse(String),                 // response shape changed
}

pub trait Engine: Send {            // every method only enqueues; none blocks
    fn load(&mut self, r#gen: u64, src: AudioSource);  // `gen` is reserved in edition 2024
    fn pause(&mut self); fn resume(&mut self);
    fn seek(&mut self, to: Duration);
    fn set_volume(&mut self, v: f32);   // 0.0..=1.0
    fn stop(&mut self);
}
// Engine constructor takes an EngineEventSink = Arc<dyn Fn(EngineEvent) + Send + Sync>;
// every event carries the load generation.
pub enum EngineEvent { Started { gen, duration: Option<Duration> }, Position { gen, at: Duration },
                       Buffering { gen, on: bool }, Ended { gen },
                       Failed { gen, kind: FailureKind /* Stream | Decode | Output */, detail: String } }
```

`Command` and `Event` derive `Serialize` and `Deserialize`:

- **Commands:** `Search { req, query }`, `Enqueue { tracks, mode: Replace | Append }`, `PlayIndex(usize)`, `Next`, `Prev`, `Pause`, `Resume`, `TogglePause`, `Seek(Absolute(Duration) | Relative(i64 secs))`, `SetVolume(u8)`, `Shutdown`.
- **Events:** `SearchResults { req, tracks }`, `SearchFailed { req, error }`, `QueueChanged { tracks, current }`, `NowPlaying { track, duration }`, `Position(Duration)` (1 Hz while playing), `State(Playing | Paused | Buffering | Stopped)`, `Volume(u8)`, `TrackFailed { track, error }`, `ProviderBroken { provider, detail }`.

**Subscribers.** `Session::subscribe() -> Receiver<Event>` is a method, not a `Command`, because it hands back a channel that can't cross IPC; a future IPC layer subscribes on the client's behalf. Each subscriber gets its own channel. If a subscriber falls more than 64 events behind, its `Position` events are dropped; state events never are.

**Queue rules:**

- The queue is an ordered list plus a current index. `Enqueue Replace` replaces the list and clears the current index; the playing track keeps playing until it ends (then playback stops) or `PlayIndex` jumps to an entry.
- `QueueChanged` is emitted on every list change and whenever the current index moves. `Seek` emits no event; the next `Position` reflects it.
- `Next` past the end stops playback.
- `Prev` restarts the current track if more than 3 s have played or it is the first track; otherwise it goes to the previous track.
- When a track starts, the actor pre-resolves the next one. Only the URL and metadata are fetched, never audio bytes.

**Stale-result rule:**

- The actor keeps a `queue_gen` that bumps on every `Enqueue`, `PlayIndex`, `Next`, `Prev` and `Stop`.
- Every resolve, including pre-resolve, is stamped `(queue_gen, index, track_id)`. The actor drops any result whose stamp no longer matches.
- Every `Engine::load` carries a load generation, and the actor ignores `EngineEvent`s from older loads. In particular, an old `Ended` never advances the queue.
- A failed pre-resolve isn't a strike against the track. That track is resolved again when it actually starts.
- Strikes count consecutive failures of tracks whose start was attempted, whether the resolve or the engine failed. They reset when a track starts or playback stops.

### requiem-provider-ytmusic

- **`InnertubeClient`** (ureq, rustls; connect timeout 5 s, read timeout 10 s).
  - `search_tracks(query)` POSTs `music.youtube.com/youtubei/v1/search` as `WEB_REMIX` (client id 67), using the songs filter `params`, and caches `responseContext.visitorData`.
  - `player(video_id)` POSTs `www.youtube.com/youtubei/v1/player` through the active `StreamClient`.
  - If no visitorData is cached, it first runs a one-off search.
  - On `LOGIN_REQUIRED`, it drops the cached visitorData, fetches a fresh one, rebuilds the HTTP agent so a new connection pool picks up a changed source address, and retries once.
- **`StreamClient` trait.** Holds the client name, numeric id, version, user agent and device fields, and builds the player request. `VisionOs` is the only implementation. Client constants live in one file, with a comment pointing at yt-dlp `extractor/youtube/_base.py` `INNERTUBE_CLIENTS` as the upstream reference.
- **Parsers.** They use typed serde structs with every field optional, so a missing field never panics.
  - Search walks `musicResponsiveListItemRenderer` to extract videoId, title, artists, album, duration and thumbnail.
  - Player reads `playabilityStatus` and `streamingData.adaptiveFormats`, which carry itag, url, contentLength and mimeType. It also reads `expiresInSeconds`.
- **Playability classification.** Fixtures cover every row:

| Player response | Result |
|---|---|
| `OK` with a usable audio format | resolved |
| `UNPLAYABLE`, `ERROR`, `AGE_CHECK_REQUIRED`, `CONTENT_CHECK_REQUIRED`, `LIVE_STREAM_OFFLINE`, or `OK` with no audio formats for this video | `Unplayable` |
| `LOGIN_REQUIRED` still present after the visitorData refresh and retry | `ClientBroken` |
| `OK` but formats only via `signatureCipher`/SABR with no `url` | `ClientBroken` (the client changed shape) |
| Top-level structure missing, or the JSON doesn't parse | `Parse` |
| Unknown status string | `Unplayable`, logged with the status |

- **`HttpRangeSource: ByteSource`.**
  - Lazy: no I/O until the first read. Content length comes from the player response.
  - Ranged GETs fetch 512 KiB chunks. That size is chosen for memory; the rustypipe cap was a byte offset, not a range size.
  - Chunk 0, the container header, is pinned. A 2-chunk LRU serves the playhead and short back-seeks, for at most 1.5 MiB cached.
  - **Range invariants:** a partial read must be `206` with a `Content-Range` whose start, end and total match the request and the known length, and a body of exactly that length. A `200` to a ranged request, or any mismatch, is an error.
  - **Transient errors** (timeout, connection reset): retry the same range once after 1 s.
  - **403 or 410:** re-resolve through the refresh path (URLs expire and embed the client IP, which changes after sleep or IPv6 address rotation). At most 3 refreshes per track, at least 10 s apart.
  - **Continuity:** if the refreshed format has the same itag and contentLength, resume at the same byte offset. Otherwise the read fails and so does the track (v0.1: reopening and seeking by time needs engine support and is deferred).
  - A 403 on the first byte of a freshly resolved URL, after one retry, is `ClientBroken`.
- **Resolve flow.** Run the player call, classify it, pick a format by D6, and return a lazy `AudioSource::Encoded`.

### requiem-audio

- **Decode thread.**
  - Receives `load`, `seek`, `pause`, `resume` and `stop` messages.
  - Probes the source with the `FormatHint` and builds a decoder from a `CodecRegistry` with symphonia's defaults (`mkv`, `aac`, `isomp4`) plus the registered `OpusDecoder`.
  - Converts samples to interleaved f32 stereo; mono is duplicated and anything above stereo is downmixed.
  - Resamples only when the source rate differs from the device rate (itag 251 at 48 kHz usually matches). The resampler crate is chosen before the perf gate exists.
  - Pushes into an SPSC ring buffer of about 3 s.
  - Recoverable `DecodeError`s skip the packet; over 50 in a row fails the track. I/O errors that survive the range source's retry and refresh fail the track.
- **Seek.** Flush the ring, call the format reader's `seek(Accurate, Time)`, and reset the decoder.
- **Output.** A cpal default output stream with the device's default config, honouring the realtime-safe rule in the threading contract. If the stream errors, the default device is reopened once. When the new config differs (rate, channels or sample format), the resampler and ring are rebuilt. If that fails, the track fails and the state goes to `Stopped`.
- **Output seam.** The cpal stream sits behind an internal `Output` trait so tests can capture samples with no device.

### Slice 2 contract (librespot), recorded now so slice 1 doesn't block it

librespot's `Player` pushes PCM into a `Sink` (`write(AudioPacket::Samples(Vec<f64>))`, 44.1 kHz stereo) and owns its own transport. It can't be adapted to a pull `ByteSource`: a paused reader would block `write` and deadlock the next seek. Slice 2 therefore adds a PCM variant that carries:

- the sample stream, pushed into the engine's ring and resampled to the device rate;
- a transport handle that the engine calls for pause, resume, seek and stop, so pausing stalls librespot itself rather than blocking its `write`.

`Command`, `Event` and the Session do not change. Only `AudioSource` gains a variant, and the engine gains a second input path.

### requiem-cli

A stdin line REPL with these commands:

- `search <q>` prints numbered results, and the CLI keeps only the newest search: a `SearchResults` whose `req` isn't the latest is ignored.
- `play <n>` sends `Enqueue { tracks: vec![results[n]], mode: Replace }` then `PlayIndex(0)`. `add <n>` sends `Enqueue { tracks: vec![results[n]], mode: Append }`. Both are rejected until the latest search has returned.
- `queue`, `next`, `prev`.
- `pause`, `resume`, `toggle`.
- `seek <secs|+secs|-secs>`, `vol <0-100>`, `status`, `quit`.

Events print as one line each, except `Position`, which only shows in `status`. `--script <file>` feeds commands non-interactively for the perf check, and `--prefer-aac` forces itag 140 for the manual check. Logging goes to stderr and is off unless `REQUIEM_LOG=debug`.

## Error handling

| Failure | Behaviour |
|---|---|
| Search network error or timeout | `SearchFailed`. Nothing else changes. |
| Resolve `Network` | Retry once after 1 s. If it still fails, emit `TrackFailed` and skip. |
| Resolve `Unplayable` | `TrackFailed`, skip to the next track. |
| Resolve `ClientBroken` | `ProviderBroken { detail }`, which the CLI prints as "YouTube changed something — requiem needs an update", then stop. |
| Resolve `Parse` | The first time, `TrackFailed` and skip, because one video can have an odd response shape. A second `Parse` in a row on a different track becomes `ProviderBroken`, then stop. |
| Mid-stream 403/410 | Refresh per the range-source rules. Once the budget is exhausted, `TrackFailed` and skip. |
| 3 consecutive started tracks fail | Stop. If at least two of the failures were refresh or 403 failures, emit `ProviderBroken` rather than three plain skips. |
| Output device lost | Reopen once, rebuilding the graph when the config changed. If that fails, `Failed` and `State(Stopped)`. |
| Decode errors | Skip the packet. Over 50 in a row fail the track. |

## Testing

- **Core.** Tests for the Session and queue state machine use a `FakeProvider` and a `FakeEngine`, and drive commands and events deterministically. They cover:
  - queue rules and prev's 3 s rule;
  - failure policies and strike counting (pre-resolve failures don't count);
  - **out-of-order completions:** a delayed resolve or pre-resolve landing after `Next`, `Prev` or `Replace` must be dropped;
  - an old `Ended` that must not advance the queue;
  - search results arriving out of order.
- **Audio.**
  - Decode fixtures: short sine tones in Opus/WebM and AAC/M4A, generated with ffmpeg and committed. Being generated, they carry no copyright concerns.
  - Through the capture `Output`, tests assert duration, sample rate, seek accuracy, resampled length, and the rebuild when the device config changes.
- **YTM parsers.** Run against recorded `search` and `player` JSON fixtures, with one fixture per playability-classification row. Fixtures must be scrubbed: player URLs embed the client IP (`ip=`) and visitorData.
- **HttpRangeSource.** Tested against an in-test local HTTP server that exercises:
  - 206 ranges;
  - 403 past an offset (the refresh path, with the same and with a changed contentLength);
  - `200` to a ranged request;
  - a bad `Content-Range`;
  - a short body;
  - a stalled connection (timeout, then retry).
- **Live smoke tests** are `#[ignore]` and named `live_*`. They:
  - run a real search and resolve;
  - read the chosen format end to end in 512 KiB ranges, recording time to first byte and asserting sustained throughput of at least 4× the itag's bitrate, so a throttled URL that still returns `206` fails;
  - run one mid-track seek;
  - repeat for itag 140;
  - run on IPv4 and IPv6 when both are routable.

  They are the early warning for YouTube breakage and are run before every release.
- **Perf gate.** `scripts/perf-check.sh` builds a release binary and drives the CLI with `--script` through these phases:
  1. idle after a search;
  2. playback for 60 s;
  3. three seeks;
  4. one track transition;
  5. idle after the queue ends.

  It samples physical footprint every 500 ms (macOS `proc_pid_rusage` `ri_phys_footprint`, Linux `Pss` from `smaps_rollup`; the cap is 30720 KiB) and reports `ps` RSS as information only: on macOS `ps` RSS includes about 10–12 MiB of shared system-framework pages that requiem cannot shrink (user decision, 2026-10-10). CPU comes from the process's CPU-time delta per phase, not from averaged `%cpu` samples. It fails if peak footprint exceeds 30 MiB in any phase or CPU reaches 1% in any playback phase. The 30 MiB budget is the user's call: the plan measures a baseline early, and if the baseline breaks the budget, that goes back to the user rather than the number moving silently.

## Risks

- **The YouTube client path dies.** This is the main risk, and no shipped mitigation removes the outage.
  - NewPipe issue #13824 (open since 2026-09-14) reports VISIONOS stream 403s around 55–60 s into playback on some devices and IPs. That didn't reproduce here: the same URL returned 206 at 0, 70, 131 and 251 s.
  - The refresh path covers expiry and address changes; the live tests detect breakage; the fix is to port the next working client from yt-dlp.
  - A yt-dlp fallback `StreamClient` stays deferred. It would add a Python and JS runtime and a process spawn per play, against the footprint goal.
- **Probes ran on one host** (IPv4 and IPv6), on 2026-10-10. Behaviour varies by IP, region and A/B bucket.
- **Seek cost on AAC grows with distance.** It walks MP4 fragments: up to 6 chunks for a 380 s seek. That is about 6 sequential range requests over HTTP, still unmeasured on the network. Opus/WebM seeks touch at most 2 chunks.
- **Perf budget unmeasured.** 30 MiB sits at the low end of ncspot's 22–46 MiB. The plan puts a baseline measurement early.
- **Terms of service.** This is an unofficial client that skips ads, which violates YouTube's terms. The README needs to say so (small task in this slice). The full opt-in notice belongs to sub-project 5.

## Alternatives considered

1. **rustypipe for metadata and streams.** Rejected. Its streams stop at 1 MiB today (`403` past byte 1,048,575 on every client tried, including the dev branch with botguard). It is stale and GPL.
2. **rustypipe for metadata, native streams.** Rejected by the user over staleness. It would also force GPL.
3. **yt-dlp subprocess for stream resolution.** Deferred as a future `StreamClient` fallback. Upstream fixes breakage fastest, but it adds a Python (and maybe deno) runtime dependency, a process spawn on the play path, and resolve latency that is unmeasured.
4. **libmpv as the engine**, with its ytdl hook. Rejected:
   - It needs a C dependency bundled per platform, and whether the result is GPL or LGPL depends on how it was built.
   - It owns the clock, the device and seeking ("there is no such thing as audio output embedding"), so librespot PCM could only enter as a wrapped byte stream.
5. **rodio.** Rejected. It pins symphonia 0.5 and hard-codes its codec registry, so the Opus adapter can't be injected.
6. **GStreamer.** Rejected. It is a heavy runtime to bundle on macOS and needs MSRV 1.92.
7. **Spotify first.** The user chose YouTube Music first. Spotify's PCM `Sink` path is confirmed from the librespot 0.8.0 source; see the Slice 2 contract.

## Plan-stage spikes

These open items are settled during planning, not here:

- The `ureq` major version, its timeouts, and its TLS root setup.
- The resampler crate.
- The SPSC ring buffer crate.
- How `opusic-sys` finds the system libopus: `OPUS_LIB_DIR` or pkg-config. pkg-config is not installed on this machine.
- **An early release-build baseline** of RSS and CPU for decode plus output on the local fixtures, before the full pipeline exists.
- AAC seek latency over real HTTP ranges.
- How to run the Linux container build.

## Council review (2026-10-10)

Both reviews were run blind on the first draft: Codex (`/codex:adversarial-review`) and Grok 4.7 high (via Cursor).

- **Codex:** needs-attention.
- **Grok:** build option (a) and don't reopen the alternatives, but change the design before planning.

Both raised the same three gaps: per-track vs client-wide error classification, a generation guard on resolves, and seek as an untested done criterion. Each was checked against the draft and was real.

**What changed:**

- Playability classification and `ClientBroken` vs `Unplayable`. Strikes now count only tracks that actually started.
- `queue_gen` and load-generation stamps; stale results and events are dropped, with out-of-order tests.
- A non-blocking `Engine`, an event sender at construction, a realtime-safe callback, and a refresh path that bypasses the actor.
- visitorData refresh on `LOGIN_REQUIRED`, a 403 refresh budget, range invariants, a continuity rule, timeouts and retry.
- Lazy `ByteSource`, plus a written Slice 2 PCM contract.
- CLI result binding to the latest `req`, and an exact `play`/`add` payload.
- A phased perf gate with CPU-time accounting and the KiB unit written down.
- Live tests with a throughput floor, a seek, both itags, and both address families.
- A pinned header chunk, `artwork_url`, multiple subscribers, and a graph rebuild on device change.
- The D5 wording fixed: the seam lowers fix cost, not outage length.

**What was rejected, with evidence:**

- **Grok: "native VISIONOS URLs are likely throttled below realtime; yt-dlp's full download only worked because it rewrites the `n` parameter."** The VISIONOS URLs carry no `n` parameter. A sequential 512 KiB ranged walk of whole tracks ran at 470–525× realtime over IPv4 and IPv6. The throughput floor in the live test was kept anyway.
- **Grok: "the 2-chunk LRU makes every seek re-fetch the WebM cue index."** Measured WebM seeks touched at most 2 chunks in total; the demuxer keeps the index in memory. Chunk 0 is pinned anyway, because it's cheap.
- **Grok: "the gapless non-goal has no evidence."** Symphonia 0.6.1's README marks MKV, MP4 and AAC gapless as "No", and the probes showed no delay or padding.
- **Grok: "change `resolve` to return a description plus a push engine API now."** Only partly adopted. Sources are now lazy and the Slice 2 contract is written down; building the push path now is deferred as unused code.
- **Codex: "ship a second StreamClient or a yt-dlp fallback in slice 1."** Deferred. An unverified fallback adds runtime dependencies against the footprint goal. Instead, slice 1 is scoped to the verified environment and the live tests gate each release.
- **Grok: "set the RSS cap from the baseline."** The cap is the user's decision. The baseline is measured early, and a miss goes back to the user.

## Evidence (2026-10-10, macOS arm64, one residential connection, anonymous)

- **rustypipe 0.11.4**, iOS client: ranges at or below byte 1,048,575 return 206; any range past it returns 403, and a no-range GET returns 403. Reproduced twice, by the research agent and independently.
- **yt-dlp 2026.08.19**, default `visionos` client: downloaded all 8,859,337 B of itag 251.
- **Native `WEB_REMIX` search** (`clientVersion 1.20260707.12.00`, songs filter): 20 tracks with videoIds, plus `responseContext.visitorData` (48 chars).
- **Native `VISIONOS` player** (`clientVersion 1.02`, id 101):
  - Without visitorData: `LOGIN_REQUIRED`.
  - With search visitorData, as header or as context field: `OK`.
  - `expiresInSeconds` 21540. URLs carry no `n` parameter.
  - itags 251, 250 and 140 returned 206 at 1 MiB, 2 MiB and the last 64 KiB.
  - Two full tracks of 13.1 MB and 14.6 MB were fetched in 1 MiB ranges.
  - The same URL returned 206 at every offset tested, at 0, 70, 131 and 251 s after resolve.
  - Sequential 512 KiB ranged reads of whole tracks took 0.74–0.82 s, about 8–9 MB/s and 470–525× realtime, over both IPv6 and IPv4.
  - One IPv6 itag 140 request hung with no response for 30 s; the IPv4 retry was fine.
- **Seek** (symphonia 0.6.1 + libopus adapter, real YouTube files, counting reader standing in for range requests):
  - itag 251 WebM: accurate seeks to 60, 200, 30, 380 and 5 s each touched at most 2 chunks of 512 KiB, with 50 clean packets after each.
  - itag 140 fragmented M4A: accurate seeks, with cost growing with distance, up to 6 chunks and 18 I/O seeks for 380 s.
- **Decode** (research agent): a full itag 251 file decoded to 429.53 s with 0 errors. The first 1 MiB of itag 140 decoded to 59.9 s with 0 errors.
- **Ads:** none inside streams, per the research agent's checks of stream durations. Desktop, DesktopMusic and Tv get pre-roll ads as separate `adSlots` entries pointing at other video ids. All 5 VISIONOS player responses checked had no `adSlots`, `adPlacements` or `playerAds`.
- **librespot 0.8.0**: `audio_backend::Sink::write(AudioPacket::Samples(Vec<f64>))` delivers 44.1 kHz stereo PCM, which is the hook for slice 2.

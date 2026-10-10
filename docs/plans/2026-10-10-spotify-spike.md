# Spotify spike — findings (2026-10-10)

Status: **deferred.** The user dropped Spotify from v1 because of the account risks below. These notes are for picking it up later. Re-verify everything first: librespot and Spotify both move fast.

## Verdict

The code path works. It needs no librespot Player and no PCM-push engine variant:

```
AudioItem::get_file → pick OGG_VORBIS_320/160/96
  → AudioFile::open(session, file_id, bytes_per_second) ‖ session.audio_key().request(track, file)
  → AudioDecrypt::new(Some(key), file)   // CTR, fully random-access seek
  → skip 0xa7 bytes (Spotify's private Ogg header page)
  → AudioSource::Encoded { hint: FormatHint::OggVorbis } → symphonia ogg + vorbis
```

This was verified offline against symphonia 0.6.1 using a fake-encrypted Vorbis file: probe, 10 s decode and a seek to 60 s all worked. The login, key request and CDN fetch were never run.

## Risks that stopped it

- **Terms of Service.** Spotify's User Guidelines prohibit reverse engineering and circumvention, so using it risks an account ban.
- **librespot #1649.** Spotify refuses AES audio keys to some accounts. New accounts get none, and older accounts see intermittent key-error storms.
- **#1737.** Since 2026-08-10, only the desktop client ID `65b708073fc0480ea92a077233ca87bd` still gets valid tokens.
- **#1760.** crates.io 0.8.0 doesn't build with vergen 9.1.0. Pin dev rev `4a005903e` and commit the lockfile. Dev also carries the CDN non-206 fix (#1722) and 0600 credential files (#1650).

## Notes for implementation

- **Blocking reads.** `AudioFileStreaming::read` blocks on a Condvar for up to 8 s while fetching runs as a tokio task. Read only from requiem's decode thread, and keep the provider's runtime (1 worker) alive.
- **Async runtime.** The provider must own a tokio runtime, which breaks the "no async runtime" rule. Scope the runtime to the provider crate and shut it down after idle.
- **Session lifetime.** `SessionWeak::upgrade` panics if the Session is dropped while components still exist, so each byte source must hold the Session.
- **TLS roots.** Use rustls with webpki roots. Native roots panicked when the keychain was unavailable.
- **Seeking.** `SeekFrom::End` is off by one in librespot. Resolve every seek to `SeekFrom::Start`.
- **Metadata.** librespot-metadata provides metadata with no Web API.
- **Search.** Search is the weak part:
  - spclient context-resolve of `spotify:search:<q>` is undocumented and returns URIs only.
  - The Web API using the desktop client ID returns 429s.
  - A user-owned developer app is capped at 5 users and 10 results per call.
- **Login.** PKCE or device-code OAuth, done once. The reusable credential blob is equivalent to a password, so keep it in the Keychain.
- **Footprint.** tokio plus Session plus TLS adds about 0.5 MiB of physical footprint, with 2 threads at idle. Vorbis decode adds about 0.9 MiB. The authenticated session was not measured.

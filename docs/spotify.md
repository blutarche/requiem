# Spotify

Spotify is off until you sign in: `signin spotify` in the CLI, or **Sign in to Spotify** in the app. Enter the code it shows at <https://www.spotify.com/pair>. Playback needs Spotify Premium.

Requiem plays Spotify through [librespot](https://github.com/librespot-org/librespot), the way Spotify's own apps connect. Spotify doesn't allow unofficial players, so your Spotify account could be suspended.

Once signed in, search lists Spotify results first, Home shows your 20 most recently liked songs, the playlists Spotify made for you and your playlists, and Library shows Liked Songs and your playlists. The player keeps the sign-in in `spotify-credentials.json`, readable only by you, next to `state.json` (`REQUIEM_SPOTIFY_CREDENTIALS_FILE` overrides the path). `signout spotify` deletes it. Design: [`docs/plans/2026-10-16-spotify-provider-design.md`](plans/2026-10-16-spotify-provider-design.md).


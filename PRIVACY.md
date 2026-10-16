# Privacy

Requiem is a music player that runs on your own computer. Its developer runs no servers, and Requiem sends the developer nothing: no account data, no listening history, no analytics and no crash reports.

## What Requiem keeps on your computer

On macOS everything is in `~/Library/Application Support/requiem/`. On Linux the Google client is in `~/.config/requiem/` and the rest is in `~/.local/state/requiem/`, or in the folders `XDG_CONFIG_HOME` and `XDG_STATE_HOME` name.

- `google-client.json`: the Google OAuth client you created for yourself.
- `google-token.json`: your Google sign-in, readable only by your user account.
- `state.json`: your queue, the track Play resumes and the volume, so the player picks up where you left off.
- `update-state.json`: where a self-update is up to.

## What Requiem sends, and to whom

- **YouTube Music and YouTube:** searches, Home, artwork and the audio you play come from Google's YouTube services.
- **Google sign-in (optional):** with your permission, Requiem reads your YouTube playlists and liked videos through the YouTube Data API with read-only access (`https://www.googleapis.com/auth/youtube.readonly`). It never changes anything in your account.
- **GitHub:** the app checks Requiem's GitHub releases for updates and downloads them from there.

What those services do with the requests is covered by their own privacy policies.

## Google user data

Requiem uses Google user data only to show your own playlists and liked videos in the player on your computer. It doesn't store that data anywhere else, share it, sell it or use it for ads. Requiem's use of information received from Google APIs adheres to the [Google API Services User Data Policy](https://developers.google.com/terms/api-services-user-data-policy), including the Limited Use requirements.

## Removing your data

Sign out of Google in Requiem (Library, then Sign Out; or `signout` in the CLI). That revokes the sign-in and deletes `google-token.json`. You can also remove Requiem's access at <https://myaccount.google.com/permissions>. To remove everything, delete the folders above.

## Questions

Open an issue at <https://github.com/blutarche/requiem/issues>.

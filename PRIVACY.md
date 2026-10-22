# Privacy

Requiem is a music player that runs on your own computer. Its developer runs no servers, and Requiem sends the developer nothing: no account data, no listening history, no analytics and no crash reports.

## What Requiem keeps on your computer

On macOS everything is in `~/Library/Application Support/requiem/`. On Linux it is in `~/.local/state/requiem/`, or in the folder `XDG_STATE_HOME` names.

- `ytmusic-session.json`: your YouTube Music sign-in, the cookie and request headers of the browser session you pasted, with your account's name. It is readable only by your user account and, on macOS, excluded from Time Machine backups.
- `spotify-credentials.json`: your Spotify sign-in, readable only by your user account.
- `state.json`: your queue, the track Play resumes and the volume, so the player picks up where you left off.
- `update-state.json`: where a self-update is up to.

## What Requiem sends, and to whom

- **YouTube Music and YouTube:** searches, Home, artwork and the audio you play come from Google's YouTube services.
- **YouTube Music sign-in (optional):** you sign in by pasting the session of a browser where you are signed in to YouTube Music. Requiem sends that session only to YouTube Music's own API (`music.youtube.com`), to read your Home, your Library, the playlists you open and your account's name. Searches and playback never carry it. Requiem only reads; it never changes anything in your account.
- **Spotify sign-in (optional):** you sign in by entering a code at <https://www.spotify.com/pair>. Requiem then talks to Spotify the way Spotify's own apps do, through the open-source librespot library: your searches, your Liked Songs and playlists, artwork and the audio you play come from Spotify. Requiem only reads; it never changes anything in your account.
- **GitHub:** the app checks Requiem's GitHub releases for updates and downloads them from there.

What those services do with the requests is covered by their own privacy policies.

## Your YouTube Music data

Requiem uses what it reads with your YouTube Music session only to show your own music in the player on your computer. It doesn't store that data anywhere else, share it, sell it or use it for ads.

## Removing your data

Sign out in Requiem: Library, then Sign Out (choose the service if you're signed in to both), or `signout ytmusic` or `signout spotify` in the CLI. Signing out of YouTube Music deletes `ytmusic-session.json`; the browser you copied the session from stays signed in until you sign out there. Signing out of Spotify deletes `spotify-credentials.json`. To remove everything, delete the folder above.

Earlier versions signed in to YouTube Music with a Google client you created. The player deletes that sign-in, `google-token.json`, when it starts, and leaves your `google-client.json` for you to delete. You can remove that client's access at <https://myaccount.google.com/permissions>.

## Questions

Open an issue at <https://github.com/blutarche/requiem/issues>.

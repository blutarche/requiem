# YouTube Music

Search and playback work without signing in. Signing in adds your own YouTube Music to Home and the Library, and to `home` and `library` in the CLI.

## Sign in with your browser session

Requiem signs in to YouTube Music with the session of a browser where you are signed in. Home shows the first 6 shelves of YouTube Music's own Home, Library shows your Liked music and your playlists, and your private playlists and mixes open. Requiem sends the session only to YouTube Music's own API, for Home, Library, the playlists you open and your account's name. Search and playback stay anonymous, and plays aren't reported to Google.

1. In a browser where you are signed in, open [music.youtube.com](https://music.youtube.com).
2. Open the browser's developer tools with Option+Cmd+I and go to the **Network** tab. In Safari, first turn on **Show features for web developers** in **Safari → Settings… → Advanced**.
3. Type `browse` in the Network tab's filter, then click **Explore** or **Library** in YouTube Music's sidebar (or scroll Home) so a `browse` request appears. Right-click it (in Safari, Control-click).
   - **Chrome:** choose **Copy → Copy as cURL**.
   - **Firefox:** in the **Copy** submenu, choose **Copy Request Headers** or **Copy as cURL**.
   - **Safari:** choose **Copy as cURL**.
4. In Requiem, choose **Sign in to YouTube Music** (on Home, the Library or in the palette), or open **Settings** and click **Sign In…** under YouTube Music. Settings shows these steps; click **Paste from Clipboard**, or press Cmd+V.

Requiem checks the session with YouTube Music before it keeps it. Once it does, it clears this Mac's clipboard, unless you copied something else since. Clear the clipboard yourself on a computer you copied from.

**What Requiem keeps:** the request's cookie and its `X-Goog-AuthUser`, `X-Goog-PageId` and `X-Goog-Visitor-Id` headers, with your account's name, in `ytmusic-session.json` next to `state.json` (on macOS, `~/Library/Application Support/requiem/`). The rest of the paste is dropped. The file is readable only by you and, on macOS, excluded from Time Machine backups. A new session replaces it: **Use Browser Session…** in Settings, while you are signed in, takes another. `account` in the CLI shows the state.

**The risks:** the cookie is your Google sign-in, not just YouTube Music's. Anyone who has the file, or the copied text, can use your Google account until that browser signs out. Using it is against YouTube's terms, and Google could flag the account.

**Signing out:** **Sign Out…** in Settings, or `signout ytmusic` in the CLI, deletes the file. If the file can't be deleted, it says why, and the session stays until you sign out again. Requiem can't sign your browser out: the copied cookie keeps working until you sign out of Google in that browser profile, which ends it. When Google ends a session, Requiem signs YouTube Music out and asks for a new one in Settings.

**From another computer:** over ssh, the CLI reads the paste from stdin with `-`, or from a file. Copy the headers on the computer with the browser, then send them from that computer's clipboard, so they never land on the Mac's disk or in a shell history:

```sh
pbpaste | ssh my-mac 'requiem signin-browser ytmusic -'
```

On Linux, use `wl-paste` or `xclip -o -selection clipboard` in place of `pbpaste`. Or save them to a file, send that, and delete it afterwards:

```sh
ssh my-mac 'requiem signin-browser ytmusic -' < headers.txt
```

Pasting into `requiem signin-browser ytmusic -` at a prompt on the Mac doesn't work: macOS passes a program at most 1023 characters of a typed or pasted line, and the cookie line is longer. In the app bundle the CLI is `/Applications/Requiem.app/Contents/MacOS/requiem`. It returns once YouTube Music has taken the session; if YouTube Music refuses it, it prints why and exits 2. With `--json` it prints an `ack` or an `account_error`. `signin ytmusic` doesn't sign in; it says to use `signin-browser`.

**From an earlier version:** Requiem used to sign in to YouTube Music with your own Google Cloud client. That sign-in, `google-token.json`, is deleted when the player starts, so YouTube Music reads as signed out until you sign in with a browser session. Requiem leaves a `google-client.json` alone; delete it if you no longer want it, and remove Requiem's access at <https://myaccount.google.com/permissions>.


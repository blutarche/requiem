# Updates

On macOS, Requiem updates itself from signed GitHub releases when it runs from Applications, or another folder you can write to. A downloaded update installs when the player next quits, or at once through **Restart to Update** in the window or the menu-bar menu. **Check for Updates…** and **Check for Updates Automatically** are in the Requiem menu; `requiem update` shows the status. Design: [`docs/self-update-design.md`](self-update-design.md).

The player fetches two small files from github.com at most once every six hours, to learn whether a newer release exists, and downloads it from GitHub when there is one. GitHub sees your IP address, the time and the file fetched. Nothing else is sent. Turning off Check for Updates Automatically stops these requests.


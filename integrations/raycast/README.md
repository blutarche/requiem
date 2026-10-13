# requiem for Raycast

Drive the [requiem](../../README.md) YouTube Music player from Raycast.

## Commands

- **Play/Pause**, **Next Track**, **Previous Track**: no-view; a HUD shows the new state or track.
- **Search Music**: search as you type. Enter plays the result now (replacing the queue); Cmd+Enter adds it to the queue.
- **Queue**: the play queue with the current track marked. Enter plays the selected track.

Each action runs `requiem --connect --json --script -` and sends the commands on stdin. The CLI starts the `requiemd` player if it isn't running.

## Load it locally

You need Raycast, Node 22 or later, and a requiem build that includes the CLI in the app bundle (`scripts/bundle-macos.sh`, then copy `target/requiem.app` to `/Applications`).

```sh
cd integrations/raycast
npm install
npm run dev
```

`npm run dev` imports the extension into Raycast and rebuilds it as you edit; stop it with Ctrl+C and the commands stay installed. Use `npm run build` for a one-off build.

If requiem isn't at `/Applications/requiem.app`, set **Requiem CLI Path** in the extension's preferences (Raycast Settings, Extensions, Requiem), for example to `target/release/requiem` in a checkout, which finds `requiemd` next to itself.

## Develop

- `npm test` runs the wrapper tests. With `REQUIEM_LIVE_CLI=<path to requiem>` and a private `REQUIEM_SOCKET_DIR` set, it also parses real CLI output.
- `npm run lint` runs `ray lint`, which checks that `author` in `package.json` is a Raycast account name. Set it to yours.
- `scripts/make-icon.py` redraws `assets/extension-icon.png`.

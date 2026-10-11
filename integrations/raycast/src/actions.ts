import { getPreferenceValues, openExtensionPreferences, showHUD } from "@raycast/api";
import { showFailureToast } from "@raycast/utils";
import { CliError, DEFAULT_CLI_PATH, Message, QueueItem, Status, ofType, runCli, skipHud, playPauseHud } from "./cli";

export function cliPath(): string {
  return getPreferenceValues<Preferences>().cliPath || DEFAULT_CLI_PATH;
}

export function run(commands: string[], signal?: AbortSignal): Promise<Message[]> {
  return runCli(cliPath(), commands, { signal });
}

export async function reportFailure(error: unknown): Promise<void> {
  await showFailureToast(error, {
    title: "requiem",
    primaryAction:
      error instanceof CliError && error.fixable
        ? { title: "Open Extension Preferences", onAction: () => openExtensionPreferences() }
        : undefined,
  });
}

/** Runs a no-view command: `script` plus a `done` that turns the output into a HUD line. */
export async function transport(script: string[], hud: (messages: Message[]) => string): Promise<void> {
  try {
    await showHUD(hud(await run(script)));
  } catch (error) {
    await reportFailure(error);
  }
}

export const pauseHud = (m: Message[]) => playPauseHud(ofType<Status>(m, "status").pop());
export const skipHudFor = (prefix: string) => (m: Message[]) => skipHud(prefix, ofType<QueueItem>(m, "queue_item"));

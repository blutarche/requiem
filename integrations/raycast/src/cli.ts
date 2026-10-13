import { spawn } from "node:child_process";

export const DEFAULT_CLI_PATH = "/Applications/Requiem.app/Contents/MacOS/requiem";

export interface TrackInfo {
  id: { provider: string; id: string };
  title: string;
  artists: string[];
  album: string | null;
  duration_ms: number | null;
  artwork_url: string | null;
}

export interface SearchResult extends TrackInfo {
  type: "result";
  index: number;
}

export interface QueueItem extends TrackInfo {
  type: "queue_item";
  index: number;
  current: boolean;
}

export interface Status {
  type: "status";
  state: "playing" | "paused" | "buffering" | "stopped";
  track: TrackInfo | null;
  position_ms: number;
  duration_ms: number | null;
  volume: number;
}

export interface Message {
  type: string;
  [key: string]: unknown;
}

/** A failure whose message tells the user what to fix. */
export class CliError extends Error {
  readonly fixable: boolean;

  constructor(message: string, fixable = false) {
    super(message);
    this.name = "CliError";
    this.fixable = fixable;
  }
}

export function parseLines(stdout: string): Message[] {
  const messages: Message[] = [];
  for (const line of stdout.split("\n")) {
    if (!line.trim()) continue;
    try {
      const value: unknown = JSON.parse(line);
      if (value && typeof value === "object" && typeof (value as Message).type === "string") {
        messages.push(value as Message);
      }
    } catch {
      // Not a JSON line; the CLI only prints JSON in this mode.
    }
  }
  return messages;
}

export function ofType<T extends { type: string }>(messages: Message[], type: T["type"]): T[] {
  return messages.filter((m) => m.type === type) as unknown as T[];
}

/** Throws the first `error` message, rewritten to say what to fix. */
export function throwOnError(messages: Message[], cliPath: string): void {
  const error = messages.find((m) => m.type === "error");
  if (!error) return;
  const text = String(error.message ?? "unknown error");
  if (text.startsWith("cannot reach the daemon")) {
    throw new CliError(
      `Requiem's player could not be started (${text}). Open Requiem.app once, or check the CLI path (${cliPath}).`,
      true,
    );
  }
  throw new CliError(text);
}

export interface RunOptions {
  signal?: AbortSignal;
  timeoutMs?: number;
}

/** A script is line based; keep user text from smuggling in more commands. */
export function oneLine(text: string): string {
  return text.replace(/\s+/g, " ").trim();
}

/** Runs `commands` as one requiem script and resolves with every JSON line it printed. */
export function runCli(cliPath: string, commands: string[], options: RunOptions = {}): Promise<Message[]> {
  const path = cliPath.trim() || DEFAULT_CLI_PATH;
  return new Promise((resolve, reject) => {
    const child = spawn(path, ["--connect", "--json", "--script", "-"], {
      stdio: ["pipe", "pipe", "pipe"],
      signal: options.signal,
      timeout: options.timeoutMs ?? 30_000,
    });
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8").on("data", (chunk: string) => (stdout += chunk));
    child.stderr.setEncoding("utf8").on("data", (chunk: string) => (stderr += chunk));
    child.stdin.on("error", () => {
      // The process died before reading the script; `close` reports why.
    });
    child.on("error", (err: NodeJS.ErrnoException) => {
      if (err.code === "ENOENT" || err.code === "EACCES") {
        return reject(
          new CliError(
            `The Requiem CLI was not found at ${path}. Install Requiem.app, or set "Requiem CLI Path" in the extension preferences.`,
            true,
          ),
        );
      }
      reject(err);
    });
    child.on("close", (code, signal) => {
      const messages = parseLines(stdout);
      try {
        throwOnError(messages, path);
      } catch (err) {
        return reject(err);
      }
      if (signal) return reject(new CliError(`The Requiem CLI timed out (${signal}).`));
      if (code !== 0) return reject(new CliError(stderr.trim() || `The Requiem CLI exited with code ${code}.`));
      resolve(messages);
    });
    child.stdin.end(commands.map((c) => c.replace(/[\r\n]+/g, " ")).join("\n") + "\n");
  });
}

export function artistLine(track: TrackInfo): string {
  return track.artists.join(", ");
}

export function formatDuration(ms: number | null): string {
  if (ms === null) return "";
  const s = Math.floor(ms / 1000);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

export function playPauseHud(status: Status | undefined): string {
  if (!status) return "Requiem did not report its state";
  const title = status.track ? status.track.title : "";
  switch (status.state) {
    case "playing":
      return `Playing: ${title}`;
    case "buffering":
      return `Buffering: ${title}`;
    case "paused":
      return `Paused: ${title}`;
    default:
      return "Nothing playing";
  }
}

export function skipHud(prefix: string, queue: QueueItem[]): string {
  const current = queue.find((q) => q.current);
  return current ? `${prefix}: ${current.title}` : "End of queue";
}

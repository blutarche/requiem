import assert from "node:assert/strict";
import { test } from "node:test";
import {
  CliError,
  type QueueItem,
  type SearchResult,
  type Status,
  ofType,
  oneLine,
  parseLines,
  playPauseHud,
  runCli,
  skipHud,
  throwOnError,
} from "./cli.ts";

const sample = [
  '{"type":"queue_changed","count":0,"current":null}',
  '{"type":"result","index":1,"title":"Aruarian Dance","artists":["Nujabes"],"album":"Samurai Champloo","duration_ms":175000,"artwork_url":"https://x/a.jpg"}',
  "not json",
  '{"type":"status","state":"playing","track":{"title":"T","artists":["A"],"album":null,"duration_ms":null,"artwork_url":null},"position_ms":0,"duration_ms":null,"volume":100}',
  "",
].join("\n");

test("parseLines keeps JSON objects with a type and drops the rest", () => {
  const messages = parseLines(sample);
  assert.deepEqual(
    messages.map((m) => m.type),
    ["queue_changed", "result", "status"],
  );
  const [result] = ofType<SearchResult>(messages, "result");
  assert.equal(result.title, "Aruarian Dance");
  assert.equal(result.index, 1);
});

test("hud text follows the reported state", () => {
  const [status] = ofType<Status>(parseLines(sample), "status");
  assert.equal(playPauseHud(status), "Playing: T");
  assert.equal(playPauseHud({ ...status, state: "stopped" }), "Nothing playing");
  const queue = [
    { index: 1, current: false, title: "a" },
    { index: 2, current: true, title: "b" },
  ] as QueueItem[];
  assert.equal(skipHud("Next", queue), "Next: b");
  assert.equal(skipHud("Next", []), "End of queue");
});

test("a daemon that cannot start is a fixable error", () => {
  const messages = parseLines('{"type":"error","message":"cannot reach the daemon: spawn failed"}');
  assert.throws(
    () => throwOnError(messages, "/x/requiem"),
    (e: unknown) => e instanceof CliError && e.fixable && e.message.includes("/x/requiem"),
  );
});

test("user text stays on one script line", () => {
  assert.equal(oneLine("a\nshutdown-daemon  b"), "a shutdown-daemon b");
});

test("a missing CLI is a fixable error", async () => {
  await assert.rejects(
    runCli("/nonexistent/requiem", ["status"]),
    (e: unknown) => e instanceof CliError && e.fixable && e.message.includes("/nonexistent/requiem"),
  );
});

// Needs a daemon of its own: REQUIEM_LIVE_CLI=<cli> with REQUIEM_SOCKET_DIR set.
test("live: the wrapper parses real CLI output", { skip: !process.env.REQUIEM_LIVE_CLI }, async () => {
  const messages = await runCli(process.env.REQUIEM_LIVE_CLI!, [
    "search nujabes",
    "await results 20",
    "status",
    "queue",
  ]);
  const results = ofType<SearchResult>(messages, "result");
  assert.ok(results.length > 0, "search returned results");
  assert.ok(results.every((r, i) => r.index === i + 1 && r.title));
  assert.equal(ofType<Status>(messages, "status").length, 1);
});

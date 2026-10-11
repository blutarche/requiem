import { Action, ActionPanel, Icon, Image, List, showHUD } from "@raycast/api";
import { usePromise } from "@raycast/utils";
import { useRef, useState } from "react";
import { reportFailure, run } from "./actions";
import { SearchResult, artistLine, formatDuration, ofType, oneLine } from "./cli";

const RESULT_TIMEOUT_SECS = 15;

export default function Command() {
  const [query, setQuery] = useState("");
  const text = oneLine(query);
  const abortable = useRef<AbortController>(null);

  const { data, isLoading } = usePromise(
    async (q: string) => {
      if (!q) return [];
      return ofType<SearchResult>(
        await run([`search ${q}`, `await results ${RESULT_TIMEOUT_SECS}`], abortable.current?.signal),
        "result",
      );
    },
    [text],
    { abortable, onError: reportFailure },
  );

  // `play` and `add` pick from the CLI session's own results, so the search is repeated in the same script.
  async function pick(command: "play" | "add", item: SearchResult) {
    try {
      await run([`search ${text}`, `await results ${RESULT_TIMEOUT_SECS}`, `${command} ${item.index}`]);
      await showHUD(command === "play" ? `Playing: ${item.title}` : `Added to queue: ${item.title}`);
    } catch (error) {
      await reportFailure(error);
    }
  }

  return (
    <List isLoading={isLoading} throttle onSearchTextChange={setQuery} searchBarPlaceholder="Search YouTube Music">
      {!text ? (
        <List.EmptyView icon={Icon.Music} title="Type to search" />
      ) : (
        <List.EmptyView icon={Icon.MagnifyingGlass} title="No results" description={`Nothing found for “${text}”.`} />
      )}
      {(data ?? []).map((item) => (
        <List.Item
          key={item.index}
          icon={
            item.artwork_url
              ? { source: item.artwork_url, mask: Image.Mask.RoundedRectangle, fallback: Icon.Music }
              : Icon.Music
          }
          title={item.title}
          subtitle={artistLine(item)}
          accessories={[{ text: item.album ?? undefined }, { text: formatDuration(item.duration_ms) }]}
          actions={
            <ActionPanel>
              <Action title="Play Now" icon={Icon.Play} onAction={() => pick("play", item)} />
              <Action
                title="Add to Queue"
                icon={Icon.Plus}
                shortcut={{ modifiers: ["cmd"], key: "return" }}
                onAction={() => pick("add", item)}
              />
            </ActionPanel>
          }
        />
      ))}
    </List>
  );
}

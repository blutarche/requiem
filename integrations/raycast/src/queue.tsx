import { Action, ActionPanel, Icon, Image, List, showHUD } from "@raycast/api";
import { usePromise } from "@raycast/utils";
import { reportFailure, run } from "./actions";
import { QueueItem, artistLine, formatDuration, ofType } from "./cli";

export default function Command() {
  const { data, isLoading, revalidate } = usePromise(
    async () => ofType<QueueItem>(await run(["queue"]), "queue_item"),
    [],
    {
      onError: reportFailure,
    },
  );
  const items = data ?? [];

  async function jump(item: QueueItem) {
    try {
      await run([`jump ${item.index}`]);
      await showHUD(`Playing: ${item.title}`);
      revalidate();
    } catch (error) {
      await reportFailure(error);
    }
  }

  return (
    <List isLoading={isLoading} selectedItemId={items.find((i) => i.current)?.index.toString()}>
      <List.EmptyView icon={Icon.List} title="The queue is empty" description="Use Search Music to add tracks." />
      {items.map((item) => (
        <List.Item
          key={item.index}
          id={item.index.toString()}
          icon={
            item.current
              ? Icon.SpeakerOn
              : item.artwork_url
                ? { source: item.artwork_url, mask: Image.Mask.RoundedRectangle, fallback: Icon.Music }
                : Icon.Music
          }
          title={item.title}
          subtitle={artistLine(item)}
          accessories={[
            ...(item.current ? [{ tag: "Now Playing" }] : []),
            { text: item.album ?? undefined },
            { text: formatDuration(item.duration_ms) },
          ]}
          actions={
            <ActionPanel>
              <Action title="Play" icon={Icon.Play} onAction={() => jump(item)} />
            </ActionPanel>
          }
        />
      ))}
    </List>
  );
}

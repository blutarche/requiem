import { skipHudFor, transport } from "./actions";

export default async function Command() {
  await transport(["prev", "sleep 0.3", "queue"], skipHudFor("Previous"));
}

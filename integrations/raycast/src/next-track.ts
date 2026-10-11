import { skipHudFor, transport } from "./actions";

export default async function Command() {
  await transport(["next", "sleep 0.3", "queue"], skipHudFor("Next"));
}

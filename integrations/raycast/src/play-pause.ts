import { pauseHud, transport } from "./actions";

export default async function Command() {
  await transport(["toggle", "sleep 0.3", "status"], pauseHud);
}

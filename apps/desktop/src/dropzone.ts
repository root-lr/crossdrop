import "@fontsource/atkinson-hyperlegible/700.css";
import "./dropzone.css";
import { t } from "./i18n";

declare global {
  interface Window {
    __DROPZONE?: { device: string; side: string };
  }
}

const zone = document.getElementById("zone")!;
const info = window.__DROPZONE ?? { device: t.nextComputer, side: "right" };
zone.className = `zone ${info.side}`;
// Shown only when the window opens wide, during a drag (see dropzone.css).
// The name goes in as text, never parsed as markup.
const name = document.createElement("strong");
name.textContent = info.device;
const p = document.createElement("p");
p.append(t.dropOnDesktop[0], name, t.dropOnDesktop[1]);
zone.replaceChildren(p);

import "@fontsource/atkinson-hyperlegible/700.css";
import "./dropzone.css";

declare global {
  interface Window {
    __DROPZONE?: { device: string; side: string };
  }
}

const zone = document.getElementById("zone")!;
const info = window.__DROPZONE ?? { device: "the next computer", side: "right" };
zone.className = `zone ${info.side}`;
// Shown only when the window opens wide, during a drag (see dropzone.css).
zone.innerHTML = `<p>Drop on <strong></strong>'s desktop</p>`;
zone.querySelector("strong")!.textContent = info.device;

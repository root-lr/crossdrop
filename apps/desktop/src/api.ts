import { invoke } from "@tauri-apps/api/core";

export type Side = "left" | "right" | "top" | "bottom";

export interface Device {
  id: string;
  name: string;
  addrs: string[];
  paired: boolean;
  online: boolean;
  permissions: string[];
  auto_accept: boolean;
}

export interface PairRequest {
  pair_id: string;
  peer_id: string;
  peer_name: string;
  sas: string;
}

export interface Offer {
  offer_id: string;
  peer_id: string;
  peer_name: string;
  files: number;
  total_bytes: number;
  preview: string[];
}

export interface Overview {
  me: { id: string; name: string; port: number };
  devices: Device[];
  pairs: PairRequest[];
  offers: Offer[];
  layout: { device: string; side: Side }[];
  sharing: boolean;
  embedded: boolean;
}

export interface Transfer {
  id: number;
  device: string;
  files: number;
  done: number;
  total: number;
  state: "waiting" | "sending" | "done" | "declined" | "failed";
  message: string;
  desktop: boolean;
}

export interface Challenge {
  pair_id: string;
  code: string;
  peer_id: string;
  peer_name: string;
}

export const SERVICES = {
  files: "crossdrop/files",
  clipboard: "crossdrop/clipboard",
  input: "interdesk/input",
} as const;

export const api = {
  overview: () => invoke<Overview>("overview"),
  pairStart: (target: string) => invoke<Challenge>("pair_start", { target }),
  pairConfirm: (pairId: string, accept: boolean) => invoke<boolean>("pair_confirm", { pairId, accept }),
  pairAnswer: (pairId: string, accept: boolean) => invoke<void>("pair_answer", { pairId, accept }),
  offerAnswer: (offerId: string, accept: boolean) => invoke<void>("offer_answer", { offerId, accept }),
  setPermission: (device: string, service: string, allow: boolean) =>
    invoke<void>("set_permission", { device, service, allow }),
  setAutoAccept: (device: string, enabled: boolean) => invoke<void>("set_auto_accept", { device, enabled }),
  unpair: (device: string) => invoke<void>("unpair", { device }),
  sendFiles: (device: string, paths: string[], desktop = false) =>
    invoke<number>("send_files", { device, paths, desktop }),
  layoutSet: (side: Side, device: string | null) => invoke<void>("layout_set", { side, device }),
  sharingSet: (on: boolean) => invoke<void>("sharing_set", { on }),
};

export function shortId(id: string): string {
  return id.slice(0, 9);
}

export function bytes(n: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = n;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u++;
  }
  return u === 0 ? `${n} B` : `${v.toFixed(v < 10 ? 1 : 0)} ${units[u]}`;
}

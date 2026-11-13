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
  clipboard_send: boolean;
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

export interface Receiving {
  transfer_id: string;
  peer_name: string;
  files: number;
  total_bytes: number;
  received_bytes: number;
  current: string;
}

export interface Overview {
  me: { id: string; name: string; port: number };
  devices: Device[];
  pairs: PairRequest[];
  offers: Offer[];
  /** Accepted transfers arriving now. */
  receiving: Receiving[];
  /** `id` is set for devices placed since ids are kept. */
  layout: { device: string; side: Side; id: string | null }[];
  /** interdesk.toml could not be read: why. */
  layout_error: string | null;
  /** e.g. "Ctrl+Alt+Escape". */
  return_key: string;
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
  receivingCancel: (transferId: string) => invoke<void>("receiving_cancel", { transferId }),
  setPermission: (device: string, service: string, allow: boolean) =>
    invoke<void>("set_permission", { device, service, allow }),
  setAutoAccept: (device: string, enabled: boolean) => invoke<void>("set_auto_accept", { device, enabled }),
  setClipboardSend: (device: string, enabled: boolean) => invoke<void>("set_clipboard_send", { device, enabled }),
  unpair: (device: string) => invoke<void>("unpair", { device }),
  /** `device`: id (or name); `label`: what the transfer list shows. */
  sendFiles: (device: string, paths: string[], desktop = false, label?: string) =>
    invoke<number>("send_files", { device, paths, desktop, label: label ?? null }),
  layoutSet: (side: Side, device: string | null) => invoke<void>("layout_set", { side, device }),
  sharingSet: (on: boolean) => invoke<void>("sharing_set", { on }),
};

export function shortId(id: string): string {
  return id.slice(0, 9);
}

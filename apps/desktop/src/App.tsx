import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { disable, enable, isEnabled } from "@tauri-apps/plugin-autostart";
import { open } from "@tauri-apps/plugin-dialog";
import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";
import { bytes, t } from "./i18n";
import {
  api,
  shortId,
  SERVICES,
  type Challenge,
  type Device,
  type Overview,
  type Receiving,
  type Side,
  type Transfer,
} from "./api";

const SIDES: Side[] = ["top", "left", "right", "bottom"];

function useOverview() {
  const [data, setData] = useState<Overview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const refresh = useCallback(async () => {
    try {
      setData(await api.overview());
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  }, []);
  useEffect(() => {
    refresh();
    const t = setInterval(refresh, 1000);
    return () => clearInterval(t);
  }, [refresh]);
  return { data, error, refresh };
}

/** Runs an action, reports its error, refreshes the view. */
function useAct(refresh: () => void, setNotice: (m: string | null) => void) {
  return useCallback(
    async (fn: () => Promise<unknown>) => {
      try {
        await fn();
        setNotice(null);
      } catch (e) {
        setNotice(String(e));
      }
      refresh();
    },
    [refresh, setNotice],
  );
}

export default function App() {
  const { data, error, refresh } = useOverview();
  const [notice, setNotice] = useState<string | null>(null);
  const [transfers, setTransfers] = useState<Record<number, Transfer>>({});
  const [pairing, setPairing] = useState<Challenge | null>(null);
  const [hover, setHover] = useState<string | null>(null);
  const act = useAct(refresh, setNotice);

  useEffect(() => {
    const un = listen<Transfer>("transfer", (e) => setTransfers((all) => ({ ...all, [e.payload.id]: e.payload })));
    return () => {
      un.then((f) => f());
    };
  }, []);

  // Files dragged from the file manager onto a device.
  const devicesRef = useRef<Device[]>([]);
  devicesRef.current = data?.devices ?? [];
  useEffect(() => {
    const target = (x: number, y: number) => {
      const r = window.devicePixelRatio || 1;
      const el = document.elementFromPoint(x / r, y / r)?.closest<HTMLElement>("[data-drop]");
      return el?.dataset.drop ?? null;
    };
    const un = getCurrentWebview().onDragDropEvent((e) => {
      const p = e.payload;
      if (p.type === "over" || p.type === "enter") {
        setHover(target(p.position.x, p.position.y));
      } else if (p.type === "drop") {
        const device = target(p.position.x, p.position.y);
        setHover(null);
        if (device) {
          const label = devicesRef.current.find((d) => d.id === device)?.name ?? device;
          api.sendFiles(device, p.paths, false, label).catch((err) => setNotice(String(err)));
        }
      } else {
        setHover(null);
      }
    });
    return () => {
      un.then((f) => f());
    };
  }, []);

  useNotifyNewRequests(data);

  if (!data) {
    return (
      <main className="boot">
        <p>{error ? t.unreachable(error) : t.starting}</p>
      </main>
    );
  }

  const paired = data.devices.filter((d) => d.paired);
  const nearby = data.devices.filter((d) => !d.paired);
  const bySide = Object.fromEntries(data.layout.map((n) => [n.side, n])) as Partial<
    Record<Side, Overview["layout"][number]>
  >;
  const placed = new Set(data.layout.flatMap((n) => [n.device.toLowerCase(), ...(n.id ? [n.id] : [])]));
  const deviceNamed = (name: string) => data.devices.find((d) => d.name.toLowerCase() === name.toLowerCase());
  const list = Object.values(transfers).sort((a, b) => b.id - a.id);

  const sendWithPicker = async (device: Device) => {
    const picked = await open({ multiple: true, title: t.sendTo(device.name) });
    const paths = Array.isArray(picked) ? picked : picked ? [picked] : [];
    if (paths.length) act(() => api.sendFiles(device.id, paths, false, device.name));
  };

  return (
    <div className="app">
      <header className="top">
        <div className="me">
          <h1>{data.me.name}</h1>
          <span className="id" title={data.me.id}>
            {shortId(data.me.id)}
          </span>
        </div>
        <label className={`share ${data.sharing ? "on" : ""}`}>
          <input
            type="checkbox"
            role="switch"
            checked={data.sharing}
            onChange={(e) => act(() => api.sharingSet(e.target.checked))}
          />
          <span className="track" aria-hidden />
          <span>
            {t.shareInput}
            <small>{data.sharing ? t.shareOn(data.return_key) : t.off}</small>
          </span>
        </label>
      </header>

      {data.layout_error && (
        <div className="notice" role="alert">
          {t.layoutError(data.layout_error)}
        </div>
      )}

      {(notice || error) && (
        <div className="notice" role="alert">
          {notice ?? error}
          <button className="link" onClick={() => setNotice(null)}>
            {t.dismiss}
          </button>
        </div>
      )}

      <div className="columns">
        <section className="desk" aria-label={t.desk}>
          <div className="desk-grid">
            {SIDES.map((side) => {
              const entry = bySide[side];
              // By id when the layout has it: right even after a rename.
              const dev = entry
                ? ((entry.id ? data.devices.find((d) => d.id === entry.id) : undefined) ?? deviceNamed(entry.device))
                : undefined;
              const name = dev?.name ?? entry?.device;
              // Drop on the device itself when the layout name is known.
              const drop = dev?.id ?? name;
              return (
                <div key={side} className={`slot slot-${side}`}>
                  {name ? (
                    <div
                      className={`neighbour ${hover === drop ? "hover" : ""} ${dev?.online ? "online" : ""}`}
                      data-drop={drop}
                    >
                      <strong>{name}</strong>
                      <span>{dev?.online ? t.dropHere : t.offline}</span>
                      <button
                        className="link quiet"
                        onClick={() => act(() => api.layoutSet(side, null))}
                        aria-label={t.removeFrom(name, t.sideIn[side])}
                      >
                        {t.remove}
                      </button>
                    </div>
                  ) : (
                    <label className="empty">
                      <span>{t.side[side]}</span>
                      <select
                        value=""
                        disabled={paired.length === 0}
                        onChange={(e) => e.target.value && act(() => api.layoutSet(side, e.target.value))}
                      >
                        <option value="">{paired.length ? t.choose : t.pairFirst}</option>
                        {paired
                          .filter((d) => !placed.has(d.name.toLowerCase()) && !placed.has(d.id))
                          .map((d) => (
                            <option key={d.id} value={d.id}>
                              {d.name}
                            </option>
                          ))}
                      </select>
                    </label>
                  )}
                </div>
              );
            })}
            <div className="slot slot-center">
              <div className="screen">
                <strong>{data.me.name}</strong>
                <span>{t.thisComputer}</span>
              </div>
            </div>
          </div>
          <p className="hint">{t.deskHint}</p>

          <h2>{t.devices}</h2>
          {paired.length === 0 && (
            <p className="hint">{t.noPaired}</p>
          )}
          <ul className="devices">
            {paired.map((d) => (
              <DeviceRow
                key={d.id}
                device={d}
                hover={hover === d.id}
                onSend={() => sendWithPicker(d)}
                act={act}
              />
            ))}
          </ul>

          <h2>{t.nearby}</h2>
          <ul className="devices">
            {nearby.map((d) => (
              <li key={d.id} className="device">
                <div className="device-head">
                  <span className={`dot ${d.online ? "on" : ""}`} aria-hidden />
                  <strong>{d.name}</strong>
                  <span className="id">{shortId(d.id)}</span>
                  <button className="primary" onClick={() => act(async () => setPairing(await api.pairStart(d.id)))}>
                    {t.pair}
                  </button>
                </div>
              </li>
            ))}
          </ul>
          <PairByAddress onStart={(addr) => act(async () => setPairing(await api.pairStart(addr)))} />
        </section>

        <aside className="side">
          <h2>{t.waitingForYou}</h2>
          {data.pairs.length === 0 && data.offers.length === 0 && <p className="hint">{t.nothingWaiting}</p>}
          {data.pairs.map((r) => (
            <div key={r.pair_id} className="request">
              <p>{t.wantsToPair(<strong>{r.peer_name}</strong>)}</p>
              <p className="code">{r.sas}</p>
              <div className="actions">
                <button className="primary" onClick={() => act(() => api.pairAnswer(r.pair_id, true))}>
                  {t.codesMatchPair}
                </button>
                <button onClick={() => act(() => api.pairAnswer(r.pair_id, false))}>{t.reject}</button>
              </div>
            </div>
          ))}
          {data.offers.map((o) => (
            <div key={o.offer_id} className="request">
              <p>{t.wantsToSend(<strong>{o.peer_name}</strong>, t.files(o.files), bytes(o.total_bytes))}</p>
              <p className="files">
                {o.preview.join(", ")}
                {o.files > o.preview.length ? ", …" : ""}
              </p>
              <div className="actions">
                <button className="primary" onClick={() => act(() => api.offerAnswer(o.offer_id, true))}>
                  {t.receive}
                </button>
                <button onClick={() => act(() => api.offerAnswer(o.offer_id, false))}>{t.decline}</button>
              </div>
            </div>
          ))}

          <div className="transfers-head">
            <h2>{t.transfers}</h2>
            {list.some((x) => FINISHED.includes(x.state)) && (
              <button
                className="link quiet"
                onClick={() =>
                  setTransfers((all) =>
                    Object.fromEntries(Object.entries(all).filter(([, x]) => !FINISHED.includes(x.state))),
                  )
                }
              >
                {t.clearFinished}
              </button>
            )}
          </div>
          {list.length === 0 && data.receiving.length === 0 && (
            <p className="hint">{t.dropToSend}</p>
          )}
          <ul className="transfers">
            {data.receiving.map((r) => (
              <ReceivingItem
                key={r.transfer_id || `${r.peer_name}:${r.total_bytes}`}
                transfer={r}
                onCancel={() => act(() => api.receivingCancel(r.transfer_id))}
              />
            ))}
            {list.map((x) => (
              <li key={x.id}>
                <div className="transfer-head">
                  <span>
                    {t.to(<strong>{x.device}</strong>)}
                    {x.desktop ? t.toDesktop : ""}
                  </span>
                  <span className={`state ${x.state}`}>{t.state[x.state]}</span>
                </div>
                <progress max={x.total || 1} value={x.state === "done" ? x.total || 1 : x.done} />
                <small>
                  {x.files ? `${t.files(x.files)}, ` : ""}
                  {t.progress(bytes(x.done), bytes(x.total))}
                  {x.message ? `: ${x.message}` : ""}
                </small>
              </li>
            ))}
          </ul>

          <h2>{t.thisComputer}</h2>
          <StartAtLogin onError={setNotice} />
        </aside>
      </div>

      {pairing && (
        <PairDialog
          challenge={pairing}
          onClose={() => {
            setPairing(null);
            refresh();
          }}
        />
      )}
    </div>
  );
}

/** A transfer arriving now, which can be stopped (after a confirmation,
 * since what it stored is deleted). */
function ReceivingItem({ transfer: r, onCancel }: { transfer: Receiving; onCancel: () => void }) {
  const [confirming, setConfirming] = useState(false);
  return (
    <li>
      <div className="transfer-head">
        <span>
          {t.from(<strong>{r.peer_name}</strong>)}
        </span>
        <span className="state sending">{t.receiving}</span>
      </div>
      <progress max={r.total_bytes || 1} value={r.received_bytes} />
      <small>
        {t.files(r.files)}, {t.progress(bytes(r.received_bytes), bytes(r.total_bytes))}
        {r.current ? `: ${r.current}` : ""}
      </small>
      {r.transfer_id &&
        (confirming ? (
          <div className="transfer-actions">
            <span>{t.stopAndDelete}</span>
            <button className="link" onClick={onCancel}>
              {t.stop}
            </button>
            <button className="link quiet" onClick={() => setConfirming(false)}>
              {t.keepReceiving}
            </button>
          </div>
        ) : (
          <div className="transfer-actions">
            <button className="link quiet" onClick={() => setConfirming(true)}>
              {t.cancel}
            </button>
          </div>
        ))}
    </li>
  );
}

/** Transfers that will not change any more. */
const FINISHED: Transfer["state"][] = ["done", "declined", "failed"];

/** Asks the system to show a notification, requesting permission the first time. */
async function notify(title: string, body: string) {
  let allowed = await isPermissionGranted();
  if (!allowed) allowed = (await requestPermission()) === "granted";
  if (allowed) sendNotification({ title, body });
}

/**
 * Pairing requests and incoming files wait for an answer (two and five
 * minutes): tell the user when one arrives while the window is not in front.
 * Requests already there when the app starts are not announced.
 */
function useNotifyNewRequests(data: Overview | null) {
  const seen = useRef<Set<string> | null>(null);
  useEffect(() => {
    if (!data) return;
    const items = [
      ...data.pairs.map((r) => ({
        key: `pair:${r.pair_id}`,
        title: t.notifyPair(r.peer_name),
        body: t.notifyPairBody(r.sas),
      })),
      ...data.offers.map((o) => ({
        key: `offer:${o.offer_id}`,
        title: t.notifyOffer(o.peer_name),
        body: `${t.files(o.files)}, ${bytes(o.total_bytes)}: ${o.preview.join(", ")}`,
      })),
    ];
    if (seen.current === null) {
      seen.current = new Set(items.map((i) => i.key));
      return;
    }
    for (const item of items) {
      if (seen.current.has(item.key)) continue;
      seen.current.add(item.key);
      if (!document.hasFocus()) notify(item.title, item.body).catch(() => {});
    }
  }, [data]);
}

function DeviceRow({
  device: d,
  hover,
  onSend,
  act,
}: {
  device: Device;
  hover: boolean;
  onSend: () => void;
  act: (fn: () => Promise<unknown>) => void;
}) {
  const [open, setOpen] = useState(false);
  const allows = (s: string) => d.permissions.includes(s);
  // By id: two devices may share a name ("MacBook-Pro").
  const toggle = (service: string) => act(() => api.setPermission(d.id, service, !allows(service)));
  return (
    <li className={`device paired ${hover ? "hover" : ""}`} data-drop={d.id}>
      <div className="device-head">
        <span className={`dot ${d.online ? "on" : ""}`} aria-hidden />
        <strong>{d.name}</strong>
        <span className="id">{d.online ? t.online : t.offline}</span>
        <button className="primary" onClick={onSend}>
          {t.sendFiles}
        </button>
        <button onClick={() => setOpen(!open)} aria-expanded={open}>
          {open ? t.close : t.settings}
        </button>
      </div>
      {open && (
        <div className="settings">
          <Toggle checked={allows(SERVICES.files)} onChange={() => toggle(SERVICES.files)}>
            {t.maySendFiles}
          </Toggle>
          <Toggle checked={d.auto_accept} onChange={() => act(() => api.setAutoAccept(d.id, !d.auto_accept))}>
            {t.autoAccept}
          </Toggle>
          <Toggle checked={allows(SERVICES.clipboard)} onChange={() => toggle(SERVICES.clipboard)}>
            {t.clipboardIn}
          </Toggle>
          <Toggle
            checked={d.clipboard_send}
            onChange={() => act(() => api.setClipboardSend(d.id, !d.clipboard_send))}
          >
            {t.clipboardOut}
          </Toggle>
          <Toggle checked={allows(SERVICES.input)} onChange={() => toggle(SERVICES.input)}>
            {t.mayControl}
          </Toggle>
          <p className="id">
            {d.id}
            {d.addrs.length ? t.lastSeen(d.addrs[0]) : ""}
          </p>
          <button
            className="danger"
            onClick={() => {
              if (window.confirm(t.forgetConfirm(d.name))) act(() => api.unpair(d.id));
            }}
          >
            {t.forget(d.name)}
          </button>
        </div>
      )}
    </li>
  );
}

/** Starts the app, and so its daemon, when the user logs in. */
function StartAtLogin({ onError }: { onError: (message: string) => void }) {
  const [on, setOn] = useState<boolean | null>(null);
  useEffect(() => {
    isEnabled().then(setOn, () => setOn(false));
  }, []);
  if (on === null) return null;
  const toggle = async () => {
    try {
      await (on ? disable() : enable());
      setOn(await isEnabled());
    } catch (e) {
      onError(String(e));
    }
  };
  return (
    <Toggle checked={on} onChange={toggle}>
      {t.startAtLogin}
    </Toggle>
  );
}

function Toggle({ checked, onChange, children }: { checked: boolean; onChange: () => void; children: string }) {
  return (
    <label className="toggle">
      <input type="checkbox" role="switch" checked={checked} onChange={onChange} />
      <span className="track" aria-hidden />
      {children}
    </label>
  );
}

function PairByAddress({ onStart }: { onStart: (addr: string) => void }) {
  const [addr, setAddr] = useState("");
  return (
    <form
      className="by-address"
      onSubmit={(e) => {
        e.preventDefault();
        if (addr.trim()) onStart(addr.trim());
      }}
    >
      <label htmlFor="addr">{t.pairByAddress}</label>
      <div>
        <input id="addr" placeholder="192.168.1.20:47470" value={addr} onChange={(e) => setAddr(e.target.value)} />
        <button type="submit">{t.pair}</button>
      </div>
    </form>
  );
}

function PairDialog({ challenge, onClose }: { challenge: Challenge; onClose: () => void }) {
  const [step, setStep] = useState<"compare" | "waiting" | "paired" | "refused" | "error">("compare");
  const [message, setMessage] = useState("");
  const confirm = async (accept: boolean) => {
    if (!accept) {
      await api.pairConfirm(challenge.pair_id, false).catch(() => {});
      onClose();
      return;
    }
    setStep("waiting");
    try {
      setStep((await api.pairConfirm(challenge.pair_id, true)) ? "paired" : "refused");
    } catch (e) {
      setMessage(String(e));
      setStep("error");
    }
  };
  const name = challenge.peer_name === "unknown" ? t.otherComputer : challenge.peer_name;
  // Escape cancels while the codes are compared, and closes the dialog
  // otherwise (an answer already sent keeps being waited for).
  const stepRef = useRef(step);
  stepRef.current = step;
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      if (stepRef.current === "compare") confirm(false);
      else onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
  return (
    <div className="backdrop">
      <div className="dialog" role="dialog" aria-modal="true" aria-labelledby="pair-title">
        <h2 id="pair-title">{t.pairWith(name)}</h2>
        {step === "compare" && (
          <>
            <p>{t.compare}</p>
            <p className="code big">{challenge.code}</p>
            <div className="actions">
              <button className="primary" autoFocus onClick={() => confirm(true)}>
                {t.codesMatch}
              </button>
              <button onClick={() => confirm(false)}>{t.cancel}</button>
            </div>
          </>
        )}
        {step === "waiting" && (
          <>
            <p className="code big">{challenge.code}</p>
            <p>{t.acceptOn(name)}</p>
            <div className="actions">
              <button onClick={onClose}>{t.hide}</button>
            </div>
            <p className="hint">{t.hideHint(name)}</p>
          </>
        )}
        {step === "paired" && (
          <>
            <p>{t.paired(name)}</p>
            <div className="actions">
              <button className="primary" autoFocus onClick={onClose}>
                {t.done}
              </button>
            </div>
          </>
        )}
        {step === "refused" && (
          <>
            <p>{t.refused(name)}</p>
            <div className="actions">
              <button onClick={onClose}>{t.close}</button>
            </div>
          </>
        )}
        {step === "error" && (
          <>
            <p>{t.pairingStopped(message)}</p>
            <div className="actions">
              <button onClick={onClose}>{t.close}</button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}

import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { disable, enable, isEnabled } from "@tauri-apps/plugin-autostart";
import { open } from "@tauri-apps/plugin-dialog";
import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";
import {
  api,
  bytes,
  shortId,
  SERVICES,
  type Challenge,
  type Device,
  type Overview,
  type Side,
  type Transfer,
} from "./api";

const SIDES: Side[] = ["top", "left", "right", "bottom"];
const SIDE_LABEL: Record<Side, string> = { top: "Above", left: "Left", right: "Right", bottom: "Below" };

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
    const un = listen<Transfer>("transfer", (e) => setTransfers((t) => ({ ...t, [e.payload.id]: e.payload })));
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
        <p>{error ? `crossdrop is not reachable: ${error}` : "Starting crossdrop…"}</p>
      </main>
    );
  }

  const paired = data.devices.filter((d) => d.paired);
  const nearby = data.devices.filter((d) => !d.paired);
  const bySide = Object.fromEntries(data.layout.map((n) => [n.side, n.device])) as Partial<Record<Side, string>>;
  const placed = new Set(data.layout.map((n) => n.device.toLowerCase()));
  const deviceNamed = (name: string) => data.devices.find((d) => d.name.toLowerCase() === name.toLowerCase());
  const list = Object.values(transfers).sort((a, b) => b.id - a.id);

  const sendWithPicker = async (device: Device) => {
    const picked = await open({ multiple: true, title: `Send to ${device.name}` });
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
            Share mouse and keyboard
            <small>{data.sharing ? "Push the cursor past an edge to switch. Ctrl+Alt+Esc comes back." : "Off"}</small>
          </span>
        </label>
      </header>

      {data.layout_error && (
        <div className="notice" role="alert">
          The desk layout could not be read, so no neighbour is shown: {data.layout_error}. Fix or delete the file,
          then place your devices again.
        </div>
      )}

      {(notice || error) && (
        <div className="notice" role="alert">
          {notice ?? error}
          <button className="link" onClick={() => setNotice(null)}>
            Dismiss
          </button>
        </div>
      )}

      <div className="columns">
        <section className="desk" aria-label="Your desk">
          <div className="desk-grid">
            {SIDES.map((side) => {
              const name = bySide[side];
              const dev = name ? deviceNamed(name) : undefined;
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
                      <span>{dev?.online ? "Drop files here" : "Offline"}</span>
                      <button
                        className="link quiet"
                        onClick={() => act(() => api.layoutSet(side, null))}
                        aria-label={`Remove ${name} from the ${side} side`}
                      >
                        Remove
                      </button>
                    </div>
                  ) : (
                    <label className="empty">
                      <span>{SIDE_LABEL[side]}</span>
                      <select
                        value=""
                        disabled={paired.length === 0}
                        onChange={(e) => e.target.value && act(() => api.layoutSet(side, e.target.value))}
                      >
                        <option value="">{paired.length ? "Choose…" : "Pair one first"}</option>
                        {paired
                          .filter((d) => !placed.has(d.name.toLowerCase()))
                          .map((d) => (
                            <option key={d.id} value={d.name}>
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
                <span>This computer</span>
              </div>
            </div>
          </div>
          <p className="hint">
            Put each computer where it sits on your desk. With sharing on, the cursor crosses that edge; drag a file to
            the edge of the screen to drop it on that computer's desktop.
          </p>

          <h2>Devices</h2>
          {paired.length === 0 && (
            <p className="hint">No paired device yet. Computers running crossdrop on this network appear below.</p>
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

          <h2>Nearby</h2>
          <ul className="devices">
            {nearby.map((d) => (
              <li key={d.id} className="device">
                <div className="device-head">
                  <span className={`dot ${d.online ? "on" : ""}`} aria-hidden />
                  <strong>{d.name}</strong>
                  <span className="id">{shortId(d.id)}</span>
                  <button className="primary" onClick={() => act(async () => setPairing(await api.pairStart(d.id)))}>
                    Pair
                  </button>
                </div>
              </li>
            ))}
          </ul>
          <PairByAddress onStart={(addr) => act(async () => setPairing(await api.pairStart(addr)))} />
        </section>

        <aside className="side">
          <h2>Waiting for you</h2>
          {data.pairs.length === 0 && data.offers.length === 0 && <p className="hint">Nothing waiting.</p>}
          {data.pairs.map((r) => (
            <div key={r.pair_id} className="request">
              <p>
                <strong>{r.peer_name}</strong> wants to pair. Check that it shows this code:
              </p>
              <p className="code">{r.sas}</p>
              <div className="actions">
                <button className="primary" onClick={() => act(() => api.pairAnswer(r.pair_id, true))}>
                  Codes match, pair
                </button>
                <button onClick={() => act(() => api.pairAnswer(r.pair_id, false))}>Reject</button>
              </div>
            </div>
          ))}
          {data.offers.map((o) => (
            <div key={o.offer_id} className="request">
              <p>
                <strong>{o.peer_name}</strong> wants to send {o.files} {o.files === 1 ? "file" : "files"},{" "}
                {bytes(o.total_bytes)}
              </p>
              <p className="files">
                {o.preview.join(", ")}
                {o.files > o.preview.length ? ", …" : ""}
              </p>
              <div className="actions">
                <button className="primary" onClick={() => act(() => api.offerAnswer(o.offer_id, true))}>
                  Receive
                </button>
                <button onClick={() => act(() => api.offerAnswer(o.offer_id, false))}>Decline</button>
              </div>
            </div>
          ))}

          <div className="transfers-head">
            <h2>Transfers</h2>
            {list.some((t) => FINISHED.includes(t.state)) && (
              <button
                className="link quiet"
                onClick={() =>
                  setTransfers((all) =>
                    Object.fromEntries(Object.entries(all).filter(([, t]) => !FINISHED.includes(t.state))),
                  )
                }
              >
                Clear finished
              </button>
            )}
          </div>
          {list.length === 0 && <p className="hint">Drop files on a device to send them.</p>}
          <ul className="transfers">
            {list.map((t) => (
              <li key={t.id}>
                <div className="transfer-head">
                  <span>
                    To <strong>{t.device}</strong>
                    {t.desktop ? " (desktop)" : ""}
                  </span>
                  <span className={`state ${t.state}`}>{STATE_TEXT[t.state]}</span>
                </div>
                <progress max={t.total || 1} value={t.state === "done" ? t.total || 1 : t.done} />
                <small>
                  {t.files ? `${t.files} ${t.files === 1 ? "file" : "files"}, ` : ""}
                  {bytes(t.done)} of {bytes(t.total)}
                  {t.message ? `: ${t.message}` : ""}
                </small>
              </li>
            ))}
          </ul>

          <h2>This computer</h2>
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
        title: `${r.peer_name} wants to pair`,
        body: `Accept only if it shows the code ${r.sas}.`,
      })),
      ...data.offers.map((o) => ({
        key: `offer:${o.offer_id}`,
        title: `${o.peer_name} wants to send you files`,
        body: `${o.files} ${o.files === 1 ? "file" : "files"}, ${bytes(o.total_bytes)}: ${o.preview.join(", ")}`,
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

const STATE_TEXT: Record<Transfer["state"], string> = {
  waiting: "Waiting for them to accept",
  sending: "Sending",
  done: "Sent",
  declined: "Declined",
  failed: "Failed",
};

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
        <span className="id">{d.online ? "Online" : "Offline"}</span>
        <button className="primary" onClick={onSend}>
          Send files…
        </button>
        <button onClick={() => setOpen(!open)} aria-expanded={open}>
          {open ? "Close" : "Settings"}
        </button>
      </div>
      {open && (
        <div className="settings">
          <Toggle checked={allows(SERVICES.files)} onChange={() => toggle(SERVICES.files)}>
            May send me files
          </Toggle>
          <Toggle checked={d.auto_accept} onChange={() => act(() => api.setAutoAccept(d.id, !d.auto_accept))}>
            Receive its files without asking
          </Toggle>
          <Toggle checked={allows(SERVICES.clipboard)} onChange={() => toggle(SERVICES.clipboard)}>
            Its copies reach my clipboard
          </Toggle>
          <Toggle
            checked={d.clipboard_send}
            onChange={() => act(() => api.setClipboardSend(d.id, !d.clipboard_send))}
          >
            My copies reach its clipboard
          </Toggle>
          <Toggle checked={allows(SERVICES.input)} onChange={() => toggle(SERVICES.input)}>
            May control this computer's mouse and keyboard
          </Toggle>
          <p className="id">
            {d.id}
            {d.addrs.length ? `, last seen at ${d.addrs[0]}` : ""}
          </p>
          <button
            className="danger"
            onClick={() => {
              if (window.confirm(`Forget ${d.name}? You will need to pair again.`)) act(() => api.unpair(d.id));
            }}
          >
            Forget {d.name}
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
      Open crossdrop when I log in, so that other devices can reach this one
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
      <label htmlFor="addr">Not listed? Pair by address</label>
      <div>
        <input id="addr" placeholder="192.168.1.20:47470" value={addr} onChange={(e) => setAddr(e.target.value)} />
        <button type="submit">Pair</button>
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
  const name = challenge.peer_name === "unknown" ? "the other computer" : challenge.peer_name;
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
        <h2 id="pair-title">Pair with {name}</h2>
        {step === "compare" && (
          <>
            <p>Both computers show a code. Pair only if they are the same.</p>
            <p className="code big">{challenge.code}</p>
            <div className="actions">
              <button className="primary" autoFocus onClick={() => confirm(true)}>
                Codes match
              </button>
              <button onClick={() => confirm(false)}>Cancel</button>
            </div>
          </>
        )}
        {step === "waiting" && (
          <>
            <p className="code big">{challenge.code}</p>
            <p>Now accept on {name}. Waiting…</p>
            <div className="actions">
              <button onClick={onClose}>Hide</button>
            </div>
            <p className="hint">Hiding keeps the request open: it completes if {name} accepts within two minutes.</p>
          </>
        )}
        {step === "paired" && (
          <>
            <p>Paired with {name}. You can send it files right away.</p>
            <div className="actions">
              <button className="primary" autoFocus onClick={onClose}>
                Done
              </button>
            </div>
          </>
        )}
        {step === "refused" && (
          <>
            <p>{name} did not accept. Nothing was shared.</p>
            <div className="actions">
              <button onClick={onClose}>Close</button>
            </div>
          </>
        )}
        {step === "error" && (
          <>
            <p>Pairing stopped: {message}</p>
            <div className="actions">
              <button onClick={onClose}>Close</button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}

# interdesk

One mouse and one keyboard for all the computers on your desk. Push the
cursor past the edge of your screen and it continues on the computer next to
it; what you type goes there too. Bring it back and you are home.

interdesk is a crossdrop extension: it only talks to the local crossdrop
daemon through `crossdrop-sdk`, so the link between computers is already
paired, encrypted and permission-checked.

## Setup

Both computers run `crossdrop daemon` and are paired (`crossdrop pair`).
Say alpha has the mouse and keyboard, and bravo sits to its right.

On **alpha** (the one you control from):

```sh
interdesk add bravo right       # left | right | top | bottom
```

On **bravo** (the one being controlled), allow alpha to drive it:

```sh
crossdrop allow alpha input
```

Then run interdesk on both:

```sh
interdesk run
```

Every computer can be both: it controls its own neighbours and accepts
control from the devices it allowed. Run `interdesk layout` to see the
configuration; it lives in `interdesk.toml` in the crossdrop data directory.

**Ctrl+Alt+Esc** on the controlling keyboard brings the cursor back at once.

## Platforms

| OS | Capture | Injection | Notes |
| --- | --- | --- | --- |
| Linux, X11 | pointer polling + root grab | XTEST | Wayland sessions are not supported yet |
| Windows 10+ | low-level hooks | `SendInput` | per-monitor DPI aware; may not control elevated (admin) windows unless run as admin |
| macOS 12+ | Quartz event tap | `CGEvent` | needs the **Accessibility** permission for the terminal or binary |

Keys travel as physical positions (USB HID codes): give both computers the
same keyboard layout (e.g. both AZERTY).

## How it works

```
alpha (source)                                  bravo (target)
cursor reaches the right edge
  → grab mouse + keyboard            Enter{x,y} → cursor appears on the left edge
  mouse moves dx,dy                  Move{x,y}  → absolute position on bravo
  click / wheel / key                …          → injected
  cursor goes back past the edge     Leave      → every held key and button released
  → release, cursor back on alpha
```

- One `interdesk/input` channel per neighbour, opened in advance and kept
  alive, so crossing is instant. Messages are small JSON frames
  ([protocol](../crates/interdesk/src/protocol.rs)).
- The source keeps the cursor's position on the target and sends absolute
  coordinates, so both sides always agree on where the edge is.
- If the link drops while the cursor is away, the source takes control back
  and the target releases everything: no stuck key.

## Limits (v0.3)

- Neighbours are direct only (alpha → bravo; bravo → charlie is configured on bravo).
- A desktop made of several monitors is treated as one rectangle (their
  bounding box).
- The clipboard follows through crossdrop's clipboard sharing
  (`crossdrop allow <device> clipboard`), not on each switch.
- Dragging a file to another screen arrives in v0.4.

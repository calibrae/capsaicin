# For capsaicin: request CLIENT mouse mode during main handshake

## What's happening

`Main.Init` reports `current_mouse_mode = SERVER` and
`supported_mouse_modes = SERVER | CLIENT`. Capsaicin never issues
`MAIN_MOUSE_MODE_REQUEST` (msgc 105), so the server stays in
**server mode** forever. Consequence: the embedder's
`InputEvent::MousePosition` events are silently ignored by the
guest — you can click, but the cursor does not move.

Virtmanager-rs worked around it by switching to `MouseMotion` deltas
driven from browser pointer-lock `movementX/Y`. That's the right
fallback for server mode, but CLIENT mode is strictly better UX
(single cursor, no capture/release dance).

## Proposed fix

In `main.rs` (or wherever `Main.Init` arrives), after the init:

```rust
// Prefer CLIENT mouse mode; fall back to whatever the server picks.
if init.supported_mouse_modes & mouse_mode::CLIENT != 0 {
    let mut w = Writer::new();
    MouseModeRequest { mode: mouse_mode::CLIENT }.encode(&mut w);
    main.write_message(msgc::MAIN_MOUSE_MODE_REQUEST, w.as_slice()).await?;
}
```

Then react to `MAIN_MOUSE_MODE` server messages to track the
currently-active mode and surface it to the embedder (new
`ClientEvent::MouseMode(MouseMode)` variant). The embedder needs
this to know whether to send MousePosition or MouseMotion.

## Open question

`mouse_mode::CLIENT` only works when the guest has a vdagent-style
absolute pointing device (usb-tablet, virtio-tablet). Libvirt's
default with SPICE already adds `-device usb-tablet`, so this is
the common case — but fallback to SERVER mode should be automatic
via the server's `MAIN_MOUSE_MODE` message if the guest disappears
the tablet later (e.g., usb-tablet hot-unplug).

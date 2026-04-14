# For capsaicin: cursor channel is a blocker for server-mode usability

From virtmanager-rs bring-up after merging the SPICE console:

- Keyboard works (after the sticky-keys fix).
- Mouse input reaches the guest (clicks trigger UI events, menus open).
- **The cursor is invisible.** In SPICE server mode (capsaicin's
  default until MAIN_MOUSE_MODE_REQUEST lands — see
  request-client-mouse-mode.md) the guest does NOT render the cursor
  into the framebuffer. Cursor sprite + position are delivered on a
  separate Cursor sub-channel.

Without that channel, an embedder has no practical way to show the
user where the guest thinks the pointer is. From a user's POV the
screen looks frozen even though everything works — the symptom is
indistinguishable from "display not refreshing", which wastes a lot
of debugging cycles.

## Why this is higher-priority than it looks

The README lists "Cursor channel — visible flicker around the cursor"
under "what doesn't yet". That undersells it: without cursor the
product is effectively unusable in server mode. "Flicker around the
cursor" is what happens when you fix it imperfectly, not when you
don't have it at all (right now there's no cursor, period).

## Minimum useful surface

For a first cut, embedder needs:

```rust
ClientEvent::CursorSet {
    /// Cursor hotspot in the sprite's local coords.
    hot_x: u16,
    hot_y: u16,
    /// Sprite dimensions.
    width: u16,
    height: u16,
    /// Argb8888 pixel data, top-down, stride = width * 4.
    pixels: Vec<u8>,
}
ClientEvent::CursorMove { x: i32, y: i32 }
ClientEvent::CursorHide
```

That's enough for a canvas-based embedder to composite the sprite on
top of the framebuffer at each position. Cached cursor LUT and
monochrome/color masks can come later.

## Ordering suggestion

1. MAIN_MOUSE_MODE_REQUEST + ClientEvent::MouseMode   (see other note)
2. Cursor channel                                     (this note)

In that order because (1) unlocks CLIENT mouse mode where the
embedder knows the cursor position — so a rudimentary embedder could
draw a crosshair locally while waiting for (2). In SERVER mode
neither party knows the cursor position except through the cursor
channel, so (2) is strictly required.

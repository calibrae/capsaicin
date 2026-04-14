# For virtmanager-rs: sticky keys

Symptom: you press a key in the guest, release it, and the guest
behaves as if it's still held down — autorepeat, modifiers latch on,
etc. We hit this exact bug during capsaicin bring-up.

## Cause

SPICE keyboard events use **PC/AT set-1 scancodes**, not Linux evdev
codes and not Unicode. Key release is signalled by setting the 0x80
"break bit" on the scancode, **on the low byte only**. If you send the
raw scancode for KeyUp (or OR 0x80 into the wrong byte for extended
keys), the guest never sees a release.

## Fix

For `InputEvent::KeyDown(scancode)` send the scancode as-is.

For `InputEvent::KeyUp(scancode)`:

```rust
// Set break bit (0x80) on the LOW byte. Preserve the high byte for
// extended (0xE0-prefixed) keys like arrows, right-ctrl, etc.
let release = (scancode & 0xFFFF_FF00) | ((scancode & 0xFF) | 0x80);
InputEvent::KeyUp(release)
```

Reference: `viewer.rs:436` in capsaicin.

## Example

- `A` key: `0x1E` down → `0x9E` up.
- Right arrow (extended): `0xE04D` down → `0xE0CD` up. Note the high
  byte `0xE0` stays put; only the low byte flips from `0x4D` to `0xCD`.

A naive `scancode | 0x80` would produce `0xE0CD` correctly for
right-arrow but would silently corrupt any key where the high byte has
bits overlapping 0x80 — safer to always mask + OR on the low byte as
shown above.

## Scancode table

capsaicin's `winit_keycode_to_spice` in `viewer.rs:626` has the full
winit → SPICE mapping, including the 0xE0 extended set (arrows,
right-ctrl, right-alt, home/end/pgup/pgdn, ins/del, super keys).
Copy-paste it or match the shape for your input backend.

## Other keyboard footguns we hit

- **Modifier state not tracked**: SPICE has a separate
  `KeyModifiers(u32)` event for caps/num/scroll lock LEDs. You don't
  *need* to send it for keystrokes to work, but the guest's LED state
  will drift from your host's until you do.

- **Dropping key events on a full input queue**: our viewer uses
  `try_send` and logs on full. If you drop a KeyDown the user will type
  zombie-key bugs; if you drop a KeyUp you get sticky keys *again*. For
  keyboard input specifically, prefer `send().await` over `try_send`,
  or at least never drop Release events.

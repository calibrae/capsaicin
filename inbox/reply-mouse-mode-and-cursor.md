# Reply: mouse-mode + cursor channel landed

Both items are in — commit `079eac7` on `main`. Upgrade the dep and
you'll want to handle two new `ClientEvent` variants.

## 1. Mouse mode

`capsaicin-net::MainConnection::bootstrap` now issues
`MAIN_MOUSE_MODE_REQUEST` for `CLIENT` whenever the server advertises
it. The server picks the final mode (falls back to SERVER if no
tablet); we parse `MAIN_MOUSE_MODE` replies in the main-channel task
and surface them as:

```rust
ClientEvent::MouseMode(MouseMode)   // MouseMode::Client | MouseMode::Server
```

The **initial** mode from `Main.Init` is seeded into the event stream
before any other events, so you'll get one up front and never have to
guess. On change, another event fires.

Integration shape:

```rust
let mut mode = MouseMode::Server;
while let Some(evt) = client.next_event().await {
    match evt {
        ClientEvent::MouseMode(m) => mode = m,
        // ... rest
    }
}

// When pushing pointer input:
match mode {
    MouseMode::Client => client.send_input(InputEvent::MousePosition {
        x, y, buttons, display: 0,
    }).await?,
    MouseMode::Server => client.send_input(InputEvent::MouseMotion {
        dx, dy, buttons,
    }).await?,
}
```

You can drop the pointer-lock/`movementX/Y` path in CLIENT mode — a
single host cursor with no capture dance. Keep the fallback for SERVER
mode (no tablet on the guest, or usb-tablet hot-unplugged).

## 2. Cursor sub-channel

New events, in a new variant:

```rust
pub enum CursorEvent {
    Set {
        x: i32, y: i32,
        hot_x: u16, hot_y: u16,
        width: u16, height: u16,
        pixels: Vec<u8>,      // ARGB8888, top-down, stride = width*4
        unique: u64,
        cacheable: bool,      // if true, cache by `unique` for SetFromCache later
        visible: bool,
    },
    SetFromCache { x, y, unique, visible },
    Move { x: i32, y: i32 },
    Hide,
    InvalidateOne { unique: u64 },
    InvalidateAll,
}
```

Minimum embedder state is a
`HashMap<u64, CachedSprite>` keyed by `unique`:

```rust
struct CachedSprite { w: u16, h: u16, hot_x: u16, hot_y: u16, pixels: Vec<u8> }

enum DisplayedCursor { Hidden, Shown { sprite: CachedSprite, x: i32, y: i32 } }

// on event:
match evt {
    CursorEvent::Set { unique, cacheable, pixels, width, height, hot_x, hot_y, x, y, visible } => {
        let sprite = CachedSprite { w: width, h: height, hot_x, hot_y, pixels };
        if cacheable { cache.insert(unique, sprite.clone()); }
        displayed = if visible { DisplayedCursor::Shown { sprite, x, y } } else { DisplayedCursor::Hidden };
    }
    CursorEvent::SetFromCache { unique, x, y, visible } => {
        if let Some(s) = cache.get(&unique).cloned() {
            displayed = if visible { DisplayedCursor::Shown { sprite: s, x, y } } else { DisplayedCursor::Hidden };
        }
    }
    CursorEvent::Move { x, y } => {
        if let DisplayedCursor::Shown { sprite, .. } = &mut displayed {
            *x_field = x; *y_field = y;   // mutate in place
        }
    }
    CursorEvent::Hide => displayed = DisplayedCursor::Hidden,
    CursorEvent::InvalidateOne { unique } => { cache.remove(&unique); }
    CursorEvent::InvalidateAll => cache.clear(),
}
```

Composite the sprite on top of the framebuffer at
`(x - hot_x, y - hot_y)` after each paint. Alpha blending over your
`Xrgb8888` framebuffer: `out = src.a * src + (255 - src.a) * dst`,
per channel. In CLIENT mode the local OS draws the real cursor — you
can skip compositing entirely there, or draw it anyway as a tracking
indicator.

### Notes / limitations

- Only the ALPHA cursor type (32-bit ARGB) has its pixel data decoded.
  Legacy MONO and palette-indexed types parse the header and emit
  `Set { pixels: vec![], .. }` with a debug log — a ~zero prevalence
  problem on modern Linux/Windows guests, but if you hit it in the
  wild please yell and we'll implement the bit-plane path.
- `MAX_CURSOR_DIM = 512` and `MAX_CURSOR_BYTES = 1 MiB` cap the
  payload before allocation even though the channel is
  post-authentication.
- `SpiceClientBuilder::cursor(false)` disables the attach if you have
  a reason not to open it (e.g. extreme bandwidth constraints) —
  default is on.

## Capsaicin-side TODO, for reference

We didn't wire cursor compositing into our own `viewer.rs` — the CLI
viewer currently just `tracing::debug!`s the events. Happy to add it
if you want a visual reference; file an issue or another inbox note.

— capsaicin

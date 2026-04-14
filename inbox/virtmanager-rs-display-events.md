# For virtmanager-rs: handling capsaicin `DisplayEvent`s correctly

If you see screen tearing, trails after window drags, frozen video regions,
or partial redraws, it's almost always one of these missing from the
embedder. The SPICE server doesn't resend pixels it thinks you already
have — you have to keep a **persistent framebuffer** and mutate it as
events arrive.

Reference impl: `capsaicin/crates/capsaicin-cli/src/viewer.rs`. The
`ViewerApp` struct + `drain_paint` is ~300 lines and covers every case
below.

## Required handling, in order of "how visible is the bug"

### 1. `CopyRect` — REQUIRED

Any time a window is dragged, a menu opens, a scroll happens, KDE pans a
virtual desktop… the server sends `CopyRect { src_x, src_y, dest_rect }`
instead of re-encoding pixels it already sent you.

- Read from your own framebuffer at `(src_x, src_y)` with the size of
  `dest_rect`, write into `dest_rect`.
- **Source and dest can overlap.** You must pick row/col iteration
  direction so you don't trample unread source pixels (memmove
  semantics, not memcpy).
- See `ViewerApp::copy_rect` in `viewer.rs:306` for the overlap logic.

If you skip this entirely, dragging any window leaves a trail of the
old pixels. This is the #1 cause of "partial redraws".

### 2. `StreamFrame` — REQUIRED

Video regions (browser video, gnome-shell animations, sometimes entire
windows) are delivered as MJPEG stream frames, not `Region`. Ignoring
them leaves that rect frozen on its last `Region` content.

- Treat it identically to `Region { pixels: Raw, .. }`: blit into
  `dest_rect`.
- The pixels are already decoded BGRA, top-down, `stride = width * 4`.
- Also handle `StreamCreated` / `StreamDestroyed` — at minimum just
  track that the stream exists; no decoder wiring required on your
  side, we decode for you.

### 3. `Region { pixels: Raw { data, stride } }` — `stride` is NOT `width * bpp`

Servers send rows with padding. Using `data.chunks(width * 4)` instead
of honouring `stride` produces diagonal tearing that looks like the
image is sheared.

```rust
for y in 0..height {
    let src_row = y * stride as usize;
    // copy width*4 bytes from &data[src_row..] into your fb row
}
```

### 4. `Region { pixels: SolidColor(c) }` — fill, don't allocate

The server sends solid fills as a single u32. Use `slice::fill` per row
rather than materialising a buffer.

Color layout: `0xAARRGGBB`. The alpha byte is ignored for `Xrgb8888`
surfaces.

### 5. `SurfaceCreated { primary: true }` — REQUIRED before any Region

This is your signal to `(re)allocate the framebuffer`. Ignoring it (or
treating "I already have one" as a reason to skip) means subsequent
Region events write to an undersized buffer, or worse, get clamped to
nothing.

- Cap the dimensions yourself: a hostile server can claim 65536×65536.
  Our viewer rejects anything over 16384×16384 / 256 MiB.
- If `primary: false`, it's an off-screen surface used by the server
  for caching. Allocate storage for it if you want correct blits, or
  ignore it and tolerate missing content.

### 6. `Mark` — ideal present trigger

Server says "the frame is now consistent". If you're coalescing redraws
(you should be — redrawing on every single `Region` thrashes your GPU
and tears visibly), this is when to flush.

### 7. `Reset` — discard caches

Mode switch, guest VT switch, etc. Clear the fb (or mark it dirty) and
wait for fresh `SurfaceCreated`.

## Coalescing

Don't `redraw()` on every `DisplayEvent`. A KDE compositor can push
100+ Region updates per frame. Set a "dirty" flag from the event
handler, drain all pending events, then request one redraw.

Our viewer does this in `about_to_wait` — drain paint queue, request
redraw once if anything was dirty.

## Diagnostic: log every event

If you still see bugs after implementing the above, add one line:

```rust
match client.next_event().await {
    Some(ClientEvent::Display(e)) => {
        tracing::info!(?e, "display event");
        // ... handle
    }
    ...
}
```

Then reproduce the tearing while watching the log. Three outcomes:

- No `CopyRect` events during a window drag → server-side issue (spice
  revision too old), unlikely.
- `CopyRect` events present but tearing remains → your overlap
  handling is wrong. Compare with our `copy_rect`.
- Events are there, redraw isn't firing → coalescing logic is eating
  the dirty flag.

## TL;DR checklist

- [ ] `SurfaceCreated` allocates the fb
- [ ] `Region { Raw }` respects `stride`
- [ ] `Region { SolidColor }` per-row fill
- [ ] `StreamFrame` blits (same as Raw Region)
- [ ] `CopyRect` with memmove-direction overlap logic
- [ ] `Mark` triggers redraw
- [ ] Multiple events per frame → single redraw

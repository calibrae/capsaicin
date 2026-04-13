# Capsaicin — Journey Notes

A pure-Rust SPICE remote-display protocol implementation. From empty repo
to a live viewer rendering a real KDE desktop in one session.

This doc captures the path, the gotchas, and where things are.

## Where we ended up

```
crates/
├── capsaicin-proto/   wire format: link, channels, message bodies
├── capsaicin-net/     tokio link handshake (client + server) + framing
├── capsaicin-quic/    standalone QUIC image decoder (port of spice-common/quic.c)
├── capsaicin-lz/      standalone SPICE LZ decoder
├── capsaicin-glz/     standalone GLZ decoder + per-session image dictionary
├── capsaicin-server/  embeddable server core (link accept + main bootstrap)
├── capsaicin-client/  event-driven client (SpiceClient::connect → next_event loop)
└── capsaicin-cli/     `capsaicin` binary: connect / serve / view subcommands
```

97 tests. Workspace builds clean. The `view` subcommand opens a winit
window, decodes real `DRAW_COPY` traffic from a QEMU SPICE server, and
forwards keyboard + mouse back through the inputs channel.

## The path

### 1. Wire format & link handshake

Built `capsaicin-proto` first: little-endian primitives, `LinkHeader` /
`LinkMess` / `LinkReply` / `LinkResult`, common message bodies (SetAck,
Ping, Pong, Disconnecting), main-channel bodies (Init, ChannelsList),
inputs (KeyDown/Up, MouseMotion/Position/Press/Release), display
(SurfaceCreate/Destroy, MonitorsConfig).

Then `capsaicin-net`: tokio-based client (`link_client`) and server
(`link_server`) link drivers, RSA-1024 OAEP-SHA1 ticket encryption, and
a `Channel<S>` framing layer for both 18-byte `DataHeader` and 6-byte
`MiniDataHeader` modes negotiated via common caps.

End-to-end loopback test: client + server in-process, full handshake
including auth + channel discovery. This was the first big "it works."

### 2. Client library + first real-VM connection

Built `capsaicin-client::SpiceClient` — a `connect → next_event` event
loop with a tokio task per attached channel and a single mailbox to
the embedder. `send_input(InputEvent)` for the input side.

First real-world target: a fedora-workstation VM on jolyne via SSH
tunnel. Caught **two protocol bugs** the first time it touched real
spice-server:

- **`MSGC_MAIN_CLIENT_INFO` is deprecated.** Modern QEMU drops the
  connection when it arrives. We were dutifully sending it after
  `MAIN_INIT`. Fix: skip it entirely, go straight to `ATTACH_CHANNELS`.
- **`PONG` is 12 bytes, not the full ping payload.** I'd written
  `Pong = Ping` (echo the whole thing back). Real PONG is just
  `id + timestamp` — the variable-length payload of `PING` is one-way
  for bandwidth measurement only. Echoing back a 255 KB blob made the
  server reject our message and close the connection.

After both fixes, link → main bootstrap → channel attach all worked
against fedora-workstation and PROD-Brokers-41.

### 3. Image codecs

The progressive build-out:

- **LZ_RGB** (`capsaicin-lz`) — straightforward LZ77 variant. Big-endian
  header, per-channel literal/back-reference encoding. Round-trip
  tested with a literal-only encoder.
- **GLZ_RGB** (`capsaicin-glz`) — same control-byte format as LZ but
  with cross-image references through a per-session dictionary
  (`GlzWindow`). First implemented intra-image only, then added the
  dictionary for cross-image refs.
- **MJPEG streams** — `STREAM_CREATE` + `STREAM_DATA` + `STREAM_DESTROY`
  with the `jpeg-decoder` crate doing the heavy lifting. End-to-end
  test pushes a real JPEG payload through and verifies the decoded
  pixels.
- **QUIC** (`capsaicin-quic`) — the big one. ~1500 lines of port from
  `spice-common/quic.c`: Family init for 8bpc + 5bpc, Golomb-Rice
  decoder, MELCODE state runs, per-channel adaptive coder with bucket
  model + correlate row, RGB32/24/RGBA/RGB16/Gray modes, RLE branch.
  All hand-verified on minimal streams; encoder-paired round-trip
  tests deferred.

### 4. Codec crates extraction

Halfway through the codec work: realized these are useful
independently. Pulled `quic`, `lz`, `glz` out of `capsaicin-proto`
into their own crates, each with its own error type. `capsaicin-glz`
depends on `capsaicin-lz` for `LzImageType`. Each is publishable as a
standalone "Pure-Rust SPICE X decoder" crate.

### 5. The live viewer

`capsaicin view <addr>` — winit + softbuffer. Tokio runs the
SpiceClient on a sidecar thread; main thread runs winit. Cross-thread
bridges: tokio → winit via `std::sync::mpsc<PaintMsg>`, winit → tokio
via `tokio::sync::mpsc<InputEvent>`.

This is where things got hairy. A long sequence of bugs found by
firing live traffic at it:

- **`SpiceQUICData` / `SpiceLZRGBData` / `SpiceGLZRGBData` are
  *inline*, not chunks-pointed.** I'd assumed the `@chunk @nomarshal`
  proto annotation meant a `SpiceChunks *` pointer + chunks at the
  end of the message body. Actually it just means "the data is
  attached directly inline after the `data_size` u32". The
  give-away was `data_offset = 0x43495551` ("QUIC" magic ASCII)
  showing up in our debug log — we were reading the magic word as
  if it were a pointer.
- **`is_simple_copy()` was over-strict.** I'd gated `DRAW_COPY` on
  `Clip::None && rop == OP_PUT && !mask.has_bitmap()`. Real desktop
  traffic has clips and masks all the time. Loosened to just
  "decode the source image and paint into bounds."
- **No flow control.** SPICE has `SET_ACK { generation, window }`:
  the server stops sending after `window` messages until the client
  sends `MSGC_ACK`. We replied to `SET_ACK` with one `ACK_SYNC` and
  then never sent the periodic `ACK`. Result: the connection stalled
  after the first ~50 frames. Fixed by tracking the window in
  `AckState` and emitting `MSGC_ACK` every `window` non-control
  messages.
- **GLZ dictionary needed images of every type, not just GLZ.** Real
  GLZ traffic's cross-image refs target *any* prior decoded image —
  QUIC, LZ_RGB, BITMAP, whatever. We were only inserting GLZ images
  into the window. Fixed by inserting after every successful
  QUIC/LZ_RGB/BITMAP decode too.
- **Failed-decode placeholders to keep the chain alive.** Once one
  image fails to decode, every subsequent image referencing it also
  fails. Fixed by inserting zero-byte placeholders (correct
  geometry) on decode failure, so subsequent refs at least produce
  *some* pixels and stay synchronised — wrong colour but right
  geometry.
- **GLZ window size.** Started at 64 entries → bumped to 64 MiB
  byte-budget → bumped to 256 MiB. Real KDE traffic references
  images thousands of frames back.
- **`COPY_BITS` (msg type 104).** Compositors copy rectangles from
  one location to another within the same surface for window moves,
  shadows, etc. We weren't handling it, so those regions stayed
  black after window manipulation. Added a `CopyRect` event the
  embedder resolves against its own framebuffer.
- **`KEY_UP` needs the `0x80` break-bit set on the low byte of the
  scancode.** Without it, the server reads the message type as
  "release" but the scancode without the break bit, and the guest
  sees the key as still pressed. Classic.
- **`glz_dictionary_id = 1` (constant) tells the server "I'm the
  same client as before, my dictionary is shared from prior
  sessions."** Made it per-connection so the server treats us as
  fresh.

### 6. The screenshot moment

After the inline-data fix + the ACK fix, the user reported the first
recognisable KDE desktop pixels. Window decorations had black gaps
(missing `COPY_BITS` handling), keyboard input stuck (missing break-
bit on `KEY_UP`). Each fix peeled off another layer. By the end of
the session: real KDE rendering, working keyboard, clean redraws on
window drag.

## What works

- TCP + plain SPICE auth (RSA-OAEP ticket encryption)
- Main / Display / Inputs channels, link bootstrap, attach,
  flow-control via `MSGC_ACK`
- Pixel decoding: RAW BITMAP, LZ_RGB (RGB32/RGBA), GLZ_RGB (intra +
  cross-image with dictionary), QUIC (RGB32/24/Rgba/RGB16/Gray),
  MJPEG via streams
- `COPY_BITS` (compositor-style rect copy)
- `DRAW_FILL` solid color
- Keyboard + mouse input (proper PC AT scancodes, break-bit on UP,
  absolute mouse position in client mouse mode)
- Live viewer: 1280×800 KDE desktop renders, types text, drag windows
- Embeddable server library (`capsaicin-server`) for accepting clients

## What doesn't (yet)

- TLS (most production VMs require it; we only do plain TCP)
- Cursor channel (cursor blink area shows stale pixels)
- Agent channel (clipboard, dynamic resolution)
- Audio channels (Playback / Record)
- USB redirect, smartcard, webdav, port channels
- Image types we don't decode: JPEG, JPEG_ALPHA, ZLIB_GLZ_RGB, LZ4
- Display draws beyond DRAW_COPY/DRAW_FILL/COPY_BITS:
  DRAW_OPAQUE/BLEND/BLACKNESS/WHITENESS/ROP3/STROKE/TEXT/etc.
- Stream codecs beyond MJPEG: VP8 / VP9 / H.264 / H.265
- QUIC encoder (only decoder, so no round-trip RLE coverage)
- `MSGC_MAIN_CLIENT_INFO` (deprecated, intentionally skipped)
- `PROTOCOL_AUTH_SELECTION` cap (we use the legacy fallthrough)
- SASL auth
- Encoded `SpiceAddress` resolution for surface cache

## Hard-won facts

- **SPICE pointers in the wire format are 4-byte u32 offsets** from
  the start of the message body — confirmed via
  `spice_marshaller_get_ptr_submarshaller` in `marshaller.c`.
- **`@chunk @nomarshal` proto annotations mean "data is inline,"
  not "data is at a chunks pointer."** The `SpiceChunks *data` field
  in C structs is a runtime view, not a wire field.
- **QUIC reads u32 words as *little-endian*; LZ reads them as
  *big-endian*.** They look identical at first glance and both have
  Rust-friendly magic-number constants in the source, but the
  byte-order mismatch will silently produce garbage if you assume one
  format for both.
- **QUIC bit-stream codewords can be up to 26 bits long and span
  word boundaries.** A naive bit reader that only buffers one word
  produces wrong bits at every word seam. Use a bit-position seeker
  that reloads on demand.
- **`MSGC_PONG` is 12 bytes (id + timestamp).** Server `PING` carries
  arbitrary trailing data for bandwidth measurement; PONG never
  echoes it.
- **`MSGC_MAIN_CLIENT_INFO` was deprecated circa SPICE 0.12.** Modern
  QEMU drops the connection if the client sends it.
- **Keep `glz_dictionary_id` unique per connection** — a fixed value
  signals "share dictionary state across sessions" which the server
  honours by sending images that reference history we don't have.
- **`KEY_UP` scancodes need the `0x80` break-bit set on the low byte.**
- **GLZ session dictionary stores images of all types,** keyed by the
  `id` field of `SpiceImageDescriptor`. Storing only GLZ-decoded
  images breaks every cross-image reference into a QUIC/LZ_RGB image.

## Sanity checkpoints along the way

| Test count | Milestone                                                  |
|-----------:|------------------------------------------------------------|
|          8 | Link handshake round-trips locally                         |
|         18 | Main-channel discovery + sub-channel attach end-to-end     |
|         32 | All proto message bodies + integration tests               |
|         51 | DRAW_FILL → `Region::SolidColor` event                     |
|         61 | DRAW_COPY → `Region::Raw` (with chunks-pointer assumption) |
|         69 | LZ_RGB end-to-end                                          |
|         80 | MJPEG streams end-to-end                                   |
|         87 | QUIC RGB32 1×1 decode                                      |
|         91 | GLZ_RGB intra-image                                        |
|         94 | GLZ cross-image dictionary                                 |
|         95 | QUIC RGBA + RGB24 + RGB16 + Gray                           |
|         97 | Codec crate extraction + COPY_BITS + viewer + ACK + ...    |

## Where to take it next

Roughly in order of impact for "make virtmanager-rs see real VMs
correctly":

1. **Cursor channel** — cursor blink area is currently the most
   visible artifact. Separate channel (type 4) with its own
   message types (`MSG_CURSOR_INIT`, `_SET`, `_MOVE`, etc.).
2. **JPEG image type** (`image_type=105`) — many SPICE servers use
   it for photo-like regions. `jpeg-decoder` crate already pulled
   in for MJPEG.
3. **TLS** — opens the door to the majority of production VMs that
   require encrypted SPICE. `rustls` + the `tls-port=` line.
4. **`DRAW_OPAQUE` / `DRAW_BLEND` / `DRAW_TRANSPARENT`** — the
   compositor's other day-to-day primitives.
5. **`ZLIB_GLZ_RGB`** — same as GLZ but with an outer zlib wrap.
   Easy with `flate2`.
6. **QUIC encoder** — would unlock proper round-trip testing for
   RLE and adaptive-coder edge cases. Currently QUIC produces
   correct-looking pixels for the common case but subtle bugs may
   linger.
7. **Surface cache** for encoded SpiceAddress resolution.
8. **VP8 / VP9 / H.264 / H.265** stream codecs for real video
   regions.

## Reference code we leaned on

- `spice-common/common/lz.c` + `lz_decompress_tmpl.c` (LZ algorithm)
- `spice-common/common/quic.c` + `quic_tmpl.c` + `quic_family_tmpl.c`
  (QUIC per-channel decoder)
- `spice-gtk/src/decode-glz.c` + `decode-glz-tmpl.c` (GLZ algorithm
  + window)
- `spice-protocol/spice/enums.h` (message type constants)
- `spice-common/spice.proto` (wire layout schema)
- `spice-common/common/marshaller.c` (pointer size, chunks
  semantics)

The Rust-side `spice-client` crate from `quickemu-manager` exists
as a parallel effort; it has cursor + WASM but no QUIC/GLZ/LZ_RGB
body decode. Mixed MIT/GPL-3.0 license — independent reimplementation
felt cleaner.

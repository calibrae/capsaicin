# Capsaicin 🌶️

A pure-Rust [SPICE](https://www.spice-space.org/) remote-display protocol
implementation. Both client and server. No C bindings, no libspice, no
glib.

Connects to real QEMU SPICE servers, decodes a live KDE desktop, and
forwards keyboard + mouse back over the inputs channel.

## Status

Early but working. The viewer renders a real VM at 1280×800 with input
forwarding. See [JOURNEY.md](JOURNEY.md) for the build log and what's
done vs what isn't.

```
97 tests passing across the workspace
```

## Workspace

```
crates/
├── capsaicin-proto/   wire format: link, channels, message bodies
├── capsaicin-net/     tokio link handshake (client + server) + framing
├── capsaicin-quic/    standalone QUIC image decoder
├── capsaicin-lz/      standalone SPICE LZ decoder
├── capsaicin-glz/     standalone GLZ decoder + per-session image dictionary
├── capsaicin-server/  embeddable server core (link accept + main bootstrap)
├── capsaicin-client/  event-driven client (SpiceClient::connect → next_event loop)
└── capsaicin-cli/     `capsaicin` binary: connect / serve / view subcommands
```

The codec crates (`capsaicin-quic`, `capsaicin-lz`, `capsaicin-glz`)
are deliberately standalone — they only depend on `std` + `thiserror`
(plus `capsaicin-lz` for `LzImageType` in `capsaicin-glz`). Useful as
self-contained "decode SPICE-compressed images" libraries beyond the
context of capsaicin itself.

## Try it

```bash
cargo build --release

# Connect to a SPICE server, log what's happening (no rendering)
RUST_LOG=capsaicin=info ./target/release/capsaicin connect 127.0.0.1:5900

# Live viewer with window, keyboard, mouse
./target/release/capsaicin view 127.0.0.1:5900

# Serve as a SPICE endpoint (mostly for testing — no framebuffer source yet)
./target/release/capsaicin serve 127.0.0.1:5900 --password sesame
```

For a SPICE-enabled QEMU VM, the typical incantation is:
```
-spice port=5900,addr=127.0.0.1,disable-ticketing=on
```

If the VM lives on a remote host, tunnel first:
```
ssh -L 5900:127.0.0.1:5900 remote-host
```

## Use as a library

```rust
use capsaicin_client::{SpiceClient, ClientEvent, DisplayEvent, InputEvent, RegionPixels};

let mut client = SpiceClient::connect("127.0.0.1:5900", "").await?;
client.send_input(InputEvent::KeyDown(0x1e)).await?;  // press 'A'

while let Some(evt) = client.next_event().await {
    match evt {
        ClientEvent::Display(DisplayEvent::SurfaceCreated { width, height, .. }) => { /* allocate */ }
        ClientEvent::Display(DisplayEvent::Region { rect, pixels: RegionPixels::Raw { data, stride }, .. }) => {
            // blit BGRA pixels at rect
        }
        ClientEvent::Display(DisplayEvent::CopyRect { src_x, src_y, dest_rect, .. }) => {
            // copy rect within framebuffer
        }
        ClientEvent::Closed(_) => break,
        _ => {}
    }
}
```

## What works

- Plain TCP + RSA-OAEP ticket auth
- Main / Display / Inputs channels with proper `MSGC_ACK` flow control
- Pixel decoding: RAW BITMAP, LZ_RGB, GLZ_RGB (cross-image), QUIC
  (RGB32/24/Rgba/RGB16/Gray), MJPEG via streams
- `COPY_BITS` for compositor rect-copy operations
- `DRAW_FILL` solid color
- Keyboard + mouse with proper PC AT scancodes (incl. extended `0xE0`
  prefix and `0x80` break-bit on release)

## What doesn't (yet)

- TLS — most production deployments require it
- Cursor channel — visible flicker around the cursor
- Agent channel (clipboard, dynamic resolution)
- Audio (Playback / Record)
- USB redirect, smartcard, webdav, port channels
- Image types: JPEG, JPEG_ALPHA, ZLIB_GLZ_RGB, LZ4
- Stream codecs beyond MJPEG: VP8 / VP9 / H.264 / H.265
- Full draw command set (DRAW_OPAQUE, DRAW_BLEND, DRAW_ROP3, etc.)

[JOURNEY.md](JOURNEY.md) has the full picture plus a roadmap of what
to tackle next.

## Related

- [`spice-client`](https://crates.io/crates/spice-client) — a parallel
  pure-Rust SPICE client effort by `arsfeld` as part of
  [quickemu-manager](https://github.com/arsfeld/quickemu-manager). It
  has cursor + WASM but no QUIC/GLZ/LZ_RGB body decode at time of
  writing, and ships with mismatched MIT/GPL-3.0 metadata. Independent
  reimplementation felt cleaner.
- [SPICE protocol spec](https://www.spice-space.org/spice-protocol.html)
- [`spice-common`](https://gitlab.freedesktop.org/spice/spice-common) —
  reference C implementation we ported from
- [`spice-gtk`](https://gitlab.freedesktop.org/spice/spice-gtk) — the
  reference GTK client; we ported its GLZ decoder

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

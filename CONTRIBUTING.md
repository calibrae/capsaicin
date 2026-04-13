# Contributing to Capsaicin

Thanks for the interest. The project is small and early; contributions
are welcome.

## Getting set up

```bash
git clone https://github.com/calibrae/capsaicin
cd capsaicin
cargo build
cargo test
```

The whole workspace builds with stable Rust. No nightly, no special
toolchain.

To exercise the live viewer you need a SPICE-enabled QEMU VM. The
typical incantation:

```
qemu-system-x86_64 ... -spice port=5900,addr=127.0.0.1,disable-ticketing=on
```

Then `cargo run --release -- view 127.0.0.1:5900`.

## Where to start

[JOURNEY.md](JOURNEY.md) has a "what doesn't work yet" section. The
high-impact items in rough order:

1. **Cursor channel.** Separate channel (type 4) with its own message
   types (`MSG_CURSOR_INIT`, `_SET`, `_MOVE`, etc.). Today the cursor
   blink area is the most visible artifact.
2. **JPEG image type** (`image_type=105`). The `jpeg-decoder` crate is
   already a dependency for MJPEG streams; wiring it into the
   `decode_draw_copy` dispatch is mostly plumbing.
3. **TLS.** Most production VMs require it. `rustls` + the QEMU
   `tls-port=` line.
4. **More draw ops** (DRAW_OPAQUE, DRAW_BLEND, DRAW_TRANSPARENT, …) —
   common in compositors that don't fully composite client-side.
5. **`ZLIB_GLZ_RGB`** — same as GLZ but with an outer zlib wrap.
   `flate2` makes this small.
6. **QUIC encoder** for proper round-trip RLE coverage.

If you want to take one of these, opening an issue first is helpful —
sometimes I've already started or have notes. Otherwise just send a PR.

## Style

- **Match the existing terseness.** Comments only when the WHY is
  non-obvious. No multi-paragraph docstrings.
- **Keep tests next to code.** Unit tests inline in `#[cfg(test)] mod`,
  integration tests under `crates/<name>/tests/`.
- **Reach for `cargo fmt` before committing.** No clippy enforcement
  yet but `cargo clippy` should be reasonably clean.
- **Codec crates stay standalone.** `capsaicin-quic`, `capsaicin-lz`,
  `capsaicin-glz` are deliberately decoupled from the rest of the
  workspace — their only deps are `std + thiserror` (plus
  `capsaicin-lz` for `LzImageType` in `capsaicin-glz`). Don't add a
  dependency on `capsaicin-proto` to those crates.

## Reference

The reference C implementation we ported from:

- [`spice-common`](https://gitlab.freedesktop.org/spice/spice-common) —
  protocol headers, marshallers, image codecs (`lz.c`, `quic.c`)
- [`spice-gtk`](https://gitlab.freedesktop.org/spice/spice-gtk) —
  reference GTK client; the GLZ decoder lives here in
  `src/decode-glz*.c`
- [`spice-protocol`](https://gitlab.freedesktop.org/spice/spice-protocol)
  — message-type and enum constants

## Subtleties to know about

A few SPICE wire-format gotchas that tripped us up — worth knowing
before you go decode-hunting:

- **SPICE pointers are 4-byte u32 offsets** from the start of the
  message body — confirmed via
  `spice_marshaller_get_ptr_submarshaller` in `marshaller.c`.
- **`@chunk @nomarshal` proto annotations mean "data is inline,"
  not "data is at a chunks pointer."** The `SpiceChunks *data` field
  in C structs is a runtime view, not a wire field.
- **QUIC reads u32 words as little-endian; LZ reads them as
  big-endian.** Don't assume the same byte order for both.
- **`MSGC_PONG` is 12 bytes (id + timestamp).** Server `PING` carries
  arbitrary trailing data for bandwidth measurement; PONG never
  echoes it. Echoing it back will get you disconnected.
- **`KEY_UP` scancodes need the `0x80` break-bit set on the low
  byte.** Otherwise the guest sees the key as held.

JOURNEY.md captures the full story of how each of these surfaced.

## License

By contributing, you agree your contribution will be licensed under
both the MIT license and the Apache License 2.0 (the project's dual
license).

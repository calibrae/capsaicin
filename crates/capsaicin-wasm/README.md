# capsaicin-wasm

WebAssembly bindings for the SPICE image codecs implemented by
[capsaicin](https://github.com/calibrae/capsaicin) — `LZ_RGB`,
`GLZ_RGB`, and `QUIC`.

Intended use: drop-in replacement for the C decoders shipped with
[spice-html5](https://gitlab.freedesktop.org/spice/spice-html5) or as
the image layer of a new browser SPICE viewer built on a WebSocket
transport.

## Build

```sh
cargo install wasm-pack
wasm-pack build --target web --release
```

Outputs an npm-ready package in `pkg/` with `.wasm`, `.js` glue, and
TypeScript definitions.

Approximate size (release + wasm-opt -Oz): ~70 KiB.

## API

```js
import init, {
  decompressLzRgba,
  decompressQuicRgb32,
  GlzDecoder,
} from './pkg/capsaicin_wasm.js';

await init();

// Stateless LZ / QUIC.
const lz = decompressLzRgba(streamBytes, numPixels);      // -> Uint8Array BGRA
const qu = decompressQuicRgb32(streamBytes, w, h);         // -> Uint8Array BGRA

// GLZ keeps a per-session dictionary.
const glz = new GlzDecoder(32 * 1024 * 1024);  // 32 MiB window
const decoded = glz.decodeRgb32(streamBytes);
const pixels = decoded.takePixels();  // Uint8Array BGRA
glz.insert(decoded.id, pixels, 32);   // feed back so later frames can reference
```

## Pixel layout

All decoders return BGRA in surface memory order (little-endian
platforms): `data[0]=B, data[1]=G, data[2]=R, data[3]=A`. To hand to a
canvas `ImageData` you need RGBA:

```js
for (let i = 0; i < pixels.length; i += 4) {
  [pixels[i], pixels[i + 2]] = [pixels[i + 2], pixels[i]];
}
const img = new ImageData(new Uint8ClampedArray(pixels.buffer), w, h);
ctx.putImageData(img, 0, 0);
```

`decompressQuicRgb16` returns packed 16-bit RGB565 — convert to RGBA
per-pixel before canvas upload.

## Status

Codecs themselves are well-tested (see the parent workspace's test
suites). The wasm boundary has **not** been exercised against a live
SPICE-over-WebSocket stream yet. PRs welcome.

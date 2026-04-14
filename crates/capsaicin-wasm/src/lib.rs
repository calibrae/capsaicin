//! WebAssembly bindings for capsaicin's SPICE image codecs.
//!
//! Exposes the LZ, GLZ, and QUIC decoders as a small JS-friendly API
//! suitable for dropping into a SPICE-over-WebSocket browser viewer
//! (the niche spice-html5 currently fills with C code compiled via
//! emscripten).
//!
//! Build with `wasm-pack build --target web` from this crate's dir;
//! the resulting `pkg/` directory is npm-publishable.
//!
//! All decoders return `Uint8Array`s of BGRA pixels in the surface's
//! native memory order — top-down, `stride = width * 4` — ready to
//! `putImageData` after a byte swap to RGBA, or to hand to a
//! `createImageBitmap` via an `ImageData` wrapper.

use wasm_bindgen::prelude::*;

/// Call once from JS (e.g. at module init) to enable panic messages in
/// the browser console. No-op if the `panic_hook` feature is off.
#[wasm_bindgen(js_name = initPanicHook)]
pub fn init_panic_hook() {
    #[cfg(feature = "panic_hook")]
    console_error_panic_hook::set_once();
}

// ---------- LZ ----------

/// Decode a SPICE LZ-compressed `RGB32` (no alpha) image. `num_pixels`
/// is `width * height`. Returns a `Uint8Array` of `num_pixels * 4`
/// BGRA bytes (alpha byte ignored, left as 0).
#[wasm_bindgen(js_name = decompressLzRgb32)]
pub fn decompress_lz_rgb32(stream: &[u8], num_pixels: usize) -> Result<Vec<u8>, JsError> {
    capsaicin_lz::decompress_rgb32(stream, num_pixels).map_err(err)
}

/// Decode a SPICE LZ-compressed `RGBA` image. `num_pixels` is
/// `width * height`. Returns `num_pixels * 4` BGRA bytes (alpha is
/// meaningful).
#[wasm_bindgen(js_name = decompressLzRgba)]
pub fn decompress_lz_rgba(stream: &[u8], num_pixels: usize) -> Result<Vec<u8>, JsError> {
    capsaicin_lz::decompress_rgba(stream, num_pixels).map_err(err)
}

// ---------- QUIC ----------

/// Decode a QUIC-compressed image in `RGB32` format. `width` and
/// `height` are the image dimensions. Returns `width * height * 4`
/// BGRA bytes (alpha byte 0).
#[wasm_bindgen(js_name = decompressQuicRgb32)]
pub fn decompress_quic_rgb32(stream: &[u8], width: u32, height: u32) -> Result<Vec<u8>, JsError> {
    capsaicin_quic::decompress_rgb32(stream, width, height).map_err(err)
}

/// Decode a QUIC-compressed image in `RGBA` format.
#[wasm_bindgen(js_name = decompressQuicRgba)]
pub fn decompress_quic_rgba(stream: &[u8], width: u32, height: u32) -> Result<Vec<u8>, JsError> {
    capsaicin_quic::decompress_rgba(stream, width, height).map_err(err)
}

/// Decode a QUIC-compressed image in `RGB16` (5:6:5) format. Returns
/// `width * height * 2` bytes in little-endian 16-bit packed format.
#[wasm_bindgen(js_name = decompressQuicRgb16)]
pub fn decompress_quic_rgb16(stream: &[u8], width: u32, height: u32) -> Result<Vec<u8>, JsError> {
    capsaicin_quic::decompress_rgb16(stream, width, height).map_err(err)
}

/// Decode a QUIC-compressed grayscale image. Returns `width * height`
/// bytes, one per pixel.
#[wasm_bindgen(js_name = decompressQuicGray)]
pub fn decompress_quic_gray(stream: &[u8], width: u32, height: u32) -> Result<Vec<u8>, JsError> {
    capsaicin_quic::decompress_gray(stream, width, height).map_err(err)
}

// ---------- GLZ ----------

/// Per-session GLZ image dictionary. GLZ frames can reference pixels
/// from images sent earlier on the same display channel, so the
/// embedder must maintain one `GlzDecoder` per SPICE session and feed
/// every successful decode back with [`GlzDecoder::insert`].
#[wasm_bindgen]
pub struct GlzDecoder {
    window: capsaicin_glz::GlzWindow,
}

#[wasm_bindgen]
impl GlzDecoder {
    /// Create a new decoder with `capacity_bytes` of backing dictionary
    /// storage. SPICE's default is 64 MiB; mobile/browser embedders
    /// often tighten this to 16–32 MiB.
    #[wasm_bindgen(constructor)]
    pub fn new(capacity_bytes: usize) -> GlzDecoder {
        GlzDecoder {
            window: capsaicin_glz::GlzWindow::new(capacity_bytes),
        }
    }

    /// Bytes currently held in the dictionary.
    #[wasm_bindgen(js_name = bytesUsed)]
    pub fn bytes_used(&self) -> usize {
        self.window.bytes_used()
    }

    /// Drop all dictionary entries. Call on session restart.
    pub fn clear(&mut self) {
        self.window.clear();
    }

    /// Decode a GLZ `RGB32` image. Pass the *payload* of the
    /// `LZ_RGB`-typed message (after the common GLZ header). `bpp` is
    /// 32 for RGB32 images.
    ///
    /// Returns the decoded BGRA bytes. Caller must then pass the same
    /// bytes to [`GlzDecoder::insert`] keyed by the image id from the
    /// header, so later frames can reference this image.
    #[wasm_bindgen(js_name = decodeRgb32)]
    pub fn decode_rgb32(&mut self, stream: &[u8]) -> Result<DecodedGlz, JsError> {
        let header = capsaicin_glz::GlzHeader::decode(stream).map_err(err)?;
        let body = &stream[capsaicin_glz::GLZ_HEADER_SIZE..];
        let pixels =
            capsaicin_glz::decompress_rgb32(body, &header, &self.window).map_err(err)?;
        Ok(DecodedGlz {
            id: header.id,
            width: header.width,
            height: header.height,
            pixels,
        })
    }

    /// Insert a fully decoded image into the dictionary, keyed by its
    /// server-assigned `id`. `bpp` is 32 for RGB32/RGBA, 16 for RGB16.
    pub fn insert(&mut self, id: u64, pixels: Vec<u8>, bpp: u32) {
        self.window.insert(id, pixels, bpp);
    }
}

/// Result of a GLZ decode: the image id + dimensions (so JS can create
/// the right-size ImageData) and the decoded BGRA bytes.
#[wasm_bindgen]
pub struct DecodedGlz {
    id: u64,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

#[wasm_bindgen]
impl DecodedGlz {
    #[wasm_bindgen(getter)]
    pub fn id(&self) -> u64 {
        self.id
    }

    #[wasm_bindgen(getter)]
    pub fn width(&self) -> u32 {
        self.width
    }

    #[wasm_bindgen(getter)]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Move the pixel buffer out. Consumes `self`.
    #[wasm_bindgen(js_name = takePixels)]
    pub fn take_pixels(self) -> Vec<u8> {
        self.pixels
    }

    /// Borrow a copy of the pixel buffer without consuming. For when
    /// the caller wants both the pixels and the metadata.
    #[wasm_bindgen(js_name = pixelsCopy)]
    pub fn pixels_copy(&self) -> Vec<u8> {
        self.pixels.clone()
    }
}

// ---------- internals ----------

fn err<E: std::fmt::Display>(e: E) -> JsError {
    JsError::new(&e.to_string())
}

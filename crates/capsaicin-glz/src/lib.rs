//! SPICE GLZ — "Group LZ", a variant of [SPICE LZ](capsaicin_lz) that adds
//! cross-image references through a per-session dictionary window.
//!
//! Wire layout (all multi-byte fields BIG-endian, like LZ):
//!
//! ```text
//! magic:            u32 = LZ_MAGIC ("LZ  " = 0x20205a4c)
//! version:          u32 = LZ_VERSION
//! tmp:              u8  // bits 0-3: image type, bit 4: top_down
//! width:            u32
//! height:           u32
//! stride:           u32
//! id:               u64 // unique within the dictionary window
//! win_head_dist:    u32 // back-distance from the dictionary head
//! ...compressed body...
//! ```
//!
//! The compressed body uses LZ-style control bytes but the back-
//! reference encoding is different:
//!
//! - `ctrl >= 32`: back-reference. Top 3 bits = length (extended via
//!   trailing bytes if 7). Bit 4 = `pixel_flag` (long pixel offset).
//!   Bottom 4 bits = high nibble of `pixel_ofs`.
//! - `ctrl < 32`: literal run of `ctrl + 1` pixels.
//!
//! For the (very common) case of `image_dist == 0` (the back-reference
//! lives inside the current image), the decode loop is identical to
//! intra-image LZ. Cross-image references require the dictionary; this
//! module currently errors on those.

use std::collections::HashMap;

pub use capsaicin_lz::LzImageType;
use thiserror::Error;
use capsaicin_lz::LzError;

/// LZ_MAGIC: "LZ  " in big-endian = 0x20205a4c.
pub const GLZ_MAGIC: u32 = 0x2020_5a4c;
/// LZ_VERSION = 0x00010001 (major.minor).
pub const GLZ_VERSION: u32 = 0x0001_0001;

const MAX_COPY: u32 = 32;

/// Header of a GLZ-compressed image. All fields are big-endian on the
/// wire. `top_down` is packed into the high bit of the type byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlzHeader {
    pub image_type: LzImageType,
    pub top_down: bool,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub id: u64,
    pub win_head_dist: u32,
}

pub const GLZ_HEADER_SIZE: usize = 4 + 4 + 1 + 4 + 4 + 4 + 8 + 4; // 33

impl GlzHeader {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < GLZ_HEADER_SIZE {
            return Err(GlzError::Short {
                need: GLZ_HEADER_SIZE,
                have: buf.len(),
            });
        }
        // Custom big-endian primitives — `Reader` is LE.
        let magic = be32(&buf[0..4]);
        if magic != GLZ_MAGIC {
            return Err(GlzError::BadMagic {
                expected: GLZ_MAGIC,
                got: magic,
            });
        }
        let version = be32(&buf[4..8]);
        if version != GLZ_VERSION {
            return Err(GlzError::BadVersion {
                major: version >> 16,
                minor: version & 0xffff,
            });
        }
        let tmp = buf[8];
        let raw_type = (tmp & 0x0F) as u32;
        let top_down = (tmp >> 4) != 0;
        let image_type = LzImageType::from_u32(raw_type)?;
        let width = be32(&buf[9..13]);
        let height = be32(&buf[13..17]);
        let stride = be32(&buf[17..21]);
        let id = be64(&buf[21..29]);
        let win_head_dist = be32(&buf[29..33]);
        Ok(Self {
            image_type,
            top_down,
            width,
            height,
            stride,
            id,
            win_head_dist,
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        push_be32(out, GLZ_MAGIC);
        push_be32(out, GLZ_VERSION);
        let tmp = (self.image_type as u8) & 0x0F | (if self.top_down { 1 << 4 } else { 0 });
        out.push(tmp);
        push_be32(out, self.width);
        push_be32(out, self.height);
        push_be32(out, self.stride);
        push_be64(out, self.id);
        push_be32(out, self.win_head_dist);
    }
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b.try_into().unwrap())
}
fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes(b.try_into().unwrap())
}
fn push_be32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn push_be64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// What went wrong inside the GLZ decompressors.
#[derive(Debug, Error)]
pub enum GlzError {
    #[error("buffer too short: need {need} bytes, have {have}")]
    Short { need: usize, have: usize },

    #[error("invalid GLZ magic: expected {expected:#x}, got {got:#x}")]
    BadMagic { expected: u32, got: u32 },

    #[error("unsupported GLZ version {major}.{minor}")]
    BadVersion { major: u32, minor: u32 },

    /// Stream contained a back-reference into the per-session
    /// dictionary, but the [`GlzWindow`] passed in didn't have the
    /// referenced image. Either the window has been evicted past it or
    /// the caller used [`decompress_rgb32_intra`] (which has no
    /// dictionary).
    #[error("cross-image reference: id - {dist} not found in window")]
    CrossImage { dist: u32 },

    /// Output ran out before the reference would land.
    #[error("back-reference past end of image")]
    BadReference,

    /// Unsupported image type for this entry point.
    #[error("unsupported GLZ image type: {0:?}")]
    UnsupportedType(LzImageType),

    /// Underlying LZ enum failed to recognise the stream's image type.
    #[error("lz image type: {0}")]
    Lz(#[from] LzError),
}

pub type Result<T> = std::result::Result<T, GlzError>;

/// Per-session image dictionary. GLZ back-references can target any
/// image still in the window via `target_id = current_id - image_dist`.
/// Stored pixels are in the format the decoder produced (BGRA for
/// RGB32). Eviction is byte-budget driven rather than entry-count
/// driven so a long tail of small icon updates can stay resident
/// alongside the occasional full-frame paint.
#[derive(Debug)]
pub struct GlzWindow {
    images: HashMap<u64, WindowEntry>,
    /// Sum of `pixels.len()` across all entries.
    bytes_used: usize,
    /// Eviction trigger.
    capacity_bytes: usize,
    /// Monotonic counter used for LRU eviction.
    next_seq: u64,
}

#[derive(Debug)]
struct WindowEntry {
    pixels: Vec<u8>,
    /// Bytes per pixel in `pixels` (4 for BGRA, 3 for RGB24, ...).
    bpp: u32,
    seq: u64,
}

impl GlzWindow {
    /// New window with the given byte budget. spice-gtk defaults to
    /// 16 MiB. Smaller risks evicting still-referenced images on busy
    /// desktops.
    pub fn new(capacity_bytes: usize) -> Self {
        Self {
            images: HashMap::new(),
            bytes_used: 0,
            capacity_bytes: capacity_bytes.max(1024),
            next_seq: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.images.len()
    }

    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    pub fn bytes_used(&self) -> usize {
        self.bytes_used
    }

    pub fn clear(&mut self) {
        self.images.clear();
        self.bytes_used = 0;
    }

    /// Insert (or replace) an image. If `bytes_used` exceeds the
    /// budget, evicts the oldest-inserted entries until it fits.
    pub fn insert(&mut self, id: u64, pixels: Vec<u8>, bpp: u32) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let new_bytes = pixels.len();
        // Replace path — drop the old entry's bytes from the budget.
        if let Some(old) = self.images.remove(&id) {
            self.bytes_used = self.bytes_used.saturating_sub(old.pixels.len());
        }
        self.images.insert(id, WindowEntry { pixels, bpp, seq });
        self.bytes_used += new_bytes;
        while self.bytes_used > self.capacity_bytes && self.images.len() > 1 {
            if let Some((&victim_id, _)) = self.images.iter().min_by_key(|(_, e)| e.seq) {
                if let Some(victim) = self.images.remove(&victim_id) {
                    self.bytes_used = self.bytes_used.saturating_sub(victim.pixels.len());
                }
            } else {
                break;
            }
        }
    }

    fn get(&self, id: u64) -> Option<&WindowEntry> {
        self.images.get(&id)
    }
}

impl Default for GlzWindow {
    /// 256 MiB — generous enough that several large frames plus a long
    /// tail of thousands of small icon strips fits comfortably. SPICE
    /// guests can keep cross-image refs alive for many seconds; deep
    /// dictionaries pay off here.
    fn default() -> Self {
        Self::new(256 * 1024 * 1024)
    }
}

/// Decode an `LZ_IMAGE_TYPE_RGB32` GLZ stream that contains no cross-
/// image references. Output is `width * height * 4` bytes of BGRA
/// (alpha = 0). Returns `Err(GlzError::CrossImage{..})` on the first
/// reference into the dictionary; use [`decompress_rgb32`] with a
/// populated [`GlzWindow`] to handle those.
pub fn decompress_rgb32_intra(stream: &[u8], header: &GlzHeader) -> std::result::Result<Vec<u8>, GlzError> {
    decompress_rgb32_inner(stream, header, None, header.id)
}

/// Decode an `LZ_IMAGE_TYPE_RGB32` GLZ stream, resolving cross-image
/// back-references through `window`.
///
/// The caller is responsible for inserting the returned image into
/// `window` if subsequent images may reference it.
pub fn decompress_rgb32(
    stream: &[u8],
    header: &GlzHeader,
    window: &GlzWindow,
) -> std::result::Result<Vec<u8>, GlzError> {
    decompress_rgb32_inner(stream, header, Some(window), header.id)
}

fn decompress_rgb32_inner(
    stream: &[u8],
    header: &GlzHeader,
    window: Option<&GlzWindow>,
    self_id: u64,
) -> std::result::Result<Vec<u8>, GlzError> {
    if header.image_type != LzImageType::Rgb32 {
        return Err(GlzError::UnsupportedType(header.image_type));
    }
    let body = &stream[GLZ_HEADER_SIZE..];
    let n_pixels = (header.width as usize) * (header.height as usize);
    let mut out = vec![0u8; n_pixels * 4];
    let mut ip = 0usize;
    let mut op = 0usize;

    macro_rules! take {
        () => {{
            if ip >= body.len() {
                return Err(GlzError::Short { need: 1, have: 0 });
            }
            let b = body[ip];
            ip += 1;
            b
        }};
    }

    while op < n_pixels {
        let ctrl = take!() as u32;
        if ctrl >= MAX_COPY {
            // Back-reference.
            let mut len = ctrl >> 5;
            let pixel_flag = (ctrl >> 4) & 0x01;
            let mut pixel_ofs = ctrl & 0x0F;

            // Extended length.
            if len == 7 {
                loop {
                    let code = take!() as u32;
                    len += code;
                    if code != 0xFF {
                        break;
                    }
                }
            }

            let code = take!() as u32;
            pixel_ofs += code << 4;

            let code = take!() as u32;
            let image_flag = (code >> 6) & 0x03;
            let mut image_dist;
            if pixel_flag == 0 {
                image_dist = code & 0x3F;
                for i in 0..image_flag {
                    let extra = take!() as u32;
                    image_dist += extra << (6 + 8 * i);
                }
            } else {
                let pixel_flag2 = (code >> 5) & 0x01;
                pixel_ofs += (code & 0x1F) << 12;
                image_dist = 0;
                for i in 0..image_flag {
                    let extra = take!() as u32;
                    image_dist += extra << (8 * i);
                }
                if pixel_flag2 != 0 {
                    let extra = take!() as u32;
                    pixel_ofs += extra << 17;
                }
            }

            // RGB32 length bias is 0, no PLT cast needed.
            if image_dist == 0 {
                pixel_ofs += 1; // intra-image bias
            }

            let len_us = len as usize;
            if op + len_us > n_pixels {
                return Err(GlzError::BadReference);
            }

            if image_dist == 0 {
                // Intra-image reference: copy from earlier in the
                // same `out` buffer.
                let ofs_us = pixel_ofs as usize;
                if ofs_us == 0 || ofs_us > op {
                    return Err(GlzError::BadReference);
                }
                let mut src = op - ofs_us;
                for _ in 0..len_us {
                    let s = src * 4;
                    let d = op * 4;
                    out[d] = out[s];
                    out[d + 1] = out[s + 1];
                    out[d + 2] = out[s + 2];
                    out[d + 3] = 0;
                    src += 1;
                    op += 1;
                }
            } else {
                // Cross-image reference: look up `self_id - image_dist`
                // in the dictionary window and copy from that image
                // starting at `pixel_ofs` (no +1 bias for cross-image).
                let Some(window) = window else {
                    return Err(GlzError::CrossImage { dist: image_dist });
                };
                let target_id = self_id.wrapping_sub(image_dist as u64);
                let Some(entry) = window.get(target_id) else {
                    return Err(GlzError::CrossImage { dist: image_dist });
                };
                let bpp = entry.bpp as usize;
                let src_start = (pixel_ofs as usize) * bpp;
                let src_end = src_start + len_us * bpp;
                if src_end > entry.pixels.len() {
                    return Err(GlzError::BadReference);
                }
                // For cross-image refs we rewrite into BGRA. The
                // referenced image is also BGRA (we only insert RGB32
                // here for now).
                for k in 0..len_us {
                    let s = src_start + k * bpp;
                    let d = (op + k) * 4;
                    out[d] = entry.pixels[s];
                    out[d + 1] = entry.pixels[s + 1];
                    out[d + 2] = entry.pixels[s + 2];
                    out[d + 3] = 0;
                }
                op += len_us;
            }
        } else {
            // Literal run of (ctrl + 1) pixels — 3 bytes (B, G, R) each.
            let count = (ctrl + 1) as usize;
            if op + count > n_pixels {
                return Err(GlzError::BadReference);
            }
            for _ in 0..count {
                let d = op * 4;
                out[d] = take!(); // B
                out[d + 1] = take!(); // G
                out[d + 2] = take!(); // R
                out[d + 3] = 0; // A
                op += 1;
            }
        }
    }
    Ok(out)
}

/// Encode a sequence of BGRA pixels as a literal-only GLZ_RGB32 body
/// (no back-references, no cross-image refs). Used for tests.
#[doc(hidden)]
pub fn compress_rgb32_literal(pixels: &[u8]) -> Vec<u8> {
    debug_assert!(pixels.len() % 4 == 0);
    let n = pixels.len() / 4;
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let run = (n - i).min(MAX_COPY as usize);
        out.push((run - 1) as u8);
        for j in 0..run {
            let p = i + j;
            out.push(pixels[p * 4]); // B
            out.push(pixels[p * 4 + 1]); // G
            out.push(pixels[p * 4 + 2]); // R
        }
        i += run;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(width: u32, height: u32) -> GlzHeader {
        GlzHeader {
            image_type: LzImageType::Rgb32,
            top_down: true,
            width,
            height,
            stride: width * 4,
            id: 1,
            win_head_dist: 0,
        }
    }

    #[test]
    fn header_roundtrip() {
        let h = header(640, 480);
        let mut buf = Vec::new();
        h.encode(&mut buf);
        assert_eq!(buf.len(), GLZ_HEADER_SIZE);
        assert_eq!(GlzHeader::decode(&buf).unwrap(), h);
    }

    #[test]
    fn header_packs_top_down_into_type_byte() {
        let mut h = header(2, 2);
        h.top_down = false;
        let mut buf = Vec::new();
        h.encode(&mut buf);
        assert_eq!(buf[8] & (1 << 4), 0);
        h.top_down = true;
        let mut buf = Vec::new();
        h.encode(&mut buf);
        assert_eq!(buf[8] & (1 << 4), 1 << 4);
    }

    #[test]
    fn rgb32_literal_only_roundtrip() {
        let h = header(4, 2);
        let mut pixels = Vec::with_capacity(8 * 4);
        for i in 0..8u8 {
            pixels.extend_from_slice(&[i * 7, i * 11, i * 13, 0]);
        }
        let mut wire = Vec::new();
        h.encode(&mut wire);
        wire.extend_from_slice(&compress_rgb32_literal(&pixels));
        let decoded = decompress_rgb32_intra(&wire, &h).unwrap();
        assert_eq!(decoded, pixels);
    }

    #[test]
    fn cross_image_returns_specific_error() {
        // Hand-craft a back-reference with image_dist=1 (cross-image).
        let h = header(2, 1);
        let mut wire = Vec::new();
        h.encode(&mut wire);
        // First pixel: literal (count=1, ctrl=0). 3 bytes BGR.
        wire.push(0);
        wire.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        // Second pixel via back-ref. ctrl >= 32, len=1 (top 3 bits =
        // 0b001 → ctrl bit 5 set, but we want len=1 so encoded = 1).
        // After bias correction RGB32 has no len bias. We want 1 pixel
        // copied from intra-image dist 1 → image_dist=0, pixel_ofs=1.
        // ctrl = (1<<5) | (0<<4) | (pixel_ofs_high=0) → 0x20.
        // BUT we want to test cross-image; flip image_dist into the
        // mix.
        // Layout:  ctrl(0x20) | pixel_ofs_low(0) | image_flag/pixel_ofs(0x40 → image_flag=1, image_dist=0)
        // Then image_flag=1 means 1 extra byte for image_dist.
        wire.push(0x20); // ctrl: len=1, pixel_flag=0, pixel_ofs=0
        wire.push(0x00); // pixel_ofs += code << 4 → 0
        wire.push(0x40); // image_flag=1, low 6 bits of image_dist=0
        wire.push(0x01); // extra byte: image_dist += 1 << (6 + 0) = 64
        let err = decompress_rgb32_intra(&wire, &h).unwrap_err();
        assert!(matches!(err, GlzError::CrossImage { .. }), "got {err:?}");
    }

    #[test]
    fn glz_window_evicts_oldest_when_full() {
        // Budget = 2 KiB minimum; insert three 1 KiB images — third
        // should evict id=1.
        let mut win = GlzWindow::new(1024);
        win.insert(1, vec![1; 512], 4);
        win.insert(2, vec![2; 512], 4);
        win.insert(3, vec![3; 512], 4);
        assert!(win.get(1).is_none(), "id=1 should be evicted");
        assert!(win.get(2).is_some(), "id=2 should survive");
        assert!(win.get(3).is_some(), "id=3 should be present");
        assert_eq!(win.len(), 2);
    }

    /// Decode a 2-pixel image whose second pixel is a back-reference
    /// (image_dist = 1, pixel_ofs = 0, len = 1) into a previously-
    /// decoded image stored in the window.
    #[test]
    fn cross_image_reference_resolves_via_window() {
        let mut win = GlzWindow::new(8);
        // Pre-populate with id=10: a single BGRA pixel (B=0xAA, G=0xBB, R=0xCC, A=0).
        win.insert(10, vec![0xAA, 0xBB, 0xCC, 0], 4);

        // Header for current image at id=11 (so image_dist=1 → id=10).
        let h = GlzHeader {
            image_type: LzImageType::Rgb32,
            top_down: true,
            width: 1,
            height: 1,
            stride: 4,
            id: 11,
            win_head_dist: 1,
        };
        // Body: a single back-reference to image 10, pixel 0, length 1.
        // Encoding (RGB32, no len bias):
        //   ctrl: len=1, pixel_flag=0, pixel_ofs_low=0
        //         → (1 << 5) | 0 | 0 = 0x20
        //   pixel_ofs_high byte: 0
        //   image_flag/dist byte:
        //     pixel_flag was 0 → image_flag in top 2 bits, low 6 bits = image_dist
        //     For image_dist=1, fits in low 6 bits → image_flag=0, value=1.
        //         → 0x01
        let mut wire = Vec::new();
        h.encode(&mut wire);
        wire.push(0x20);
        wire.push(0x00);
        wire.push(0x01);
        let decoded = decompress_rgb32(&wire, &h, &win).unwrap();
        assert_eq!(decoded, vec![0xAA, 0xBB, 0xCC, 0]);
    }
}

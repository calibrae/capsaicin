//! SPICE LZ (a custom LZ77 variant) decompressor.
//!
//! Reference: `spice-common/common/lz.c`,
//! `spice-common/common/lz_decompress_tmpl.c`.
//!
//! Wire layout of an LZ-compressed image inside a `SpiceLZRGBData`:
//!
//! ```text
//! magic:    u32 (big-endian) = LZ_MAGIC ("LZ  " = 0x20205a4c)
//! version:  u32 (big-endian)
//! type:     u32 (big-endian) — LzImageType
//! width:    u32 (big-endian)
//! height:   u32 (big-endian)
//! stride:   u32 (big-endian)
//! top_down: u32 (big-endian) — boolean
//! ...compressed byte stream...
//! ```
//!
//! Note that everywhere ELSE in SPICE the wire format is little-endian.
//! The LZ payload is the exception: it uses big-endian internally.
//!
//! The compressed stream is a sequence of control bytes:
//!
//! - `ctrl < 32` → literal run of `ctrl + 1` pixels follow inline (each
//!   pixel is encoded per format: 3 bytes B,G,R for RGB32; 1 byte for
//!   alpha-only; etc.).
//! - `ctrl >= 32` → back-reference. Top 3 bits encode length, bottom 5
//!   bits the high byte of the distance. Extended encodings exist for
//!   long lengths (length=7 prefix) and far distances (low byte = 0xff
//!   with high bits all set).
//!
//! For the `RGBA` format the compressed payload is two LZ streams
//! concatenated: first an RGB32 stream covering B/G/R (with `pad = 0`),
//! then an alpha stream that overwrites only the `pad` byte of each
//! output pixel.

use thiserror::Error;

/// Errors raised by this decoder.
#[derive(Debug, Error)]
pub enum LzError {
    #[error("buffer too short: need {need} bytes, have {have}")]
    Short { need: usize, have: usize },

    #[error("invalid LZ magic: expected {expected:#x}, got {got:#x}")]
    BadMagic { expected: u32, got: u32 },

    #[error("unsupported LZ version {major}.{minor}")]
    BadVersion { major: u32, minor: u32 },

    #[error("invalid LZ image type {0}")]
    BadImageType(u32),

    #[error("image dimensions {width}×{height} exceed the configured cap")]
    TooLarge { width: u32, height: u32 },
}

pub type Result<T> = std::result::Result<T, LzError>;

/// Maximum image dimension we'll accept on either axis.
pub const MAX_IMAGE_DIM: u32 = 16384;
/// Maximum decoded image size in bytes.
pub const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;

/// Validate `width × height × bpp` is non-overflowing and within
/// `MAX_IMAGE_BYTES`. Returns the byte count.
pub fn validate_dims(width: u32, height: u32, bpp: u32) -> Result<usize> {
    if width == 0 || height == 0 || width > MAX_IMAGE_DIM || height > MAX_IMAGE_DIM {
        return Err(LzError::TooLarge { width, height });
    }
    let bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(bpp as usize))
        .ok_or(LzError::TooLarge { width, height })?;
    if bytes > MAX_IMAGE_BYTES {
        return Err(LzError::TooLarge { width, height });
    }
    Ok(bytes)
}

pub const LZ_MAGIC: u32 = 0x2020_5a4c; // "LZ  "
pub const LZ_VERSION: u32 = 0x0001_0001; // major=1 minor=1
const MAX_COPY: u32 = 32;
const MAX_DISTANCE: u32 = 8191;

/// Image type carried by the LZ stream header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum LzImageType {
    Plt1Le = 1,
    Plt1Be = 2,
    Plt4Le = 3,
    Plt4Be = 4,
    Plt8 = 5,
    Rgb16 = 6,
    Rgb24 = 7,
    Rgb32 = 8,
    Rgba = 9,
    Xxxa = 10,
    A8 = 11,
}

impl LzImageType {
    pub fn from_u32(v: u32) -> Result<Self> {
        Ok(match v {
            1 => Self::Plt1Le,
            2 => Self::Plt1Be,
            3 => Self::Plt4Le,
            4 => Self::Plt4Be,
            5 => Self::Plt8,
            6 => Self::Rgb16,
            7 => Self::Rgb24,
            8 => Self::Rgb32,
            9 => Self::Rgba,
            10 => Self::Xxxa,
            11 => Self::A8,
            _ => return Err(LzError::Short { need: 0, have: 0 }),
        })
    }
}

/// LZ stream header. All fields are big-endian on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LzHeader {
    pub image_type: LzImageType,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub top_down: bool,
}

pub const LZ_HEADER_SIZE: usize = 4 * 7;

impl LzHeader {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < LZ_HEADER_SIZE {
            return Err(LzError::Short {
                need: LZ_HEADER_SIZE,
                have: buf.len(),
            });
        }
        let magic = be_u32(&buf[0..4]);
        if magic != LZ_MAGIC {
            return Err(LzError::BadMagic {
                expected: LZ_MAGIC,
                got: magic,
            });
        }
        let version = be_u32(&buf[4..8]);
        if version != LZ_VERSION {
            return Err(LzError::BadVersion {
                major: version >> 16,
                minor: version & 0xffff,
            });
        }
        let image_type = LzImageType::from_u32(be_u32(&buf[8..12]))?;
        let width = be_u32(&buf[12..16]);
        let height = be_u32(&buf[16..20]);
        let stride = be_u32(&buf[20..24]);
        let top_down = be_u32(&buf[24..28]) != 0;
        Ok(Self {
            image_type,
            width,
            height,
            stride,
            top_down,
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&LZ_MAGIC.to_be_bytes());
        out.extend_from_slice(&LZ_VERSION.to_be_bytes());
        out.extend_from_slice(&(self.image_type as u32).to_be_bytes());
        out.extend_from_slice(&self.width.to_be_bytes());
        out.extend_from_slice(&self.height.to_be_bytes());
        out.extend_from_slice(&self.stride.to_be_bytes());
        out.extend_from_slice(&(self.top_down as u32).to_be_bytes());
    }
}

fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b.try_into().unwrap())
}

/// What a back-reference / literal carries through to each pixel slot.
#[derive(Clone, Copy)]
enum Stage {
    /// RGB32 / RGB24 / first half of RGBA: writes the B/G/R bytes of each
    /// 4-byte BGRA pixel and zeroes the A byte.
    Bgr,
    /// Alpha-only second pass over an existing BGRA buffer.
    Alpha,
}

/// Run an LZ decompression pass over `input`, writing `num_pixels` BGRA
/// pixels into `out` (always 4 bytes per pixel). For [`Stage::Bgr`] this
/// fills B/G/R and zeroes A; for [`Stage::Alpha`] it leaves B/G/R alone
/// and only writes the A byte. Returns the number of input bytes
/// consumed.
fn decompress_pass(
    input: &[u8],
    num_pixels: usize,
    out: &mut [u8],
    stage: Stage,
) -> Result<usize> {
    debug_assert!(out.len() >= num_pixels * 4);
    let mut ip = 0usize;
    let mut op = 0usize;

    macro_rules! read_byte {
        () => {{
            if ip >= input.len() {
                return Err(LzError::Short {
                    need: 1,
                    have: 0,
                });
            }
            let b = input[ip];
            ip += 1;
            b
        }};
    }

    while op < num_pixels {
        let ctrl = read_byte!() as u32;
        if ctrl >= MAX_COPY {
            // back-reference
            let mut len = ctrl >> 5;
            let mut ofs = (ctrl & 31) << 8;
            len = len.wrapping_sub(1);
            if len == 6 {
                loop {
                    let code = read_byte!() as u32;
                    len = len.wrapping_add(code);
                    if code != 255 {
                        break;
                    }
                }
            }
            let code = read_byte!() as u32;
            ofs = ofs.wrapping_add(code);
            if code == 255 && (ofs - code) == (31 << 8) {
                let hi = read_byte!() as u32;
                let lo = read_byte!() as u32;
                ofs = (hi << 8) | lo;
                ofs = ofs.wrapping_add(MAX_DISTANCE);
            }

            // Length bias depends on stage. RGB32 → +1, alpha pass → +3.
            len = match stage {
                Stage::Bgr => len.wrapping_add(1),
                Stage::Alpha => len.wrapping_add(3),
            };
            ofs = ofs.wrapping_add(1);

            let len = len as usize;
            let ofs = ofs as usize;
            if ofs == 0 || ofs > op || op + len > num_pixels {
                return Err(LzError::Short { need: len, have: 0 });
            }

            let mut src = op - ofs;
            for _ in 0..len {
                match stage {
                    Stage::Bgr => {
                        out[op * 4] = out[src * 4];
                        out[op * 4 + 1] = out[src * 4 + 1];
                        out[op * 4 + 2] = out[src * 4 + 2];
                        out[op * 4 + 3] = 0;
                    }
                    Stage::Alpha => {
                        out[op * 4 + 3] = out[src * 4 + 3];
                    }
                }
                src += 1;
                op += 1;
            }
        } else {
            // literal run of (ctrl + 1) pixels
            let count = (ctrl + 1) as usize;
            if op + count > num_pixels {
                return Err(LzError::Short {
                    need: count,
                    have: 0,
                });
            }
            for _ in 0..count {
                match stage {
                    Stage::Bgr => {
                        out[op * 4] = read_byte!(); // B
                        out[op * 4 + 1] = read_byte!(); // G
                        out[op * 4 + 2] = read_byte!(); // R
                        out[op * 4 + 3] = 0;
                    }
                    Stage::Alpha => {
                        out[op * 4 + 3] = read_byte!();
                    }
                }
                op += 1;
            }
        }
    }
    Ok(ip)
}

/// Decompress an LZ_RGB32 stream into a new BGRA buffer (`pad = 0`).
pub fn decompress_rgb32(stream: &[u8], num_pixels: usize) -> Result<Vec<u8>> {
    bounded_pixels(num_pixels)?;
    let mut out = vec![0u8; num_pixels * 4];
    let _ = decompress_pass(stream, num_pixels, &mut out, Stage::Bgr)?;
    Ok(out)
}

/// Decompress an LZ_RGBA stream: BGR pass first, then alpha overlay.
pub fn decompress_rgba(stream: &[u8], num_pixels: usize) -> Result<Vec<u8>> {
    bounded_pixels(num_pixels)?;
    let mut out = vec![0u8; num_pixels * 4];
    let consumed = decompress_pass(stream, num_pixels, &mut out, Stage::Bgr)?;
    let _ = decompress_pass(&stream[consumed..], num_pixels, &mut out, Stage::Alpha)?;
    Ok(out)
}

/// Validate `num_pixels * 4` is within `MAX_IMAGE_BYTES` and doesn't
/// overflow `usize` on the way there.
fn bounded_pixels(num_pixels: usize) -> Result<()> {
    if num_pixels == 0 {
        return Err(LzError::TooLarge { width: 0, height: 0 });
    }
    let bytes = num_pixels.checked_mul(4).ok_or(LzError::TooLarge {
        width: 0,
        height: 0,
    })?;
    if bytes > MAX_IMAGE_BYTES {
        return Err(LzError::TooLarge {
            width: num_pixels.min(u32::MAX as usize) as u32,
            height: 0,
        });
    }
    Ok(())
}

// --------- minimal compressor used only by tests / fuzzers ----------

/// Encode a sequence of BGRA pixels as LZ_RGB32 using literal runs only.
/// Round-trip-only — produces valid-but-uncompressed output.
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

/// Encode the alpha channel of a BGRA pixel buffer as a literal-run LZ
/// alpha stream.
#[doc(hidden)]
pub fn compress_alpha_literal(pixels: &[u8]) -> Vec<u8> {
    debug_assert!(pixels.len() % 4 == 0);
    let n = pixels.len() / 4;
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let run = (n - i).min(MAX_COPY as usize);
        out.push((run - 1) as u8);
        for j in 0..run {
            out.push(pixels[(i + j) * 4 + 3]);
        }
        i += run;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pixels(n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(n * 4);
        for i in 0..n {
            v.push((i * 7 + 1) as u8); // B
            v.push((i * 11 + 2) as u8); // G
            v.push((i * 13 + 3) as u8); // R
            v.push((i * 17 + 4) as u8); // A
        }
        v
    }

    #[test]
    fn header_roundtrip() {
        let h = LzHeader {
            image_type: LzImageType::Rgb32,
            width: 1280,
            height: 800,
            stride: 5120,
            top_down: true,
        };
        let mut buf = Vec::new();
        h.encode(&mut buf);
        assert_eq!(buf.len(), LZ_HEADER_SIZE);
        assert_eq!(LzHeader::decode(&buf).unwrap(), h);
    }

    #[test]
    fn header_rejects_bad_magic() {
        let mut buf = vec![0u8; LZ_HEADER_SIZE];
        buf[0..4].copy_from_slice(&0xdead_beefu32.to_be_bytes());
        assert!(matches!(
            LzHeader::decode(&buf),
            Err(LzError::BadMagic { .. })
        ));
    }

    #[test]
    fn rgb32_literal_roundtrip_short() {
        let pixels = sample_pixels(1);
        let stream = compress_rgb32_literal(&pixels);
        // ctrl=0 + 3 BGR bytes = 4 bytes total
        assert_eq!(stream.len(), 4);
        let decoded = decompress_rgb32(&stream, 1).unwrap();
        // RGB32: B,G,R from input, A=0
        assert_eq!(decoded, vec![pixels[0], pixels[1], pixels[2], 0]);
    }

    #[test]
    fn rgb32_literal_roundtrip_multi_run() {
        // 100 pixels — exceeds MAX_COPY (32) → multiple runs
        let pixels = sample_pixels(100);
        let stream = compress_rgb32_literal(&pixels);
        let decoded = decompress_rgb32(&stream, 100).unwrap();
        for i in 0..100 {
            assert_eq!(decoded[i * 4], pixels[i * 4], "B mismatch at {i}");
            assert_eq!(decoded[i * 4 + 1], pixels[i * 4 + 1], "G mismatch at {i}");
            assert_eq!(decoded[i * 4 + 2], pixels[i * 4 + 2], "R mismatch at {i}");
            assert_eq!(decoded[i * 4 + 3], 0, "alpha must be 0 for RGB32");
        }
    }

    #[test]
    fn rgba_literal_roundtrip() {
        let pixels = sample_pixels(50);
        let mut stream = compress_rgb32_literal(&pixels);
        stream.extend_from_slice(&compress_alpha_literal(&pixels));
        let decoded = decompress_rgba(&stream, 50).unwrap();
        assert_eq!(decoded.len(), pixels.len());
        for i in 0..50 {
            assert_eq!(&decoded[i * 4..(i + 1) * 4], &pixels[i * 4..(i + 1) * 4]);
        }
    }

    /// Hand-craft a back-reference: 1 literal pixel, then a run that
    /// repeats it 4 times via reference (offset=1, length=4).
    #[test]
    fn rgb32_back_reference_run() {
        // Back-ref encoding: ctrl = (len_code << 5) | (ofs_high & 31).
        // Decoder steps: len = ctrl>>5; len -= 1; (extended check); read ofs low;
        // RGB32 bias: len += 1, ofs += 1.
        // We want len=4, ofs=1 → len_code=4, encoded ofs=0 → ctrl=0x80, low=0.
        let mut stream = vec![
            0, // ctrl=0 → 1 literal pixel
            0xAA, 0xBB, 0xCC, // B, G, R
        ];
        stream.push(0x80);
        stream.push(0x00);
        let decoded = decompress_rgb32(&stream, 5).unwrap();
        assert_eq!(decoded.len(), 20);
        for px in decoded.chunks_exact(4) {
            assert_eq!(px, [0xAA, 0xBB, 0xCC, 0]);
        }
    }
}

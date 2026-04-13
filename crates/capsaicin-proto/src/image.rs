//! `SpiceImage` hierarchy: the payload structure pointed to by every
//! drawing command that needs pixels (e.g. `DRAW_COPY`'s `src_bitmap`).
//!
//! On the wire:
//!
//! ```text
//! SpiceImageDescriptor { id:u64, type:u8, flags:u8, width:u32, height:u32 }
//! (followed by a type-specific payload, e.g. SpiceBitmap for type==BITMAP)
//! ```
//!
//! For `BITMAP`:
//!
//! ```text
//! SpiceBitmap { format:u8, flags:u8, x:u32, y:u32, stride:u32,
//!               palette_ptr:u32, data_ptr:u32 }
//! ```
//!
//! `data_ptr` points into the same message body at a `SpiceChunks`:
//!
//! ```text
//! SpiceChunks { data_size:u32, num_chunks:u32, chunks[num_chunks] }
//! SpiceChunk  { len:u32, bytes:u8[len] }
//! ```
//!
//! This module only parses the `BITMAP` branch today; compressed branches
//! (`QUIC`, `LZ_RGB`, `LZ4`, `JPEG`, ...) are recognised but not decoded.

use crate::types::{Reader, Writer};
use crate::{ProtoError, Result};

pub mod image_type {
    pub const BITMAP: u8 = 0;
    pub const QUIC: u8 = 1;
    pub const RESERVED: u8 = 2;
    pub const LZ_PLT: u8 = 100;
    pub const LZ_RGB: u8 = 101;
    pub const GLZ_RGB: u8 = 102;
    pub const FROM_CACHE: u8 = 103;
    pub const SURFACE: u8 = 104;
    pub const JPEG: u8 = 105;
    pub const FROM_CACHE_LOSSLESS: u8 = 106;
    pub const ZLIB_GLZ_RGB: u8 = 107;
    pub const JPEG_ALPHA: u8 = 108;
    pub const LZ4: u8 = 109;
}

/// `SpiceBitmap.format`
#[allow(non_upper_case_globals)]
pub mod bitmap_fmt {
    pub const INVALID: u8 = 0;
    pub const _1BIT_LE: u8 = 1;
    pub const _1BIT_BE: u8 = 2;
    pub const _4BIT_LE: u8 = 3;
    pub const _4BIT_BE: u8 = 4;
    pub const _8BIT: u8 = 5;
    pub const _16BIT: u8 = 6;
    pub const _24BIT: u8 = 7;
    pub const _32BIT: u8 = 8;
    pub const RGBA: u8 = 9;
    pub const _8BIT_A: u8 = 10;
}

pub mod bitmap_flags {
    /// Pixel rows are stored top-to-bottom. If unset, bottom-up.
    pub const TOP_DOWN: u8 = 1 << 0;
}

/// Common header attached to every `SpiceImage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageDescriptor {
    pub id: u64,
    pub image_type: u8,
    pub flags: u8,
    pub width: u32,
    pub height: u32,
}

impl ImageDescriptor {
    pub const SIZE: usize = 18;

    pub fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self {
            id: r.u64()?,
            image_type: r.u8()?,
            flags: r.u8()?,
            width: r.u32()?,
            height: r.u32()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u64(self.id);
        w.u8(self.image_type);
        w.u8(self.flags);
        w.u32(self.width);
        w.u32(self.height);
    }
}

/// `SpiceBitmap` — follows an `ImageDescriptor` whose `image_type` is
/// [`image_type::BITMAP`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bitmap {
    pub format: u8,
    pub flags: u8,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// Offset to `SpicePalette` in the message body, or 0.
    pub palette_offset: u32,
    /// Offset to `SpiceChunks` in the message body.
    pub data_offset: u32,
}

impl Bitmap {
    pub const SIZE: usize = 22;

    pub fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self {
            format: r.u8()?,
            flags: r.u8()?,
            width: r.u32()?,
            height: r.u32()?,
            stride: r.u32()?,
            palette_offset: r.u32()?,
            data_offset: r.u32()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u8(self.format);
        w.u8(self.flags);
        w.u32(self.width);
        w.u32(self.height);
        w.u32(self.stride);
        w.u32(self.palette_offset);
        w.u32(self.data_offset);
    }

    pub fn is_top_down(&self) -> bool {
        self.flags & bitmap_flags::TOP_DOWN != 0
    }
}

/// Collect the bytes of a `SpiceChunks` object at `offset` inside
/// `msg_body` into a single flat `Vec<u8>` (concatenating chunks in
/// order).
///
/// Wire format:
///
/// ```text
/// data_size: u32  // total across all chunks
/// num_chunks: u32
/// per chunk:
///   len: u32
///   bytes: u8[len]
/// ```
pub fn read_chunks(msg_body: &[u8], offset: u32) -> Result<Vec<u8>> {
    let start = offset as usize;
    if start + 8 > msg_body.len() {
        return Err(ProtoError::Short {
            need: 8,
            have: msg_body.len().saturating_sub(start),
        });
    }
    let mut r = Reader::new(&msg_body[start..]);
    let data_size = r.u32()? as usize;
    let num_chunks = r.u32()? as usize;
    let mut out = Vec::with_capacity(data_size);
    for _ in 0..num_chunks {
        let len = r.u32()? as usize;
        let bytes = r.bytes(len)?;
        out.extend_from_slice(bytes);
    }
    if out.len() != data_size {
        return Err(ProtoError::Short {
            need: data_size,
            have: out.len(),
        });
    }
    Ok(out)
}

/// Encode a single-chunk `SpiceChunks` into a new buffer. Useful for
/// tests.
pub fn encode_single_chunk(data: &[u8]) -> Vec<u8> {
    let mut w = Writer::new();
    w.u32(data.len() as u32);
    w.u32(1);
    w.u32(data.len() as u32);
    w.bytes(data);
    w.into_vec()
}

/// Bytes per pixel for formats whose storage we understand.
pub fn bitmap_bytes_per_pixel(format: u8) -> Option<u32> {
    Some(match format {
        bitmap_fmt::_16BIT => 2,
        bitmap_fmt::_24BIT => 3,
        bitmap_fmt::_32BIT | bitmap_fmt::RGBA => 4,
        bitmap_fmt::_8BIT | bitmap_fmt::_8BIT_A => 1,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_descriptor_roundtrip() {
        let d = ImageDescriptor {
            id: 0x1122_3344_5566_7788,
            image_type: image_type::BITMAP,
            flags: 0,
            width: 640,
            height: 480,
        };
        let mut w = Writer::new();
        d.encode(&mut w);
        assert_eq!(w.as_slice().len(), ImageDescriptor::SIZE);
        let mut r = Reader::new(w.as_slice());
        assert_eq!(ImageDescriptor::decode(&mut r).unwrap(), d);
    }

    #[test]
    fn bitmap_roundtrip() {
        let b = Bitmap {
            format: bitmap_fmt::_32BIT,
            flags: bitmap_flags::TOP_DOWN,
            width: 32,
            height: 16,
            stride: 128,
            palette_offset: 0,
            data_offset: 64,
        };
        let mut w = Writer::new();
        b.encode(&mut w);
        assert_eq!(w.as_slice().len(), Bitmap::SIZE);
        let mut r = Reader::new(w.as_slice());
        assert_eq!(Bitmap::decode(&mut r).unwrap(), b);
    }

    #[test]
    fn chunks_single_roundtrip() {
        let payload: Vec<u8> = (0..200).map(|i| i as u8).collect();
        let encoded = encode_single_chunk(&payload);
        // data_size + num_chunks + chunk_len + payload = 12 + 200.
        assert_eq!(encoded.len(), 12 + payload.len());
        let decoded = read_chunks(&encoded, 0).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn chunks_respects_offset() {
        let payload = b"hello world";
        let mut wire = vec![0xAA, 0xBB, 0xCC];
        wire.extend_from_slice(&encode_single_chunk(payload));
        let decoded = read_chunks(&wire, 3).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn chunks_multi() {
        // Hand-craft a two-chunk buffer.
        let a: &[u8] = &[1, 2, 3, 4];
        let b: &[u8] = &[5, 6, 7];
        let mut w = Writer::new();
        w.u32((a.len() + b.len()) as u32); // data_size
        w.u32(2); // num_chunks
        w.u32(a.len() as u32);
        w.bytes(a);
        w.u32(b.len() as u32);
        w.bytes(b);
        let decoded = read_chunks(w.as_slice(), 0).unwrap();
        assert_eq!(decoded, [1, 2, 3, 4, 5, 6, 7]);
    }
}

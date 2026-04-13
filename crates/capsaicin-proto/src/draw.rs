//! Display-channel drawing commands.
//!
//! SPICE's drawing commands use a marshal-tree wire format: a flat struct
//! up front, with `uint32` offsets inside it pointing to variable-length
//! sub-structures that follow. Offsets are measured from the start of
//! the message body. An offset of `0` means "null / absent".
//!
//! This module covers the subset needed to render simple solid fills
//! (`DRAW_FILL`). The full set (`DRAW_COPY` + `SpiceImage` + `SpiceBitmap`
//! + `SpiceChunks`, plus compressed variants) is layered on top.

use crate::types::{Point, Reader, Rect, Writer};
use crate::Result;

/// Rop descriptor bits (`SPICE_ROPD_*`). The common case we care about is
/// `OP_PUT` — "overwrite destination with source", no blending.
pub mod ropd {
    pub const INVERS_SRC: u16 = 1 << 0;
    pub const INVERS_BRUSH: u16 = 1 << 1;
    pub const INVERS_DEST: u16 = 1 << 2;
    pub const OP_PUT: u16 = 1 << 3;
    pub const OP_OR: u16 = 1 << 4;
    pub const OP_AND: u16 = 1 << 5;
    pub const OP_XOR: u16 = 1 << 6;
    pub const OP_BLACKNESS: u16 = 1 << 7;
    pub const OP_WHITENESS: u16 = 1 << 8;
    pub const OP_INVERS: u16 = 1 << 9;
    pub const INVERS_RES: u16 = 1 << 10;
}

/// `SpiceClip` type tag.
pub mod clip_type {
    pub const NONE: u8 = 0;
    pub const RECTS: u8 = 1;
}

/// `SpiceBrush` type tag.
pub mod brush_type {
    pub const NONE: u32 = 0;
    pub const SOLID: u32 = 1;
    pub const PATTERN: u32 = 2;
}

/// Clip region accompanying every drawing command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clip {
    None,
    /// Clip rectangles are pointed to by this offset (not yet parsed).
    Rects { offset: u32 },
}

impl Clip {
    pub fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let t = r.u8()?;
        Ok(match t {
            clip_type::NONE => Self::None,
            clip_type::RECTS => {
                let offset = r.u32()?;
                Self::Rects { offset }
            }
            // Unknown clip types: treat the tag as all that was there.
            // Parsers of the outer message will then fail on size.
            _ => Self::Rects { offset: 0 },
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        match self {
            Self::None => w.u8(clip_type::NONE),
            Self::Rects { offset } => {
                w.u8(clip_type::RECTS);
                w.u32(*offset);
            }
        }
    }
}

/// Paint brush: solid color, pattern, or none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Brush {
    None,
    /// 32-bit color (little-endian). For a 32-bit `xRGB`/`ARGB` surface
    /// the bytes in memory are `B, G, R, A`.
    Solid(u32),
    /// Pattern image + origin. Image is pointed to by `offset` within the
    /// message body.
    Pattern { offset: u32, pos: Point },
}

impl Brush {
    pub fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let t = r.u32()?;
        Ok(match t {
            brush_type::NONE => Self::None,
            brush_type::SOLID => Self::Solid(r.u32()?),
            brush_type::PATTERN => {
                let offset = r.u32()?;
                let pos = Point {
                    x: r.i32()?,
                    y: r.i32()?,
                };
                Self::Pattern { offset, pos }
            }
            _ => Self::None,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        match self {
            Self::None => w.u32(brush_type::NONE),
            Self::Solid(c) => {
                w.u32(brush_type::SOLID);
                w.u32(*c);
            }
            Self::Pattern { offset, pos } => {
                w.u32(brush_type::PATTERN);
                w.u32(*offset);
                w.i32(pos.x);
                w.i32(pos.y);
            }
        }
    }
}

/// Mask attached to drawing commands. `bitmap_offset == 0` means no mask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QMask {
    pub flags: u8,
    pub pos: Point,
    pub bitmap_offset: u32,
}

impl QMask {
    pub fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self {
            flags: r.u8()?,
            pos: Point {
                x: r.i32()?,
                y: r.i32()?,
            },
            bitmap_offset: r.u32()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u8(self.flags);
        w.i32(self.pos.x);
        w.i32(self.pos.y);
        w.u32(self.bitmap_offset);
    }

    pub fn has_bitmap(&self) -> bool {
        self.bitmap_offset != 0
    }
}

/// Common header shared by every drawing command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawBase {
    pub surface_id: u32,
    pub bounds: Rect,
    pub clip: Clip,
}

impl DrawBase {
    pub fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self {
            surface_id: r.u32()?,
            bounds: Rect::decode(r)?,
            clip: Clip::decode(r)?,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.surface_id);
        self.bounds.encode(w);
        self.clip.encode(w);
    }
}

/// Parsed `MSG_DISPLAY_DRAW_FILL` body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawFill {
    pub base: DrawBase,
    pub brush: Brush,
    pub rop_descriptor: u16,
    pub mask: QMask,
}

impl DrawFill {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        let base = DrawBase::decode(&mut r)?;
        let brush = Brush::decode(&mut r)?;
        let rop_descriptor = r.u16()?;
        let mask = QMask::decode(&mut r)?;
        Ok(Self {
            base,
            brush,
            rop_descriptor,
            mask,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        self.base.encode(w);
        self.brush.encode(w);
        w.u16(self.rop_descriptor);
        self.mask.encode(w);
    }

    /// True when this fill can be rendered as a single solid rectangle
    /// without reading the destination surface (no complex rop, no mask,
    /// no clip rects).
    pub fn is_simple_solid(&self) -> bool {
        matches!(self.clip(), Clip::None)
            && matches!(self.brush, Brush::Solid(_))
            && self.rop_descriptor == ropd::OP_PUT
            && !self.mask.has_bitmap()
    }

    pub fn clip(&self) -> Clip {
        self.base.clip
    }

    pub fn solid_color(&self) -> Option<u32> {
        if let Brush::Solid(c) = self.brush {
            Some(c)
        } else {
            None
        }
    }
}

/// `MSG_DISPLAY_COPY_BITS` (104) — copy a rectangle from `src_pos` to
/// `base.bounds` within the same surface. Used heavily by compositors
/// for window movement, scrolling, and shadow updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyBits {
    pub base: DrawBase,
    pub src_pos: Point,
}

impl CopyBits {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        let base = DrawBase::decode(&mut r)?;
        let x = r.i32()?;
        let y = r.i32()?;
        Ok(Self {
            base,
            src_pos: Point { x, y },
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        self.base.encode(w);
        w.i32(self.src_pos.x);
        w.i32(self.src_pos.y);
    }
}

/// `SpiceImageScaleMode`
pub mod scale_mode {
    pub const INTERPOLATE: u8 = 0;
    pub const NEAREST: u8 = 1;
}

/// Fixed part of `MSG_DISPLAY_DRAW_COPY`. `src_bitmap_offset` points
/// into the same message body at a `SpiceImage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawCopy {
    pub base: DrawBase,
    pub src_bitmap_offset: u32,
    pub src_area: Rect,
    pub rop_descriptor: u16,
    pub scale_mode: u8,
    pub mask: QMask,
}

impl DrawCopy {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        let base = DrawBase::decode(&mut r)?;
        let src_bitmap_offset = r.u32()?;
        let src_area = Rect::decode(&mut r)?;
        let rop_descriptor = r.u16()?;
        let scale_mode = r.u8()?;
        let mask = QMask::decode(&mut r)?;
        Ok(Self {
            base,
            src_bitmap_offset,
            src_area,
            rop_descriptor,
            scale_mode,
            mask,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        self.base.encode(w);
        w.u32(self.src_bitmap_offset);
        self.src_area.encode(w);
        w.u16(self.rop_descriptor);
        w.u8(self.scale_mode);
        self.mask.encode(w);
    }

    /// True when the copy can be rendered as a straight `memcpy` from
    /// `src_bitmap` into `base.bounds` with no blending, mask, or clip.
    pub fn is_simple_copy(&self) -> bool {
        matches!(self.base.clip, Clip::None)
            && self.rop_descriptor == ropd::OP_PUT
            && !self.mask.has_bitmap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_solid_fill() -> DrawFill {
        DrawFill {
            base: DrawBase {
                surface_id: 0,
                bounds: Rect {
                    top: 10,
                    left: 20,
                    bottom: 30,
                    right: 40,
                },
                clip: Clip::None,
            },
            brush: Brush::Solid(0x00_FF_80_00),
            rop_descriptor: ropd::OP_PUT,
            mask: QMask {
                flags: 0,
                pos: Point { x: 0, y: 0 },
                bitmap_offset: 0,
            },
        }
    }

    #[test]
    fn draw_fill_roundtrip_simple() {
        let d = simple_solid_fill();
        let mut w = Writer::new();
        d.encode(&mut w);
        let encoded = w.as_slice();
        // surface_id(4) + box(16) + clip.type(1) + brush.type(4) +
        // brush.color(4) + ropd(2) + mask.flags(1) + mask.pos(8) +
        // mask.bitmap_offset(4) = 44 bytes.
        assert_eq!(encoded.len(), 44);
        assert_eq!(DrawFill::decode(encoded).unwrap(), d);
    }

    #[test]
    fn simple_solid_is_recognised() {
        assert!(simple_solid_fill().is_simple_solid());
    }

    #[test]
    fn pattern_brush_is_not_simple_solid() {
        let mut d = simple_solid_fill();
        d.brush = Brush::Pattern {
            offset: 48,
            pos: Point { x: 0, y: 0 },
        };
        assert!(!d.is_simple_solid());
    }

    #[test]
    fn ropd_other_than_op_put_is_not_simple_solid() {
        let mut d = simple_solid_fill();
        d.rop_descriptor = ropd::OP_XOR;
        assert!(!d.is_simple_solid());
    }

    #[test]
    fn mask_disqualifies_simple_solid() {
        let mut d = simple_solid_fill();
        d.mask.bitmap_offset = 48;
        assert!(!d.is_simple_solid());
    }

    fn simple_copy() -> DrawCopy {
        DrawCopy {
            base: DrawBase {
                surface_id: 0,
                bounds: Rect {
                    top: 0,
                    left: 0,
                    bottom: 16,
                    right: 32,
                },
                clip: Clip::None,
            },
            src_bitmap_offset: 57,
            src_area: Rect {
                top: 0,
                left: 0,
                bottom: 16,
                right: 32,
            },
            rop_descriptor: ropd::OP_PUT,
            scale_mode: scale_mode::NEAREST,
            mask: QMask {
                flags: 0,
                pos: Point { x: 0, y: 0 },
                bitmap_offset: 0,
            },
        }
    }

    #[test]
    fn draw_copy_roundtrip() {
        let d = simple_copy();
        let mut w = Writer::new();
        d.encode(&mut w);
        // base (21 NONE) + ptr (4) + src_area (16) + ropd (2) + scale (1) + mask (13) = 57
        assert_eq!(w.as_slice().len(), 57);
        assert_eq!(DrawCopy::decode(w.as_slice()).unwrap(), d);
        assert!(d.is_simple_copy());
    }

    #[test]
    fn draw_copy_with_mask_is_not_simple() {
        let mut d = simple_copy();
        d.mask.bitmap_offset = 64;
        assert!(!d.is_simple_copy());
    }

    #[test]
    fn rects_clip_disqualifies_simple_solid() {
        let mut d = simple_solid_fill();
        d.base.clip = Clip::Rects { offset: 48 };
        assert!(!d.is_simple_solid());
    }
}

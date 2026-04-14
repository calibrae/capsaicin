//! Cursor-channel message bodies.
//!
//! In SPICE's `SERVER` mouse mode the guest does not paint the cursor
//! into the framebuffer; the cursor sprite and position arrive on a
//! dedicated sub-channel so the viewer can composite them on top. In
//! `CLIENT` mouse mode the local OS draws the cursor and these
//! messages are mostly advisory, but servers still send them.
//!
//! We implement the alpha-cursor (`type = 0`, 32-bit ARGB) path. Other
//! legacy types (MONO, palette-indexed) are parsed as headers so the
//! caller can report "unsupported type" rather than crash, but their
//! pixel data is returned unconverted.
//!
//! Wire layout, for reference:
//!
//! ```text
//! SpicePoint16     { i16 x, i16 y }
//! SpiceCursorHeader {
//!     u64 unique;   // cache key
//!     u16 type;     // 0=ALPHA, 1=MONO, 2=4, 3=8, 4=16, 5=24, 6=32
//!     u16 width, height;
//!     u16 hot_spot_x, hot_spot_y;
//! }
//! SpiceCursor {
//!     u16 flags;
//!     SpiceCursorHeader header;
//!     u32 data_size;
//!     u8  data[data_size];
//! }
//! ```

use crate::types::{Reader, Writer};
use crate::Result;

/// Cursor-channel server messages.
pub mod cursor_msg {
    pub const INIT: u16 = 101;
    pub const RESET: u16 = 102;
    pub const SET: u16 = 103;
    pub const MOVE: u16 = 104;
    pub const HIDE: u16 = 105;
    pub const TRAIL: u16 = 106;
    pub const INVAL_ONE: u16 = 107;
    pub const INVAL_ALL: u16 = 108;
}

/// Cursor pixel-encoding types carried by `CursorHeader::type`.
pub mod cursor_type {
    pub const ALPHA: u16 = 0;
    pub const MONO: u16 = 1;
    pub const COLOR_4: u16 = 2;
    pub const COLOR_8: u16 = 3;
    pub const COLOR_16: u16 = 4;
    pub const COLOR_24: u16 = 5;
    pub const COLOR_32: u16 = 6;
}

/// Cursor flag bits (lower bits of `SpiceCursor.flags`).
pub mod cursor_flag {
    pub const NONE: u16 = 0;
    pub const CACHE_ME: u16 = 1 << 0;
    pub const FROM_CACHE: u16 = 1 << 1;
    pub const FROM_CACHE_LOSSLESS: u16 = 1 << 2;
}

/// Upper bound on cursor sprite dimensions. SPICE in practice caps at
/// 64×64 for hardware cursors and 128×128 for software; a 512×512 cap
/// is generous without being a DoS vector.
pub const MAX_CURSOR_DIM: u16 = 512;
/// Upper bound on cursor pixel-data payload, derived from
/// `MAX_CURSOR_DIM * MAX_CURSOR_DIM * 4 bytes` (alpha cursors).
pub const MAX_CURSOR_BYTES: usize = 512 * 512 * 4;

/// Cursor sprite metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorHeader {
    pub unique: u64,
    pub kind: u16,
    pub width: u16,
    pub height: u16,
    pub hot_spot_x: u16,
    pub hot_spot_y: u16,
}

impl CursorHeader {
    pub const SIZE: usize = 8 + 2 + 2 + 2 + 2 + 2;

    pub fn decode(r: &mut Reader) -> Result<Self> {
        Ok(Self {
            unique: r.u64()?,
            kind: r.u16()?,
            width: r.u16()?,
            height: r.u16()?,
            hot_spot_x: r.u16()?,
            hot_spot_y: r.u16()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u64(self.unique);
        w.u16(self.kind);
        w.u16(self.width);
        w.u16(self.height);
        w.u16(self.hot_spot_x);
        w.u16(self.hot_spot_y);
    }
}

/// A cursor sprite as sent on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub flags: u16,
    pub header: CursorHeader,
    /// Raw pixel bytes in the format described by `header.kind`. For
    /// ALPHA this is `width * height * 4` ARGB bytes. Empty when the
    /// `FROM_CACHE` flag is set (server is referring to a previously
    /// `CACHE_ME`'d entry by `header.unique`).
    pub data: Vec<u8>,
}

impl Cursor {
    pub fn decode(r: &mut Reader) -> Result<Self> {
        let flags = r.u16()?;
        let header = CursorHeader::decode(r)?;
        if header.width > MAX_CURSOR_DIM || header.height > MAX_CURSOR_DIM {
            return Err(crate::ProtoError::SizeTooLarge {
                declared: (header.width as u32).saturating_mul(header.height as u32),
                max: MAX_CURSOR_DIM as u32 * MAX_CURSOR_DIM as u32,
            });
        }
        // From-cache cursors have no inline payload — the rest of the
        // message buffer is therefore empty for the current read.
        let data = if flags & cursor_flag::FROM_CACHE != 0
            || flags & cursor_flag::FROM_CACHE_LOSSLESS != 0
        {
            Vec::new()
        } else {
            let n = r.remaining();
            r.bytes(n)?.to_vec()
        };
        if data.len() > MAX_CURSOR_BYTES {
            return Err(crate::ProtoError::SizeTooLarge {
                declared: data.len() as u32,
                max: MAX_CURSOR_BYTES as u32,
            });
        }
        Ok(Self {
            flags,
            header,
            data,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u16(self.flags);
        self.header.encode(w);
        w.bytes(&self.data);
    }
}

/// `SPICE_MSG_CURSOR_INIT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorInit {
    pub position_x: i16,
    pub position_y: i16,
    pub trail_length: u16,
    pub trail_frequency: u16,
    pub visible: u8,
    pub cursor: Cursor,
}

impl CursorInit {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        let position_x = r.i16()?;
        let position_y = r.i16()?;
        let trail_length = r.u16()?;
        let trail_frequency = r.u16()?;
        let visible = r.u8()?;
        let cursor = Cursor::decode(&mut r)?;
        Ok(Self {
            position_x,
            position_y,
            trail_length,
            trail_frequency,
            visible,
            cursor,
        })
    }
}

/// `SPICE_MSG_CURSOR_SET`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorSet {
    pub position_x: i16,
    pub position_y: i16,
    pub visible: u8,
    pub cursor: Cursor,
}

impl CursorSet {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        let position_x = r.i16()?;
        let position_y = r.i16()?;
        let visible = r.u8()?;
        let cursor = Cursor::decode(&mut r)?;
        Ok(Self {
            position_x,
            position_y,
            visible,
            cursor,
        })
    }
}

/// `SPICE_MSG_CURSOR_MOVE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorMove {
    pub position_x: i16,
    pub position_y: i16,
}

impl CursorMove {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self {
            position_x: r.i16()?,
            position_y: r.i16()?,
        })
    }
}

/// `SPICE_MSG_CURSOR_INVAL_ONE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorInvalOne {
    pub unique: u64,
}

impl CursorInvalOne {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self { unique: r.u64()? })
    }
}

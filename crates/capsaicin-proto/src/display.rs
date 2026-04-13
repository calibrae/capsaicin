//! Display-channel messages.
//!
//! This is a minimal subset: message-type constants plus the bodies of
//! `SURFACE_CREATE`, `SURFACE_DESTROY`, `DISPLAY_MODE`, and
//! `MONITORS_CONFIG`. The full drawing primitives (DRAW_FILL, DRAW_COPY,
//! stream frames) use nested marshal-tree structures and are layered on
//! top of this module separately.

use crate::types::{Reader, Writer};
use crate::Result;

/// Message-type constants for the display channel.
pub mod msg_type {
    // Server -> client
    pub const MODE: u16 = 101; // deprecated
    pub const MARK: u16 = 102;
    pub const RESET: u16 = 103;
    pub const COPY_BITS: u16 = 104;
    pub const INVAL_LIST: u16 = 105;
    pub const INVAL_ALL_PIXMAPS: u16 = 106;
    pub const INVAL_PALETTE: u16 = 107;
    pub const INVAL_ALL_PALETTES: u16 = 108;
    pub const STREAM_CREATE: u16 = 122;
    pub const STREAM_DATA: u16 = 123;
    pub const STREAM_CLIP: u16 = 124;
    pub const STREAM_DESTROY: u16 = 125;
    pub const STREAM_DESTROY_ALL: u16 = 126;
    pub const DRAW_FILL: u16 = 302;
    pub const DRAW_OPAQUE: u16 = 303;
    pub const DRAW_COPY: u16 = 304;
    pub const DRAW_BLEND: u16 = 305;
    pub const DRAW_BLACKNESS: u16 = 306;
    pub const DRAW_WHITENESS: u16 = 307;
    pub const DRAW_INVERS: u16 = 308;
    pub const DRAW_ROP3: u16 = 309;
    pub const DRAW_STROKE: u16 = 310;
    pub const DRAW_TEXT: u16 = 311;
    pub const DRAW_TRANSPARENT: u16 = 312;
    pub const DRAW_ALPHA_BLEND: u16 = 313;
    pub const SURFACE_CREATE: u16 = 314;
    pub const SURFACE_DESTROY: u16 = 315;
    pub const MONITORS_CONFIG: u16 = 317;
    pub const DRAW_COMPOSITE: u16 = 318;
    pub const STREAM_DATA_SIZED: u16 = 319;
    pub const STREAM_ACTIVATE_REPORT: u16 = 320;

    // Client -> server
    pub const INIT: u16 = 101;
    pub const STREAM_REPORT: u16 = 102;
    pub const PREFERRED_COMPRESSION: u16 = 103;
}

/// Surface pixel formats.
#[allow(non_upper_case_globals)]
pub mod surface_fmt {
    pub const INVALID: u32 = 0;
    pub const _1_A: u32 = 1;
    pub const _8_A: u32 = 8;
    pub const _16_555: u32 = 16;
    pub const _16_565: u32 = 80;
    pub const _32_xRGB: u32 = 32;
    pub const _32_ARGB: u32 = 96;
}

pub mod surface_flags {
    pub const PRIMARY: u32 = 1 << 0;
}

/// `SPICE_MSGC_DISPLAY_INIT` — first message the client sends on the
/// display channel. Without it some servers buffer output indefinitely.
/// 14 bytes on the wire (packed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayInit {
    pub pixmap_cache_id: u8,
    pub pixmap_cache_size: i64,
    pub glz_dictionary_id: u8,
    pub glz_dictionary_window_size: i32,
}

impl DisplayInit {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self {
            pixmap_cache_id: r.u8()?,
            pixmap_cache_size: r.u64()? as i64,
            glz_dictionary_id: r.u8()?,
            glz_dictionary_window_size: r.u32()? as i32,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u8(self.pixmap_cache_id);
        w.u64(self.pixmap_cache_size as u64);
        w.u8(self.glz_dictionary_id);
        w.u32(self.glz_dictionary_window_size as u32);
    }
}

/// `SPICE_MSG_DISPLAY_SURFACE_CREATE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceCreate {
    pub surface_id: u32,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub flags: u32,
}

impl SurfaceCreate {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self {
            surface_id: r.u32()?,
            width: r.u32()?,
            height: r.u32()?,
            format: r.u32()?,
            flags: r.u32()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.surface_id);
        w.u32(self.width);
        w.u32(self.height);
        w.u32(self.format);
        w.u32(self.flags);
    }
}

/// `SPICE_MSG_DISPLAY_SURFACE_DESTROY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceDestroy {
    pub surface_id: u32,
}

impl SurfaceDestroy {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self {
            surface_id: r.u32()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.surface_id);
    }
}

/// `SPICE_MSG_DISPLAY_MODE` (deprecated but still emitted by some servers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    pub x_res: u32,
    pub y_res: u32,
    pub bits: u32,
}

impl Mode {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self {
            x_res: r.u32()?,
            y_res: r.u32()?,
            bits: r.u32()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.x_res);
        w.u32(self.y_res);
        w.u32(self.bits);
    }
}

/// A single monitor head inside `MonitorsConfig`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Head {
    pub id: u32,
    pub surface_id: u32,
    pub width: u32,
    pub height: u32,
    pub x: u32,
    pub y: u32,
    pub flags: u32,
}

impl Head {
    pub fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self {
            id: r.u32()?,
            surface_id: r.u32()?,
            width: r.u32()?,
            height: r.u32()?,
            x: r.u32()?,
            y: r.u32()?,
            flags: r.u32()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.id);
        w.u32(self.surface_id);
        w.u32(self.width);
        w.u32(self.height);
        w.u32(self.x);
        w.u32(self.y);
        w.u32(self.flags);
    }
}

/// `SPICE_MSG_DISPLAY_MONITORS_CONFIG`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorsConfig {
    pub max_allowed: u16,
    pub heads: Vec<Head>,
}

impl MonitorsConfig {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        let count = r.u16()? as usize;
        let max_allowed = r.u16()?;
        let mut heads = Vec::with_capacity(count);
        for _ in 0..count {
            heads.push(Head::decode(&mut r)?);
        }
        Ok(Self { max_allowed, heads })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u16(self.heads.len() as u16);
        w.u16(self.max_allowed);
        for h in &self.heads {
            h.encode(w);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surface_create_roundtrip() {
        let m = SurfaceCreate {
            surface_id: 0,
            width: 1920,
            height: 1080,
            format: surface_fmt::_32_xRGB,
            flags: surface_flags::PRIMARY,
        };
        let mut w = Writer::new();
        m.encode(&mut w);
        assert_eq!(w.as_slice().len(), 20);
        assert_eq!(SurfaceCreate::decode(w.as_slice()).unwrap(), m);
    }

    #[test]
    fn monitors_config_roundtrip() {
        let m = MonitorsConfig {
            max_allowed: 4,
            heads: vec![Head {
                id: 0,
                surface_id: 0,
                width: 1920,
                height: 1080,
                x: 0,
                y: 0,
                flags: 0,
            }],
        };
        let mut w = Writer::new();
        m.encode(&mut w);
        assert_eq!(MonitorsConfig::decode(w.as_slice()).unwrap(), m);
    }
}

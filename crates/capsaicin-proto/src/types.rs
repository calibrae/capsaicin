//! Primitive types shared across SPICE messages.

use crate::{ProtoError, Result};

/// Cursor of a little-endian byte stream. Tracks how many bytes have been
/// consumed so message-body parsers can be composed linearly.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(ProtoError::Short {
                need: n,
                have: self.remaining(),
            });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn i16(&mut self) -> Result<i16> {
        Ok(self.u16()? as i16)
    }

    pub fn i32(&mut self) -> Result<i32> {
        Ok(self.u32()? as i32)
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.take(n)
    }
}

/// Append-only little-endian byte writer.
pub struct Writer {
    buf: Vec<u8>,
}

impl Default for Writer {
    fn default() -> Self {
        Self::new()
    }
}

impl Writer {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
        }
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn i16(&mut self, v: i16) {
        self.u16(v as u16);
    }
    pub fn i32(&mut self, v: i32) {
        self.u32(v as u32);
    }
    pub fn bytes(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point16 {
    pub x: i16,
    pub y: i16,
}

/// SPICE rectangle: inclusive top/left, exclusive bottom/right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub top: i32,
    pub left: i32,
    pub bottom: i32,
    pub right: i32,
}

impl Rect {
    pub fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self {
            top: r.i32()?,
            left: r.i32()?,
            bottom: r.i32()?,
            right: r.i32()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.i32(self.top);
        w.i32(self.left);
        w.i32(self.bottom);
        w.i32(self.right);
    }

    pub fn width(&self) -> i32 {
        self.right - self.left
    }
    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }
}

/// Identifier pair sent in `MAIN_CHANNELS_LIST`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelId {
    pub channel_type: u8,
    pub id: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_advances() {
        let mut r = Reader::new(&[1, 0, 0, 0, 2, 0]);
        assert_eq!(r.u32().unwrap(), 1);
        assert_eq!(r.u16().unwrap(), 2);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn reader_short() {
        let mut r = Reader::new(&[1]);
        assert!(matches!(r.u32(), Err(ProtoError::Short { .. })));
    }

    #[test]
    fn rect_roundtrip() {
        let r = Rect {
            top: 1,
            left: 2,
            bottom: 3,
            right: 4,
        };
        let mut w = Writer::new();
        r.encode(&mut w);
        let mut rr = Reader::new(w.as_slice());
        assert_eq!(Rect::decode(&mut rr).unwrap(), r);
    }
}

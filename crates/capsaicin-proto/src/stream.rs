//! Display-channel stream messages: `STREAM_CREATE`, `STREAM_DATA`,
//! `STREAM_DATA_SIZED`, `STREAM_CLIP`, `STREAM_DESTROY`,
//! `STREAM_DESTROY_ALL`.
//!
//! Streams carry a sequence of pre-encoded video frames (MJPEG, VP8/9,
//! H.264/5) for a region of a surface. The server uses them when it
//! detects sustained motion (cursor area, scrolling, video).

use crate::draw::Clip;
use crate::types::{Reader, Rect, Writer};
use crate::{ProtoError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VideoCodec {
    Mjpeg = 1,
    Vp8 = 2,
    H264 = 3,
    Vp9 = 4,
    H265 = 5,
}

impl VideoCodec {
    pub fn from_u8(v: u8) -> Result<Self> {
        Ok(match v {
            1 => Self::Mjpeg,
            2 => Self::Vp8,
            3 => Self::H264,
            4 => Self::Vp9,
            5 => Self::H265,
            _ => return Err(ProtoError::BadChannelType(v)),
        })
    }
}

pub mod stream_flags {
    /// Frame rows are top-to-bottom (matches normal raster order).
    pub const TOP_DOWN: u8 = 1 << 0;
}

/// `MSG_DISPLAY_STREAM_CREATE` (122).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamCreate {
    pub surface_id: u32,
    pub stream_id: u32,
    pub flags: u8,
    pub codec: VideoCodec,
    pub stamp: u64,
    pub stream_width: u32,
    pub stream_height: u32,
    pub src_width: u32,
    pub src_height: u32,
    pub dest: Rect,
    pub clip: Clip,
}

impl StreamCreate {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        let surface_id = r.u32()?;
        let stream_id = r.u32()?;
        let flags = r.u8()?;
        let codec = VideoCodec::from_u8(r.u8()?)?;
        let stamp = r.u64()?;
        let stream_width = r.u32()?;
        let stream_height = r.u32()?;
        let src_width = r.u32()?;
        let src_height = r.u32()?;
        let dest = Rect::decode(&mut r)?;
        let clip = Clip::decode(&mut r)?;
        Ok(Self {
            surface_id,
            stream_id,
            flags,
            codec,
            stamp,
            stream_width,
            stream_height,
            src_width,
            src_height,
            dest,
            clip,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.surface_id);
        w.u32(self.stream_id);
        w.u8(self.flags);
        w.u8(self.codec as u8);
        w.u64(self.stamp);
        w.u32(self.stream_width);
        w.u32(self.stream_height);
        w.u32(self.src_width);
        w.u32(self.src_height);
        self.dest.encode(w);
        self.clip.encode(w);
    }

    pub fn is_top_down(&self) -> bool {
        self.flags & stream_flags::TOP_DOWN != 0
    }
}

/// `StreamDataHeader` — shared prefix of stream payload messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamDataHeader {
    pub stream_id: u32,
    pub multi_media_time: u32,
}

impl StreamDataHeader {
    pub fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self {
            stream_id: r.u32()?,
            multi_media_time: r.u32()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.stream_id);
        w.u32(self.multi_media_time);
    }
}

/// `MSG_DISPLAY_STREAM_DATA` (123). Body is `data_size` bytes of
/// codec-encoded payload, copied out of the wire buffer for handoff to
/// the codec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamData {
    pub header: StreamDataHeader,
    pub data: Vec<u8>,
}

impl StreamData {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        let header = StreamDataHeader::decode(&mut r)?;
        let data_size = r.u32()? as usize;
        let data = r.bytes(data_size)?.to_vec();
        Ok(Self { header, data })
    }

    pub fn encode(&self, w: &mut Writer) {
        self.header.encode(w);
        w.u32(self.data.len() as u32);
        w.bytes(&self.data);
    }
}

/// `MSG_DISPLAY_STREAM_DATA_SIZED` (319). Frame carries its own
/// dimensions + dest rect, useful when the stream has been resized
/// on the fly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamDataSized {
    pub header: StreamDataHeader,
    pub width: u32,
    pub height: u32,
    pub dest: Rect,
    pub data: Vec<u8>,
}

impl StreamDataSized {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        let header = StreamDataHeader::decode(&mut r)?;
        let width = r.u32()?;
        let height = r.u32()?;
        let dest = Rect::decode(&mut r)?;
        let data_size = r.u32()? as usize;
        let data = r.bytes(data_size)?.to_vec();
        Ok(Self {
            header,
            width,
            height,
            dest,
            data,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        self.header.encode(w);
        w.u32(self.width);
        w.u32(self.height);
        self.dest.encode(w);
        w.u32(self.data.len() as u32);
        w.bytes(&self.data);
    }
}

/// `MSG_DISPLAY_STREAM_DESTROY` (125).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamDestroy {
    pub stream_id: u32,
}

impl StreamDestroy {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self {
            stream_id: r.u32()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.stream_id);
    }
}

/// `MSG_DISPLAY_STREAM_CLIP` (124).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamClip {
    pub stream_id: u32,
    pub clip: Clip,
}

impl StreamClip {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        let stream_id = r.u32()?;
        let clip = Clip::decode(&mut r)?;
        Ok(Self { stream_id, clip })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.stream_id);
        self.clip.encode(w);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_create_roundtrip_none_clip() {
        let m = StreamCreate {
            surface_id: 0,
            stream_id: 7,
            flags: stream_flags::TOP_DOWN,
            codec: VideoCodec::Mjpeg,
            stamp: 0x1234_5678_9ABC_DEF0,
            stream_width: 1920,
            stream_height: 1080,
            src_width: 1920,
            src_height: 1080,
            dest: Rect {
                top: 100,
                left: 200,
                bottom: 300,
                right: 400,
            },
            clip: Clip::None,
        };
        let mut w = Writer::new();
        m.encode(&mut w);
        // surface_id(4) + stream_id(4) + flags(1) + codec(1) + stamp(8) +
        // stream_w(4) + stream_h(4) + src_w(4) + src_h(4) + dest(16) +
        // clip(1) = 51 bytes.
        assert_eq!(w.as_slice().len(), 51);
        assert_eq!(StreamCreate::decode(w.as_slice()).unwrap(), m);
    }

    #[test]
    fn stream_data_roundtrip() {
        let m = StreamData {
            header: StreamDataHeader {
                stream_id: 1,
                multi_media_time: 0x0DEADBEE,
            },
            data: vec![0xFF, 0xD8, 0xFF, 0xE0, 0xFF, 0xD9],
        };
        let mut w = Writer::new();
        m.encode(&mut w);
        // header(8) + data_size(4) + 6 bytes = 18.
        assert_eq!(w.as_slice().len(), 18);
        assert_eq!(StreamData::decode(w.as_slice()).unwrap(), m);
    }

    #[test]
    fn stream_data_sized_roundtrip() {
        let m = StreamDataSized {
            header: StreamDataHeader {
                stream_id: 2,
                multi_media_time: 42,
            },
            width: 64,
            height: 48,
            dest: Rect { top: 0, left: 0, bottom: 48, right: 64 },
            data: vec![0xAA; 100],
        };
        let mut w = Writer::new();
        m.encode(&mut w);
        assert_eq!(StreamDataSized::decode(w.as_slice()).unwrap(), m);
    }

    #[test]
    fn stream_destroy_roundtrip() {
        let m = StreamDestroy { stream_id: 99 };
        let mut w = Writer::new();
        m.encode(&mut w);
        assert_eq!(StreamDestroy::decode(w.as_slice()).unwrap(), m);
    }
}

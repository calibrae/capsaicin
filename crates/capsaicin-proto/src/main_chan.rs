//! Main-channel message bodies.

use crate::limits::{MAX_CHANNELS_LIST, bounded_count};
use crate::types::{ChannelId, Reader, Writer};
use crate::Result;

/// `SPICE_MSG_MAIN_INIT` — first message on the main channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Init {
    pub session_id: u32,
    pub display_channels_hint: u32,
    pub supported_mouse_modes: u32,
    pub current_mouse_mode: u32,
    pub agent_connected: u32,
    pub agent_tokens: u32,
    pub multi_media_time: u32,
    pub ram_hint: u32,
}

impl Init {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self {
            session_id: r.u32()?,
            display_channels_hint: r.u32()?,
            supported_mouse_modes: r.u32()?,
            current_mouse_mode: r.u32()?,
            agent_connected: r.u32()?,
            agent_tokens: r.u32()?,
            multi_media_time: r.u32()?,
            ram_hint: r.u32()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.session_id);
        w.u32(self.display_channels_hint);
        w.u32(self.supported_mouse_modes);
        w.u32(self.current_mouse_mode);
        w.u32(self.agent_connected);
        w.u32(self.agent_tokens);
        w.u32(self.multi_media_time);
        w.u32(self.ram_hint);
    }
}

/// `SPICE_MSG_MAIN_CHANNELS_LIST` — advertises the sub-channels that the
/// server is ready to accept connections on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelsList {
    pub channels: Vec<ChannelId>,
}

impl ChannelsList {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        // Bound the count before pre-allocating: a hostile server
        // sending `n = 0xFFFFFFFF` would otherwise trigger an 8 GiB
        // `Vec::with_capacity` and panic the client.
        let n = bounded_count(r.u32()?, MAX_CHANNELS_LIST)?;
        let mut channels = Vec::with_capacity(n);
        for _ in 0..n {
            channels.push(ChannelId {
                channel_type: r.u8()?,
                id: r.u8()?,
            });
        }
        Ok(Self { channels })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.channels.len() as u32);
        for c in &self.channels {
            w.u8(c.channel_type);
            w.u8(c.id);
        }
    }
}

/// `SPICE_MSGC_MAIN_CLIENT_INFO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientInfo {
    pub cache_size: u64,
}

impl ClientInfo {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self {
            cache_size: r.u64()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u64(self.cache_size);
    }
}

/// `SPICE_MSG_MAIN_MOUSE_MODE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseMode {
    pub supported_modes: u32,
    pub current_mode: u32,
}

impl MouseMode {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self {
            supported_modes: r.u32()?,
            current_mode: r.u32()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.supported_modes);
        w.u32(self.current_mode);
    }
}

/// `SPICE_MSGC_MAIN_MOUSE_MODE_REQUEST` — client asks the server to
/// switch mouse reporting mode. The server responds with a
/// `SPICE_MSG_MAIN_MOUSE_MODE` carrying the mode it actually picked
/// (may differ from the request if the guest can't support it — e.g.
/// CLIENT requires an absolute pointing device like `usb-tablet`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseModeRequest {
    pub mode: u32,
}

impl MouseModeRequest {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self { mode: r.u32()? })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.mode);
    }
}

/// `SPICE_MSG_MAIN_MULTI_MEDIA_TIME`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MultiMediaTime {
    pub time: u32,
}

impl MultiMediaTime {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self { time: r.u32()? })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.time);
    }
}

/// Mouse mode bitmask values carried by `MouseMode` and `Init`.
pub mod mouse_mode {
    pub const SERVER: u32 = 1 << 0;
    pub const CLIENT: u32 = 1 << 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_roundtrip() {
        let m = Init {
            session_id: 0x1111_2222,
            display_channels_hint: 1,
            supported_mouse_modes: mouse_mode::SERVER | mouse_mode::CLIENT,
            current_mouse_mode: mouse_mode::SERVER,
            agent_connected: 0,
            agent_tokens: 10,
            multi_media_time: 0,
            ram_hint: 0,
        };
        let mut w = Writer::new();
        m.encode(&mut w);
        assert_eq!(w.as_slice().len(), 32);
        assert_eq!(Init::decode(w.as_slice()).unwrap(), m);
    }

    #[test]
    fn channels_list_roundtrip() {
        let list = ChannelsList {
            channels: vec![
                ChannelId {
                    channel_type: 2,
                    id: 0,
                },
                ChannelId {
                    channel_type: 3,
                    id: 0,
                },
                ChannelId {
                    channel_type: 4,
                    id: 0,
                },
            ],
        };
        let mut w = Writer::new();
        list.encode(&mut w);
        assert_eq!(ChannelsList::decode(w.as_slice()).unwrap(), list);
    }
}

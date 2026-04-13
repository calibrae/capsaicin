//! Message bodies common to every channel.

use crate::types::{Reader, Writer};
use crate::Result;

/// `SPICE_MSG_SET_ACK` — server tells the client how many messages to ack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetAck {
    pub generation: u32,
    pub window: u32,
}

impl SetAck {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self {
            generation: r.u32()?,
            window: r.u32()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.generation);
        w.u32(self.window);
    }
}

/// `SPICE_MSGC_ACK_SYNC` — client echoes back the generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckSync {
    pub generation: u32,
}

impl AckSync {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self {
            generation: r.u32()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.generation);
    }
}

/// `SPICE_MSG_PING` — server-initiated latency probe.
///
/// The body is a fixed 12-byte header followed by an arbitrary opaque
/// payload that the client must echo back verbatim in `PONG`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ping {
    pub id: u32,
    pub timestamp: u64,
    pub data: Vec<u8>,
}

impl Ping {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        let id = r.u32()?;
        let timestamp = r.u64()?;
        let rem = r.remaining();
        let data = r.bytes(rem)?.to_vec();
        Ok(Self {
            id,
            timestamp,
            data,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.id);
        w.u64(self.timestamp);
        w.bytes(&self.data);
    }
}

/// `SPICE_MSGC_PONG` — id + timestamp only. The opaque data carried by
/// a server `Ping` is **not** echoed back; it exists solely to let the
/// server measure downstream bandwidth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pong {
    pub id: u32,
    pub timestamp: u64,
}

impl Pong {
    pub fn from_ping(ping: &Ping) -> Self {
        Self {
            id: ping.id,
            timestamp: ping.timestamp,
        }
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self {
            id: r.u32()?,
            timestamp: r.u64()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.id);
        w.u64(self.timestamp);
    }
}

/// `SPICE_MSG_DISCONNECTING` / `SPICE_MSGC_DISCONNECTING`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Disconnecting {
    pub time_stamp: u64,
    pub reason: u32,
}

impl Disconnecting {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self {
            time_stamp: r.u64()?,
            reason: r.u32()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u64(self.time_stamp);
        w.u32(self.reason);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_ack_roundtrip() {
        let m = SetAck {
            generation: 1,
            window: 100,
        };
        let mut w = Writer::new();
        m.encode(&mut w);
        assert_eq!(SetAck::decode(w.as_slice()).unwrap(), m);
    }

    #[test]
    fn ping_roundtrip_preserves_opaque_payload() {
        let p = Ping {
            id: 7,
            timestamp: 0xdead_beef_cafe,
            data: vec![1, 2, 3, 4, 5],
        };
        let mut w = Writer::new();
        p.encode(&mut w);
        let decoded = Ping::decode(w.as_slice()).unwrap();
        assert_eq!(decoded, p);
    }
}

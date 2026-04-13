//! SPICE link handshake messages.
//!
//! Flow:
//!
//! ```text
//! client -> server :  LinkHeader + LinkMess + caps[]
//! server -> client :  LinkHeader + LinkReply + caps[]
//! client -> server :  encrypted ticket (128 bytes, RSA1024)
//! server -> client :  LinkResult (u32 error)
//! ```

use crate::enums::{
    ChannelType, LinkError, SPICE_MAGIC, SPICE_TICKET_PUBKEY_BYTES, SPICE_VERSION_MAJOR,
    SPICE_VERSION_MINOR,
};
use crate::{ProtoError, Result};

pub const LINK_HEADER_SIZE: usize = 16;
pub const LINK_MESS_FIXED_SIZE: usize = 18;
pub const LINK_REPLY_FIXED_SIZE: usize = 4 + SPICE_TICKET_PUBKEY_BYTES + 4 + 4 + 4;
pub const LINK_RESULT_SIZE: usize = 4;
pub const ENCRYPTED_TICKET_SIZE: usize = 128;

/// Prefixes every message during the link handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkHeader {
    pub magic: u32,
    pub major_version: u32,
    pub minor_version: u32,
    /// Length (in bytes) of the payload that follows this header.
    pub size: u32,
}

impl LinkHeader {
    pub fn new(size: u32) -> Self {
        Self {
            magic: SPICE_MAGIC,
            major_version: SPICE_VERSION_MAJOR,
            minor_version: SPICE_VERSION_MINOR,
            size,
        }
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < LINK_HEADER_SIZE {
            return Err(ProtoError::Short {
                need: LINK_HEADER_SIZE,
                have: buf.len(),
            });
        }
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        if magic != SPICE_MAGIC {
            return Err(ProtoError::BadMagic {
                expected: SPICE_MAGIC,
                got: magic,
            });
        }
        let major_version = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        let minor_version = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        let size = u32::from_le_bytes(buf[12..16].try_into().unwrap());
        Ok(Self {
            magic,
            major_version,
            minor_version,
            size,
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.magic.to_le_bytes());
        out.extend_from_slice(&self.major_version.to_le_bytes());
        out.extend_from_slice(&self.minor_version.to_le_bytes());
        out.extend_from_slice(&self.size.to_le_bytes());
    }
}

/// Client -> server. Selects the channel the connection should be attached to
/// and advertises the capabilities the client supports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkMess {
    pub connection_id: u32,
    pub channel_type: ChannelType,
    pub channel_id: u8,
    pub common_caps: Vec<u32>,
    pub channel_caps: Vec<u32>,
}

impl LinkMess {
    /// Size of the encoded message, including the trailing caps arrays.
    pub fn encoded_len(&self) -> usize {
        LINK_MESS_FIXED_SIZE + 4 * (self.common_caps.len() + self.channel_caps.len())
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < LINK_MESS_FIXED_SIZE {
            return Err(ProtoError::Short {
                need: LINK_MESS_FIXED_SIZE,
                have: buf.len(),
            });
        }
        let connection_id = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let channel_type = ChannelType::from_u8(buf[4])?;
        let channel_id = buf[5];
        let num_common_caps = u32::from_le_bytes(buf[6..10].try_into().unwrap()) as usize;
        let num_channel_caps = u32::from_le_bytes(buf[10..14].try_into().unwrap()) as usize;
        let caps_offset = u32::from_le_bytes(buf[14..18].try_into().unwrap()) as usize;

        let common_caps = read_caps(buf, caps_offset, num_common_caps)?;
        let channel_caps = read_caps(buf, caps_offset + 4 * num_common_caps, num_channel_caps)?;

        Ok(Self {
            connection_id,
            channel_type,
            channel_id,
            common_caps,
            channel_caps,
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.connection_id.to_le_bytes());
        out.push(self.channel_type as u8);
        out.push(self.channel_id);
        out.extend_from_slice(&(self.common_caps.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.channel_caps.len() as u32).to_le_bytes());
        out.extend_from_slice(&(LINK_MESS_FIXED_SIZE as u32).to_le_bytes());
        write_caps(out, &self.common_caps);
        write_caps(out, &self.channel_caps);
    }
}

/// Server -> client. Reports whether the link is accepted and returns the
/// RSA public key used to encrypt the auth ticket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkReply {
    pub error: LinkError,
    pub pub_key: [u8; SPICE_TICKET_PUBKEY_BYTES],
    pub common_caps: Vec<u32>,
    pub channel_caps: Vec<u32>,
}

impl LinkReply {
    pub fn encoded_len(&self) -> usize {
        LINK_REPLY_FIXED_SIZE + 4 * (self.common_caps.len() + self.channel_caps.len())
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < LINK_REPLY_FIXED_SIZE {
            return Err(ProtoError::Short {
                need: LINK_REPLY_FIXED_SIZE,
                have: buf.len(),
            });
        }
        let error = LinkError::from_u32(u32::from_le_bytes(buf[0..4].try_into().unwrap()))?;

        let pk_end = 4 + SPICE_TICKET_PUBKEY_BYTES;
        let mut pub_key = [0u8; SPICE_TICKET_PUBKEY_BYTES];
        pub_key.copy_from_slice(&buf[4..pk_end]);

        let num_common_caps =
            u32::from_le_bytes(buf[pk_end..pk_end + 4].try_into().unwrap()) as usize;
        let num_channel_caps =
            u32::from_le_bytes(buf[pk_end + 4..pk_end + 8].try_into().unwrap()) as usize;
        let caps_offset =
            u32::from_le_bytes(buf[pk_end + 8..pk_end + 12].try_into().unwrap()) as usize;

        let common_caps = read_caps(buf, caps_offset, num_common_caps)?;
        let channel_caps = read_caps(buf, caps_offset + 4 * num_common_caps, num_channel_caps)?;

        Ok(Self {
            error,
            pub_key,
            common_caps,
            channel_caps,
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.error as u32).to_le_bytes());
        out.extend_from_slice(&self.pub_key);
        out.extend_from_slice(&(self.common_caps.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.channel_caps.len() as u32).to_le_bytes());
        out.extend_from_slice(&(LINK_REPLY_FIXED_SIZE as u32).to_le_bytes());
        write_caps(out, &self.common_caps);
        write_caps(out, &self.channel_caps);
    }
}

/// Final u32 sent by the server after the ticket is validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkResult(pub LinkError);

impl LinkResult {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < LINK_RESULT_SIZE {
            return Err(ProtoError::Short {
                need: LINK_RESULT_SIZE,
                have: buf.len(),
            });
        }
        Ok(Self(LinkError::from_u32(u32::from_le_bytes(
            buf[0..4].try_into().unwrap(),
        ))?))
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.0 as u32).to_le_bytes());
    }
}

fn read_caps(buf: &[u8], offset: usize, count: usize) -> Result<Vec<u32>> {
    let end = offset
        .checked_add(4 * count)
        .ok_or(ProtoError::BadCapsOffset {
            offset: offset as u32,
            len: buf.len(),
        })?;
    if end > buf.len() {
        return Err(ProtoError::BadCapsOffset {
            offset: offset as u32,
            len: buf.len(),
        });
    }
    let mut caps = Vec::with_capacity(count);
    for i in 0..count {
        let base = offset + 4 * i;
        caps.push(u32::from_le_bytes(buf[base..base + 4].try_into().unwrap()));
    }
    Ok(caps)
}

fn write_caps(out: &mut Vec<u8>, caps: &[u32]) {
    for c in caps {
        out.extend_from_slice(&c.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_header_roundtrip() {
        let h = LinkHeader::new(42);
        let mut buf = Vec::new();
        h.encode(&mut buf);
        assert_eq!(buf.len(), LINK_HEADER_SIZE);
        assert_eq!(LinkHeader::decode(&buf).unwrap(), h);
    }

    #[test]
    fn link_header_rejects_bad_magic() {
        let mut buf = vec![0u8; LINK_HEADER_SIZE];
        buf[0..4].copy_from_slice(&0xdead_beefu32.to_le_bytes());
        assert!(matches!(
            LinkHeader::decode(&buf),
            Err(ProtoError::BadMagic { .. })
        ));
    }

    #[test]
    fn link_mess_roundtrip_with_caps() {
        let m = LinkMess {
            connection_id: 0,
            channel_type: ChannelType::Main,
            channel_id: 0,
            common_caps: vec![0b11],
            channel_caps: vec![0b1, 0b10],
        };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert_eq!(buf.len(), m.encoded_len());
        let decoded = LinkMess::decode(&buf).unwrap();
        assert_eq!(decoded, m);
    }

    #[test]
    fn link_reply_roundtrip() {
        let mut pub_key = [0u8; SPICE_TICKET_PUBKEY_BYTES];
        for (i, b) in pub_key.iter_mut().enumerate() {
            *b = i as u8;
        }
        let r = LinkReply {
            error: LinkError::Ok,
            pub_key,
            common_caps: vec![1, 2, 3],
            channel_caps: vec![42],
        };
        let mut buf = Vec::new();
        r.encode(&mut buf);
        assert_eq!(buf.len(), r.encoded_len());
        assert_eq!(LinkReply::decode(&buf).unwrap(), r);
    }

    #[test]
    fn link_result_roundtrip() {
        let r = LinkResult(LinkError::PermissionDenied);
        let mut buf = Vec::new();
        r.encode(&mut buf);
        assert_eq!(buf.len(), LINK_RESULT_SIZE);
        assert_eq!(LinkResult::decode(&buf).unwrap(), r);
    }
}

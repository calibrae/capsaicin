//! Per-message data headers that follow the link handshake.

use crate::{ProtoError, Result};

/// Wire size of the full [`DataHeader`] (packed).
pub const DATA_HEADER_SIZE: usize = 18;

/// Wire size of the [`MiniDataHeader`] (packed).
pub const MINI_DATA_HEADER_SIZE: usize = 6;

/// Full data header used when neither side advertises
/// `SPICE_COMMON_CAP_MINI_HEADER`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataHeader {
    pub serial: u64,
    pub msg_type: u16,
    pub size: u32,
    /// Offset from the start of the header to a `SpiceSubMessageList`, or 0.
    pub sub_list: u32,
}

impl DataHeader {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < DATA_HEADER_SIZE {
            return Err(ProtoError::Short {
                need: DATA_HEADER_SIZE,
                have: buf.len(),
            });
        }
        Ok(Self {
            serial: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            msg_type: u16::from_le_bytes(buf[8..10].try_into().unwrap()),
            size: u32::from_le_bytes(buf[10..14].try_into().unwrap()),
            sub_list: u32::from_le_bytes(buf[14..18].try_into().unwrap()),
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.serial.to_le_bytes());
        out.extend_from_slice(&self.msg_type.to_le_bytes());
        out.extend_from_slice(&self.size.to_le_bytes());
        out.extend_from_slice(&self.sub_list.to_le_bytes());
    }
}

/// Compact header used when both sides advertise
/// `SPICE_COMMON_CAP_MINI_HEADER`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MiniDataHeader {
    pub msg_type: u16,
    pub size: u32,
}

impl MiniDataHeader {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < MINI_DATA_HEADER_SIZE {
            return Err(ProtoError::Short {
                need: MINI_DATA_HEADER_SIZE,
                have: buf.len(),
            });
        }
        Ok(Self {
            msg_type: u16::from_le_bytes(buf[0..2].try_into().unwrap()),
            size: u32::from_le_bytes(buf[2..6].try_into().unwrap()),
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.msg_type.to_le_bytes());
        out.extend_from_slice(&self.size.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_header_roundtrip() {
        let h = DataHeader {
            serial: 0x0102030405060708,
            msg_type: 0x1234,
            size: 0xdead_beef,
            sub_list: 0,
        };
        let mut buf = Vec::new();
        h.encode(&mut buf);
        assert_eq!(buf.len(), DATA_HEADER_SIZE);
        assert_eq!(DataHeader::decode(&buf).unwrap(), h);
    }

    #[test]
    fn mini_header_roundtrip() {
        let h = MiniDataHeader {
            msg_type: 0x4242,
            size: 99,
        };
        let mut buf = Vec::new();
        h.encode(&mut buf);
        assert_eq!(buf.len(), MINI_DATA_HEADER_SIZE);
        assert_eq!(MiniDataHeader::decode(&buf).unwrap(), h);
    }

    #[test]
    fn data_header_short_buf() {
        assert!(matches!(
            DataHeader::decode(&[0u8; 4]),
            Err(ProtoError::Short { .. })
        ));
    }
}

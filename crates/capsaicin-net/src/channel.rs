//! Post-handshake data framing.
//!
//! Once the link handshake completes, both sides exchange length-prefixed
//! messages. The framing format depends on whether `MINI_HEADER` was
//! negotiated in common caps: 18-byte [`DataHeader`] vs 6-byte
//! [`MiniDataHeader`].

use bytes::BytesMut;
use capsaicin_proto::header::{
    DATA_HEADER_SIZE, DataHeader, MINI_DATA_HEADER_SIZE, MiniDataHeader,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{NetError, Result};

/// Refuse to allocate for a message bigger than this. Most SPICE messages
/// are tiny; framebuffer updates are the exception but still fit.
pub const DEFAULT_MAX_MESSAGE_SIZE: u32 = 32 * 1024 * 1024;

/// A framed message body plus the header fields that were on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub msg_type: u16,
    /// Present when full headers are in use.
    pub serial: Option<u64>,
    pub body: BytesMut,
}

pub struct Channel<S> {
    stream: S,
    mini_header: bool,
    send_serial: u64,
    max_message_size: u32,
}

impl<S> Channel<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub fn new(stream: S, mini_header: bool) -> Self {
        Self {
            stream,
            mini_header,
            // SPICE serials start at 1 — 0 is reserved.
            send_serial: 1,
            max_message_size: DEFAULT_MAX_MESSAGE_SIZE,
        }
    }

    pub fn set_max_message_size(&mut self, max: u32) {
        self.max_message_size = max;
    }

    pub fn mini_header(&self) -> bool {
        self.mini_header
    }

    pub fn get_ref(&self) -> &S {
        &self.stream
    }

    pub fn get_mut(&mut self) -> &mut S {
        &mut self.stream
    }

    pub fn into_inner(self) -> S {
        self.stream
    }

    pub async fn read_message(&mut self) -> Result<Message> {
        if self.mini_header {
            let mut hdr = [0u8; MINI_DATA_HEADER_SIZE];
            self.stream.read_exact(&mut hdr).await?;
            let h = MiniDataHeader::decode(&hdr)?;
            let body = self.read_body(h.size).await?;
            Ok(Message {
                msg_type: h.msg_type,
                serial: None,
                body,
            })
        } else {
            let mut hdr = [0u8; DATA_HEADER_SIZE];
            self.stream.read_exact(&mut hdr).await?;
            let h = DataHeader::decode(&hdr)?;
            let body = self.read_body(h.size).await?;
            Ok(Message {
                msg_type: h.msg_type,
                serial: Some(h.serial),
                body,
            })
        }
    }

    async fn read_body(&mut self, size: u32) -> Result<BytesMut> {
        if size > self.max_message_size {
            return Err(NetError::MessageTooLarge {
                size,
                max: self.max_message_size,
            });
        }
        let mut body = BytesMut::zeroed(size as usize);
        self.stream.read_exact(&mut body).await?;
        Ok(body)
    }

    pub async fn write_message(&mut self, msg_type: u16, body: &[u8]) -> Result<()> {
        if body.len() as u64 > u32::MAX as u64 {
            return Err(NetError::MessageTooLarge {
                size: u32::MAX,
                max: self.max_message_size,
            });
        }
        let size = body.len() as u32;
        if size > self.max_message_size {
            return Err(NetError::MessageTooLarge {
                size,
                max: self.max_message_size,
            });
        }

        let mut hdr = Vec::with_capacity(DATA_HEADER_SIZE);
        if self.mini_header {
            MiniDataHeader { msg_type, size }.encode(&mut hdr);
        } else {
            DataHeader {
                serial: self.send_serial,
                msg_type,
                size,
                sub_list: 0,
            }
            .encode(&mut hdr);
            self.send_serial = self.send_serial.wrapping_add(1);
        }
        self.stream.write_all(&hdr).await?;
        self.stream.write_all(body).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn roundtrip_mini_header() {
        let (a, b) = duplex(4096);
        let mut ca = Channel::new(a, true);
        let mut cb = Channel::new(b, true);

        ca.write_message(42, b"hello").await.unwrap();
        let m = cb.read_message().await.unwrap();
        assert_eq!(m.msg_type, 42);
        assert_eq!(m.serial, None);
        assert_eq!(&m.body[..], b"hello");
    }

    #[tokio::test]
    async fn roundtrip_full_header_increments_serial() {
        let (a, b) = duplex(4096);
        let mut ca = Channel::new(a, false);
        let mut cb = Channel::new(b, false);

        ca.write_message(7, b"x").await.unwrap();
        ca.write_message(8, b"yz").await.unwrap();

        let m1 = cb.read_message().await.unwrap();
        let m2 = cb.read_message().await.unwrap();
        assert_eq!(m1.serial, Some(1));
        assert_eq!(m2.serial, Some(2));
        assert_eq!(m1.msg_type, 7);
        assert_eq!(m2.msg_type, 8);
    }

    #[tokio::test]
    async fn rejects_oversized_declared_size() {
        let (a, b) = duplex(4096);
        let mut ca = Channel::new(a, true);
        let mut cb = Channel::new(b, true);
        cb.set_max_message_size(8);

        // Fake a mini header that claims a gigantic body.
        let hdr = MiniDataHeader {
            msg_type: 1,
            size: 9_999_999,
        };
        let mut bytes = Vec::new();
        hdr.encode(&mut bytes);
        ca.get_mut().write_all(&bytes).await.unwrap();

        let err = cb.read_message().await.unwrap_err();
        assert!(matches!(err, NetError::MessageTooLarge { .. }));
    }
}

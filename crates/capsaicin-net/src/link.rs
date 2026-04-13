//! Client-side link handshake driver.

use capsaicin_proto::caps::{self, CapSet};
use capsaicin_proto::enums::{ChannelType, LinkError};
use capsaicin_proto::link::{
    ENCRYPTED_TICKET_SIZE, LINK_HEADER_SIZE, LINK_RESULT_SIZE, LinkHeader, LinkMess, LinkReply,
    LinkResult,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{Channel, NetError, Result, auth};

/// Inputs for the client-side handshake.
#[derive(Debug, Clone)]
pub struct LinkOptions<'a> {
    pub connection_id: u32,
    pub channel_type: ChannelType,
    pub channel_id: u8,
    pub password: &'a str,
    pub common_caps: CapSet,
    pub channel_caps: CapSet,
}

impl<'a> LinkOptions<'a> {
    /// Sensible defaults: advertise `AUTH_SPICE` and `MINI_HEADER`.
    pub fn new(channel_type: ChannelType) -> Self {
        Self {
            connection_id: 0,
            channel_type,
            channel_id: 0,
            password: "",
            common_caps: CapSet::with_caps([caps::common::AUTH_SPICE, caps::common::MINI_HEADER]),
            channel_caps: CapSet::new(),
        }
    }
}

/// Drive the client side of the SPICE link handshake, then return a framed
/// [`Channel`] ready for data exchange.
pub async fn link_client<S>(mut stream: S, opts: LinkOptions<'_>) -> Result<Channel<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // 1. Build and send LinkHeader + LinkMess.
    let mess = LinkMess {
        connection_id: opts.connection_id,
        channel_type: opts.channel_type,
        channel_id: opts.channel_id,
        common_caps: opts.common_caps.words().to_vec(),
        channel_caps: opts.channel_caps.words().to_vec(),
    };
    let mess_len = mess.encoded_len();
    let mut out = Vec::with_capacity(LINK_HEADER_SIZE + mess_len);
    LinkHeader::new(mess_len as u32).encode(&mut out);
    mess.encode(&mut out);
    stream.write_all(&out).await?;

    // 2. Read the server LinkHeader and validate magic/version.
    let mut hdr_buf = [0u8; LINK_HEADER_SIZE];
    stream.read_exact(&mut hdr_buf).await?;
    let hdr = LinkHeader::decode(&hdr_buf)?;

    // 3. Read the LinkReply (fixed part + caps).
    let mut reply_buf = vec![0u8; hdr.size as usize];
    stream.read_exact(&mut reply_buf).await?;
    let reply = LinkReply::decode(&reply_buf)?;
    if reply.error != LinkError::Ok {
        return Err(NetError::Link(reply.error));
    }

    // 4. Encrypt the ticket and send it (raw 128 bytes, no header).
    let ticket = auth::encrypt_ticket(&reply.pub_key, opts.password)?;
    debug_assert_eq!(ticket.len(), ENCRYPTED_TICKET_SIZE);
    stream.write_all(&ticket).await?;

    // 5. Read the final LinkResult.
    let mut result_buf = [0u8; LINK_RESULT_SIZE];
    stream.read_exact(&mut result_buf).await?;
    let result = LinkResult::decode(&result_buf)?;
    if result.0 != LinkError::Ok {
        return Err(NetError::Link(result.0));
    }

    // 6. Negotiate framing. Mini header is used only when BOTH sides set it.
    let server_common = CapSet(reply.common_caps);
    let use_mini = opts.common_caps.has(caps::common::MINI_HEADER)
        && server_common.has(caps::common::MINI_HEADER);

    Ok(Channel::new(stream, use_mini))
}

#[cfg(test)]
mod tests {
    use super::*;
    use capsaicin_proto::enums::SPICE_TICKET_PUBKEY_BYTES;
    use rsa::pkcs1v15::Pkcs1v15Encrypt;
    use rsa::pkcs8::EncodePublicKey;
    use rsa::{Oaep, RsaPrivateKey, RsaPublicKey};
    use sha1::Sha1;
    use tokio::io::duplex;

    /// Minimal server stub: accept the link, reply with a generated RSA key,
    /// decrypt the ticket, assert the password, and acknowledge.
    async fn fake_server<S>(
        mut stream: S,
        priv_key: RsaPrivateKey,
        expected_password: &str,
        server_common_caps: CapSet,
    ) where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        // Read client link.
        let mut hdr_buf = [0u8; LINK_HEADER_SIZE];
        stream.read_exact(&mut hdr_buf).await.unwrap();
        let hdr = LinkHeader::decode(&hdr_buf).unwrap();
        let mut mess_buf = vec![0u8; hdr.size as usize];
        stream.read_exact(&mut mess_buf).await.unwrap();
        let _mess = LinkMess::decode(&mess_buf).unwrap();

        // Reply with a real RSA pubkey in DER form.
        let der = RsaPublicKey::from(&priv_key).to_public_key_der().unwrap();
        let mut pk = [0u8; SPICE_TICKET_PUBKEY_BYTES];
        pk.copy_from_slice(der.as_bytes());
        let reply = LinkReply {
            error: LinkError::Ok,
            pub_key: pk,
            common_caps: server_common_caps.words().to_vec(),
            channel_caps: vec![],
        };
        let mut out = Vec::new();
        LinkHeader::new(reply.encoded_len() as u32).encode(&mut out);
        reply.encode(&mut out);
        stream.write_all(&out).await.unwrap();

        // Read encrypted ticket and decrypt.
        let mut ct = [0u8; ENCRYPTED_TICKET_SIZE];
        stream.read_exact(&mut ct).await.unwrap();
        let pt = priv_key.decrypt(Oaep::new::<Sha1>(), &ct).unwrap();
        assert_eq!(&pt[..pt.len() - 1], expected_password.as_bytes());

        // Send LinkResult::Ok.
        let mut r = Vec::new();
        LinkResult(LinkError::Ok).encode(&mut r);
        stream.write_all(&r).await.unwrap();

        // Silence unused warning.
        let _ = Pkcs1v15Encrypt;
    }

    #[tokio::test]
    async fn client_handshake_negotiates_mini_header() {
        let mut rng = rand::rngs::OsRng;
        let priv_key = RsaPrivateKey::new(&mut rng, 1024).unwrap();

        let (client, server) = duplex(8192);
        let server_caps = CapSet::with_caps([caps::common::MINI_HEADER]);

        let server_priv = priv_key.clone();
        let server_task = tokio::spawn(async move {
            fake_server(server, server_priv, "s3cret", server_caps).await;
        });

        let opts = LinkOptions {
            connection_id: 0,
            channel_type: ChannelType::Main,
            channel_id: 0,
            password: "s3cret",
            common_caps: CapSet::with_caps([caps::common::AUTH_SPICE, caps::common::MINI_HEADER]),
            channel_caps: CapSet::new(),
        };
        let channel = link_client(client, opts).await.unwrap();
        assert!(channel.mini_header());

        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn falls_back_to_full_header_when_server_lacks_cap() {
        let mut rng = rand::rngs::OsRng;
        let priv_key = RsaPrivateKey::new(&mut rng, 1024).unwrap();

        let (client, server) = duplex(8192);
        let server_caps = CapSet::new(); // no MINI_HEADER

        let server_priv = priv_key.clone();
        let server_task = tokio::spawn(async move {
            fake_server(server, server_priv, "pw", server_caps).await;
        });

        let mut opts = LinkOptions::new(ChannelType::Main);
        opts.password = "pw";
        let channel = link_client(client, opts).await.unwrap();
        assert!(!channel.mini_header());

        server_task.await.unwrap();
    }
}

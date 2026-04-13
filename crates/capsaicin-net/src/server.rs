//! Server-side link handshake.

use capsaicin_proto::caps::{self, CapSet};
use capsaicin_proto::enums::{ChannelType, LinkError, SPICE_TICKET_PUBKEY_BYTES};
use capsaicin_proto::limits::{MAX_LINK_PAYLOAD, bounded_size};
use capsaicin_proto::link::{
    ENCRYPTED_TICKET_SIZE, LINK_HEADER_SIZE, LinkHeader, LinkMess, LinkReply, LinkResult,
};
use rsa::{Oaep, RsaPrivateKey, RsaPublicKey, pkcs8::EncodePublicKey};
use sha1::Sha1;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{Channel, NetError, Result};

/// Result of a successful server-side handshake.
pub struct AcceptedLink<S> {
    pub channel: Channel<S>,
    pub connection_id: u32,
    pub channel_type: ChannelType,
    pub channel_id: u8,
    pub client_common_caps: CapSet,
    pub client_channel_caps: CapSet,
}

/// Knobs for [`link_server`].
pub struct ServerLinkOptions<'a> {
    pub priv_key: &'a RsaPrivateKey,
    pub expected_password: &'a str,
    pub server_common_caps: CapSet,
    pub server_channel_caps: CapSet,
}

impl<'a> ServerLinkOptions<'a> {
    pub fn new(priv_key: &'a RsaPrivateKey, expected_password: &'a str) -> Self {
        Self {
            priv_key,
            expected_password,
            server_common_caps: CapSet::with_caps([
                caps::common::AUTH_SPICE,
                caps::common::MINI_HEADER,
            ]),
            server_channel_caps: CapSet::new(),
        }
    }
}

/// Drive the server side of the link handshake. On success the returned
/// [`AcceptedLink`] exposes a framed [`Channel`] together with the channel
/// identifier and caps the client advertised.
pub async fn link_server<S>(mut stream: S, opts: ServerLinkOptions<'_>) -> Result<AcceptedLink<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // 1. Read client LinkHeader + LinkMess.
    let mut hdr_buf = [0u8; LINK_HEADER_SIZE];
    stream.read_exact(&mut hdr_buf).await?;
    let hdr = LinkHeader::decode(&hdr_buf)?;
    // Hostile client could send `size = 0xFFFFFFFF` and OOM the host
    // before authentication has run; cap to MAX_LINK_PAYLOAD.
    let mess_size = bounded_size(hdr.size, MAX_LINK_PAYLOAD)?;
    let mut mess_buf = vec![0u8; mess_size];
    stream.read_exact(&mut mess_buf).await?;
    let mess = LinkMess::decode(&mess_buf)?;

    // 2. Encode our public key as DER (SubjectPublicKeyInfo, 162 bytes).
    let der = RsaPublicKey::from(opts.priv_key)
        .to_public_key_der()
        .map_err(|_| NetError::BadServerKey)?;
    if der.as_bytes().len() != SPICE_TICKET_PUBKEY_BYTES {
        return Err(NetError::BadServerKey);
    }
    let mut pub_key = [0u8; SPICE_TICKET_PUBKEY_BYTES];
    pub_key.copy_from_slice(der.as_bytes());

    // 3. Send LinkHeader + LinkReply.
    let reply = LinkReply {
        error: LinkError::Ok,
        pub_key,
        common_caps: opts.server_common_caps.words().to_vec(),
        channel_caps: opts.server_channel_caps.words().to_vec(),
    };
    let mut out = Vec::with_capacity(LINK_HEADER_SIZE + reply.encoded_len());
    LinkHeader::new(reply.encoded_len() as u32).encode(&mut out);
    reply.encode(&mut out);
    stream.write_all(&out).await?;

    // 4. Read encrypted ticket and validate against expected password.
    let mut ct = [0u8; ENCRYPTED_TICKET_SIZE];
    stream.read_exact(&mut ct).await?;
    let result_code = match opts.priv_key.decrypt(Oaep::new::<Sha1>(), &ct) {
        Ok(pt) => {
            // Expect password + trailing null.
            let pw = if pt.last().copied() == Some(0) {
                &pt[..pt.len() - 1]
            } else {
                &pt[..]
            };
            if pw == opts.expected_password.as_bytes() {
                LinkError::Ok
            } else {
                LinkError::PermissionDenied
            }
        }
        Err(_) => LinkError::PermissionDenied,
    };

    // 5. Send LinkResult. On failure, return after writing it.
    let mut r = Vec::with_capacity(4);
    LinkResult(result_code).encode(&mut r);
    stream.write_all(&r).await?;
    if result_code != LinkError::Ok {
        return Err(NetError::Link(result_code));
    }

    // 6. Negotiate framing.
    let client_common = CapSet(mess.common_caps);
    let client_channel = CapSet(mess.channel_caps);
    let use_mini = client_common.has(caps::common::MINI_HEADER)
        && opts.server_common_caps.has(caps::common::MINI_HEADER);

    Ok(AcceptedLink {
        channel: Channel::new(stream, use_mini),
        connection_id: mess.connection_id,
        channel_type: mess.channel_type,
        channel_id: mess.channel_id,
        client_common_caps: client_common,
        client_channel_caps: client_channel,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LinkOptions, link_client};
    use rand::rngs::OsRng;
    use tokio::io::duplex;

    #[tokio::test]
    async fn client_and_server_complete_handshake() {
        let mut rng = OsRng;
        let priv_key = RsaPrivateKey::new(&mut rng, 1024).unwrap();
        let pk_server = priv_key.clone();

        let (a, b) = duplex(8192);

        let server_task = tokio::spawn(async move {
            let opts = ServerLinkOptions::new(&pk_server, "sesame");
            link_server(b, opts).await.unwrap()
        });

        let client_task = tokio::spawn(async move {
            let mut opts = LinkOptions::new(ChannelType::Main);
            opts.password = "sesame";
            link_client(a, opts).await.unwrap()
        });

        let accepted = server_task.await.unwrap();
        let client = client_task.await.unwrap();

        assert_eq!(accepted.channel_type, ChannelType::Main);
        assert!(accepted.channel.mini_header());
        assert!(client.mini_header());
    }

    #[tokio::test]
    async fn server_rejects_wrong_password() {
        let mut rng = OsRng;
        let priv_key = RsaPrivateKey::new(&mut rng, 1024).unwrap();
        let pk_server = priv_key.clone();

        let (a, b) = duplex(8192);

        let server_task = tokio::spawn(async move {
            let opts = ServerLinkOptions::new(&pk_server, "correct");
            link_server(b, opts).await
        });

        let client_task = tokio::spawn(async move {
            let mut opts = LinkOptions::new(ChannelType::Main);
            opts.password = "wrong";
            link_client(a, opts).await
        });

        let server_res = server_task.await.unwrap();
        let client_res = client_task.await.unwrap();
        assert!(matches!(
            server_res,
            Err(NetError::Link(LinkError::PermissionDenied))
        ));
        assert!(matches!(
            client_res,
            Err(NetError::Link(LinkError::PermissionDenied))
        ));
    }

    /// Pre-auth allocation DoS regression: a hostile client sends a
    /// LinkHeader with `size = 0xFFFFFFFF`. Without the
    /// MAX_LINK_PAYLOAD cap this would `vec![0u8; 4_294_967_295]` and
    /// OOM the host before validating anything else.
    #[tokio::test]
    async fn server_rejects_oversized_link_payload_pre_auth() {
        use tokio::io::AsyncWriteExt;
        let mut rng = OsRng;
        let priv_key = RsaPrivateKey::new(&mut rng, 1024).unwrap();

        let (mut client, server) = duplex(8192);
        let server_task = tokio::spawn(async move {
            let opts = ServerLinkOptions::new(&priv_key, "pw");
            link_server(server, opts).await
        });
        // SPICE_MAGIC + version 2.2 + size = u32::MAX
        let mut bogus_header = Vec::new();
        bogus_header.extend_from_slice(&0x5144_4552u32.to_le_bytes()); // "REDQ"
        bogus_header.extend_from_slice(&2u32.to_le_bytes()); // major
        bogus_header.extend_from_slice(&2u32.to_le_bytes()); // minor
        bogus_header.extend_from_slice(&u32::MAX.to_le_bytes()); // size
        client.write_all(&bogus_header).await.unwrap();
        // Drop client to close the stream; server should error promptly
        // rather than allocating 4 GiB and stalling.
        drop(client);
        let res = server_task.await.unwrap();
        let err = res.err().expect("server should have errored, not accepted");
        assert!(
            matches!(
                err,
                NetError::Proto(capsaicin_proto::ProtoError::SizeTooLarge { .. })
            ),
            "expected SizeTooLarge, got {err:?}"
        );
    }
}

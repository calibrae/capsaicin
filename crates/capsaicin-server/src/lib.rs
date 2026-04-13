//! Embeddable SPICE server core.
//!
//! Provides the pieces an application needs to accept SPICE clients:
//!
//! - [`Server`]: owns the RSA key, password, and session-id counter.
//! - [`Server::accept`]: runs the link handshake on a stream.
//! - [`serve_main_bootstrap`]: drives the post-link bootstrap of the main
//!   channel (`MAIN_INIT`, `CLIENT_INFO`/`ATTACH_CHANNELS`, `CHANNELS_LIST`).
//!
//! Sub-channel dispatch, surface creation, frame generation — all left to
//! the embedder. The library is deliberately thin.

use std::sync::atomic::{AtomicU32, Ordering};

use capsaicin_net::{AcceptedLink, Channel, ServerLinkOptions, link_server};
use capsaicin_proto::common;
use capsaicin_proto::enums::{
    ChannelType, main_msg, main_msgc, msg as common_msg, msgc as common_msgc,
};
use capsaicin_proto::main_chan::{self, ChannelsList, Init};
use capsaicin_proto::types::{ChannelId, Writer};
use rand::rngs::OsRng;
use rsa::RsaPrivateKey;
use tokio::io::{AsyncRead, AsyncWrite};

pub use capsaicin_net::NetError;
pub type Result<T> = std::result::Result<T, NetError>;

pub struct Server {
    priv_key: RsaPrivateKey,
    password: String,
    next_session_id: AtomicU32,
}

impl Server {
    /// Generate a fresh 1024-bit RSA keypair and build a server.
    pub fn new(password: impl Into<String>) -> Result<Self> {
        let mut rng = OsRng;
        let priv_key = RsaPrivateKey::new(&mut rng, 1024)
            .map_err(|e| NetError::RsaEncrypt(e.to_string()))?;
        Ok(Self::with_key(priv_key, password))
    }

    pub fn with_key(priv_key: RsaPrivateKey, password: impl Into<String>) -> Self {
        Self {
            priv_key,
            password: password.into(),
            next_session_id: AtomicU32::new(1),
        }
    }

    pub fn password(&self) -> &str {
        &self.password
    }

    /// Allocate a new session id. Session ids must be non-zero so clients
    /// can distinguish "new session" from "reattach" semantics.
    pub fn new_session_id(&self) -> u32 {
        let mut id = self.next_session_id.fetch_add(1, Ordering::Relaxed);
        if id == 0 {
            id = self.next_session_id.fetch_add(1, Ordering::Relaxed);
        }
        id
    }

    /// Accept a single client stream — runs the link handshake only.
    pub async fn accept<S>(&self, stream: S) -> Result<AcceptedLink<S>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let opts = ServerLinkOptions::new(&self.priv_key, &self.password);
        link_server(stream, opts).await
    }
}

/// After [`Server::accept`] returns a main-channel link, call this helper
/// to send `MAIN_INIT` and wait for the client's `ATTACH_CHANNELS`, then
/// reply with the sub-channel list.
///
/// `session_id` should come from [`Server::new_session_id`] for the first
/// main-channel connection, and be echoed by clients reconnecting for
/// sub-channels.
pub async fn serve_main_bootstrap<S>(
    channel: &mut Channel<S>,
    session_id: u32,
    available_channels: &[ChannelId],
    init_overrides: Option<Init>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Tell the client how often to ACK. Window of 0 disables server-side
    // ACK tracking for now.
    let mut w = Writer::new();
    common::SetAck {
        generation: 1,
        window: 0,
    }
    .encode(&mut w);
    channel
        .write_message(common_msg::SET_ACK, w.as_slice())
        .await?;

    // MAIN_INIT.
    let init = init_overrides.unwrap_or(Init {
        session_id,
        display_channels_hint: 1,
        supported_mouse_modes: main_chan::mouse_mode::SERVER | main_chan::mouse_mode::CLIENT,
        current_mouse_mode: main_chan::mouse_mode::SERVER,
        agent_connected: 0,
        agent_tokens: 0,
        multi_media_time: 0,
        ram_hint: 0,
    });
    let mut w = Writer::new();
    init.encode(&mut w);
    channel
        .write_message(main_msg::INIT, w.as_slice())
        .await?;

    // Loop until we've seen ATTACH_CHANNELS, answering ACK_SYNC / PING.
    loop {
        let msg = channel.read_message().await?;
        match msg.msg_type {
            common_msgc::ACK_SYNC | common_msgc::ACK => {
                // Client caught up on ACK window; nothing else to do.
            }
            common_msgc::PONG => {
                // Latency sample ignored in MVP.
            }
            main_msgc::CLIENT_INFO => {
                let _info = main_chan::ClientInfo::decode(&msg.body)?;
            }
            main_msgc::ATTACH_CHANNELS => break,
            common_msgc::DISCONNECTING => break,
            _ => {
                tracing::debug!(
                    msg_type = msg.msg_type,
                    len = msg.body.len(),
                    "server: unhandled pre-attach client message"
                );
            }
        }
    }

    // Reply with the full channel list.
    let list = ChannelsList {
        channels: available_channels.to_vec(),
    };
    let mut w = Writer::new();
    list.encode(&mut w);
    channel
        .write_message(main_msg::CHANNELS_LIST, w.as_slice())
        .await
}

/// Convenience: the channel list most minimal servers expose.
pub fn default_channels() -> Vec<ChannelId> {
    vec![
        ChannelId {
            channel_type: ChannelType::Main as u8,
            id: 0,
        },
        ChannelId {
            channel_type: ChannelType::Display as u8,
            id: 0,
        },
        ChannelId {
            channel_type: ChannelType::Inputs as u8,
            id: 0,
        },
    ]
}

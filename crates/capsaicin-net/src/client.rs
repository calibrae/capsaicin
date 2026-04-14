//! High-level SPICE client: drive main-channel bootstrap, discover and
//! attach sub-channels.

use capsaicin_proto::caps::CapSet;
use capsaicin_proto::enums::{
    ChannelType, main_msg, main_msgc, msg as common_msg, msgc as common_msgc,
};
use capsaicin_proto::common;
use capsaicin_proto::main_chan::{ChannelsList, Init, MouseModeRequest, mouse_mode};
use capsaicin_proto::types::{ChannelId, Writer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use crate::{Channel, LinkOptions, NetError, Result, SpiceStream, TlsConfig, link_client};
use crate::tls::connect_tls;

/// Main channel after bootstrap: `Init` read, channel list discovered,
/// ready to reply to subsequent ACK/PING messages.
pub struct MainConnection<S> {
    pub channel: Channel<S>,
    pub session_id: u32,
    pub init: Init,
    pub available_channels: Vec<ChannelId>,
}

impl MainConnection<SpiceStream> {
    /// Dial a SPICE server over plain TCP, run the main-channel bootstrap,
    /// and return the populated [`MainConnection`].
    pub async fn connect(addr: &str, password: &str) -> Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        let mut opts = LinkOptions::new(ChannelType::Main);
        opts.password = password;
        let channel = link_client(SpiceStream::Plain(stream), opts).await?;
        Self::bootstrap(channel).await
    }

    /// Dial a SPICE server over TLS and run the main-channel bootstrap.
    pub async fn connect_tls(addr: &str, password: &str, tls: TlsConfig) -> Result<Self> {
        let stream = connect_tls(addr, tls).await?;
        let mut opts = LinkOptions::new(ChannelType::Main);
        opts.password = password;
        let channel = link_client(SpiceStream::Tls(Box::new(stream)), opts).await?;
        Self::bootstrap(channel).await
    }
}

impl<S> MainConnection<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Bootstrap an already-linked main channel: wait for `MAIN_INIT`, send
    /// `ATTACH_CHANNELS`, wait for `CHANNELS_LIST`, transparently answering
    /// common-channel control messages in the meantime.
    pub async fn bootstrap(mut channel: Channel<S>) -> Result<Self> {
        let mut init: Option<Init> = None;
        let mut attached = false;
        let mut channels_list: Option<ChannelsList> = None;

        while channels_list.is_none() {
            let msg = channel.read_message().await?;
            match msg.msg_type {
                common_msg::SET_ACK => {
                    let ack = common::SetAck::decode(&msg.body)?;
                    let mut w = Writer::new();
                    common::AckSync {
                        generation: ack.generation,
                    }
                    .encode(&mut w);
                    channel
                        .write_message(common_msgc::ACK_SYNC, w.as_slice())
                        .await?;
                }
                common_msg::PING => {
                    let ping = common::Ping::decode(&msg.body)?;
                    let mut w = Writer::new();
                    common::Pong::from_ping(&ping).encode(&mut w);
                    channel
                        .write_message(common_msgc::PONG, w.as_slice())
                        .await?;
                }
                common_msg::NOTIFY | common_msg::LIST | common_msg::WAIT_FOR_CHANNELS => {
                    // Advisory, ignore for now.
                }
                main_msg::INIT => {
                    let parsed = Init::decode(&msg.body)?;
                    // Request CLIENT mouse mode if the guest supports
                    // it. SERVER mode silently drops absolute
                    // MousePosition events, so CLIENT is strictly
                    // better UX whenever a tablet-style input device
                    // is available. The server replies with a
                    // MAIN_MOUSE_MODE carrying the mode it actually
                    // chose (falls back to SERVER if no tablet).
                    if parsed.supported_mouse_modes & mouse_mode::CLIENT != 0
                        && parsed.current_mouse_mode != mouse_mode::CLIENT
                    {
                        let mut w = Writer::new();
                        MouseModeRequest { mode: mouse_mode::CLIENT }.encode(&mut w);
                        channel
                            .write_message(main_msgc::MOUSE_MODE_REQUEST, w.as_slice())
                            .await?;
                    }
                    init = Some(parsed);
                    // MSGC_MAIN_CLIENT_INFO is deprecated (removed in
                    // modern SPICE); sending it causes QEMU to drop the
                    // connection. Go straight to ATTACH_CHANNELS.
                    channel.write_message(main_msgc::ATTACH_CHANNELS, &[]).await?;
                    attached = true;
                }
                main_msg::CHANNELS_LIST => {
                    channels_list = Some(ChannelsList::decode(&msg.body)?);
                }
                main_msg::MOUSE_MODE
                | main_msg::MULTI_MEDIA_TIME
                | main_msg::NAME
                | main_msg::UUID
                | main_msg::AGENT_CONNECTED
                | main_msg::AGENT_CONNECTED_TOKENS
                | main_msg::AGENT_DISCONNECTED
                | main_msg::AGENT_DATA
                | main_msg::AGENT_TOKEN => {
                    // Out of scope for bootstrap — accepted but not parsed.
                }
                _ => {
                    tracing::debug!(
                        msg_type = msg.msg_type,
                        len = msg.body.len(),
                        "main: unhandled pre-attach message"
                    );
                }
            }
        }

        let init = init.ok_or(NetError::Proto(
            capsaicin_proto::ProtoError::BadLinkError(0),
        ))?;
        let _ = attached; // explicit: we did send ATTACH before CHANNELS_LIST

        Ok(Self {
            channel,
            session_id: init.session_id,
            init,
            available_channels: channels_list.unwrap().channels,
        })
    }
}

/// Open a sub-channel TCP connection and complete the link handshake, using
/// the `session_id` that was handed out on the main channel. If `tls` is
/// `Some`, the underlying socket is wrapped in TLS first.
pub async fn connect_sub_channel(
    addr: &str,
    session_id: u32,
    channel_type: ChannelType,
    channel_id: u8,
    password: &str,
    channel_caps: CapSet,
    tls: Option<TlsConfig>,
) -> Result<Channel<SpiceStream>> {
    let stream = match tls {
        Some(cfg) => SpiceStream::Tls(Box::new(connect_tls(addr, cfg).await?)),
        None => {
            let s = TcpStream::connect(addr).await?;
            s.set_nodelay(true)?;
            SpiceStream::Plain(s)
        }
    };
    let mut opts = LinkOptions::new(channel_type);
    opts.connection_id = session_id;
    opts.channel_id = channel_id;
    opts.password = password;
    opts.channel_caps = channel_caps;
    link_client(stream, opts).await
}

/// Helper: does the advertised list include a channel of `ty`?
pub fn has_channel(channels: &[ChannelId], ty: ChannelType) -> bool {
    channels.iter().any(|c| c.channel_type == ty as u8)
}


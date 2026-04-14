//! `SpiceClient` — top-level connection object used by the embedder.

use capsaicin_net::{Channel, MainConnection, SpiceStream, TlsConfig, connect_sub_channel};
use capsaicin_proto::caps::CapSet;
use capsaicin_proto::common;
use capsaicin_proto::enums::{ChannelType, main_msg, msg as common_msg, msgc as common_msgc};
use capsaicin_proto::main_chan;
use capsaicin_proto::types::Writer;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::{ClientError, Result};
use crate::events::{ClientEvent, InputEvent, MouseMode};
use crate::{cursor, display, inputs};

/// Size of the internal event / input mailboxes. Generous enough that a
/// UI thread will not stall a GPU-backed stream of draws, small enough
/// that backpressure surfaces if the embedder falls behind.
const CHANNEL_CAPACITY: usize = 256;

/// Configuration knobs for `connect`.
pub struct SpiceClientBuilder {
    attach_display: bool,
    attach_inputs: bool,
    attach_cursor: bool,
    event_capacity: usize,
    input_capacity: usize,
}

impl Default for SpiceClientBuilder {
    fn default() -> Self {
        Self {
            attach_display: true,
            attach_inputs: true,
            attach_cursor: true,
            event_capacity: CHANNEL_CAPACITY,
            input_capacity: CHANNEL_CAPACITY,
        }
    }
}

impl SpiceClientBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open the display sub-channel. Defaults to true.
    pub fn display(mut self, v: bool) -> Self {
        self.attach_display = v;
        self
    }

    /// Open the inputs sub-channel. Defaults to true.
    pub fn inputs(mut self, v: bool) -> Self {
        self.attach_inputs = v;
        self
    }

    /// Open the cursor sub-channel. Defaults to true. In SERVER mouse
    /// mode the guest does not paint the cursor into the framebuffer,
    /// so turning this off means the user sees no cursor at all.
    pub fn cursor(mut self, v: bool) -> Self {
        self.attach_cursor = v;
        self
    }

    pub fn event_capacity(mut self, n: usize) -> Self {
        self.event_capacity = n.max(1);
        self
    }

    pub fn input_capacity(mut self, n: usize) -> Self {
        self.input_capacity = n.max(1);
        self
    }

    pub async fn connect(self, addr: &str, password: &str) -> Result<SpiceClient> {
        SpiceClient::connect_with(self, addr, password, None).await
    }

    pub async fn connect_tls(
        self,
        addr: &str,
        password: &str,
        tls: TlsConfig,
    ) -> Result<SpiceClient> {
        SpiceClient::connect_with(self, addr, password, Some(tls)).await
    }
}

/// Event-driven SPICE client.
///
/// On drop, all background tasks are aborted and open TCP connections
/// closed. Call [`SpiceClient::close`] for a graceful wait.
pub struct SpiceClient {
    events_rx: mpsc::Receiver<ClientEvent>,
    input_tx: Option<mpsc::Sender<InputEvent>>,
    tasks: Vec<JoinHandle<()>>,
    session_id: u32,
}

impl SpiceClient {
    /// Shortcut for the default builder.
    pub async fn connect(addr: &str, password: &str) -> Result<Self> {
        SpiceClientBuilder::new().connect(addr, password).await
    }

    /// Shortcut for the default builder with TLS.
    pub async fn connect_tls(addr: &str, password: &str, tls: TlsConfig) -> Result<Self> {
        SpiceClientBuilder::new()
            .connect_tls(addr, password, tls)
            .await
    }

    pub fn builder() -> SpiceClientBuilder {
        SpiceClientBuilder::new()
    }

    async fn connect_with(
        cfg: SpiceClientBuilder,
        addr: &str,
        password: &str,
        tls: Option<TlsConfig>,
    ) -> Result<Self> {
        // 1. Main channel + bootstrap.
        let main = match tls.clone() {
            Some(cfg) => MainConnection::connect_tls(addr, password, cfg).await?,
            None => MainConnection::connect(addr, password).await?,
        };
        let session_id = main.session_id;
        let advertised: Vec<u8> = main
            .available_channels
            .iter()
            .map(|c| c.channel_type)
            .collect();

        let has_display = advertised.contains(&(ChannelType::Display as u8));
        let has_inputs = advertised.contains(&(ChannelType::Inputs as u8));
        let has_cursor = advertised.contains(&(ChannelType::Cursor as u8));

        if cfg.attach_display && !has_display {
            return Err(ClientError::MissingChannel("display"));
        }
        if cfg.attach_inputs && !has_inputs {
            return Err(ClientError::MissingChannel("inputs"));
        }
        // Cursor is soft-required: in CLIENT mouse mode the local OS
        // paints the cursor, so its absence is only a problem in SERVER
        // mode. Log and carry on rather than refuse to connect.
        if cfg.attach_cursor && !has_cursor {
            tracing::warn!(
                "server did not advertise cursor channel — cursor will be invisible in SERVER mouse mode"
            );
        }

        // 2. Open sub-channels eagerly (keep the main session alive).
        let display_channel = if cfg.attach_display {
            let mut ch = connect_sub_channel(
                addr,
                session_id,
                ChannelType::Display,
                0,
                password,
                CapSet::new(),
                tls.clone(),
            )
            .await?;
            display::send_init(&mut ch).await?;
            Some(ch)
        } else {
            None
        };

        let inputs_channel = if cfg.attach_inputs {
            Some(
                connect_sub_channel(
                    addr,
                    session_id,
                    ChannelType::Inputs,
                    0,
                    password,
                    CapSet::new(),
                    tls.clone(),
                )
                .await?,
            )
        } else {
            None
        };

        let cursor_channel = if cfg.attach_cursor && has_cursor {
            Some(
                connect_sub_channel(
                    addr,
                    session_id,
                    ChannelType::Cursor,
                    0,
                    password,
                    CapSet::new(),
                    tls.clone(),
                )
                .await?,
            )
        } else {
            None
        };

        // 3. Mailboxes for the public API.
        let (events_tx, events_rx) = mpsc::channel(cfg.event_capacity);
        let (input_tx, input_rx) = mpsc::channel(cfg.input_capacity);

        // Seed the event stream with the mouse mode reported in Init,
        // so the embedder can pick its input strategy before the first
        // MOUSE_MODE message (which the server only sends on change).
        let _ = events_tx
            .send(ClientEvent::MouseMode(MouseMode::from_raw(
                main.init.current_mouse_mode,
            )))
            .await;

        // 4. Spawn per-channel tasks.
        let mut tasks = Vec::new();

        // Main channel task: drain + reply to common control.
        let main_events = events_tx.clone();
        tasks.push(tokio::spawn(run_main(main.channel, main_events)));

        if let Some(ch) = display_channel {
            let tx = events_tx.clone();
            tasks.push(tokio::spawn(display::run(ch, tx)));
        }

        if let Some(ch) = cursor_channel {
            let tx = events_tx.clone();
            tasks.push(tokio::spawn(cursor::run(ch, tx)));
        }

        let input_tx_ret = if let Some(ch) = inputs_channel {
            tasks.push(tokio::spawn(inputs::run(ch, input_rx)));
            Some(input_tx)
        } else {
            drop(input_tx);
            None
        };

        Ok(Self {
            events_rx,
            input_tx: input_tx_ret,
            tasks,
            session_id,
        })
    }

    /// Session id allocated by the server on the main channel. Useful
    /// for logging and, in future, for reattaching dropped channels.
    pub fn session_id(&self) -> u32 {
        self.session_id
    }

    /// Pull the next [`ClientEvent`]. Returns `None` after the client has
    /// been fully shut down.
    pub async fn next_event(&mut self) -> Option<ClientEvent> {
        self.events_rx.recv().await
    }

    /// Enqueue an input event for delivery to the guest. Fails if the
    /// inputs channel was disabled at connect time or the client is
    /// closed.
    pub async fn send_input(&self, event: InputEvent) -> Result<()> {
        let Some(tx) = self.input_tx.as_ref() else {
            return Err(ClientError::MissingChannel("inputs"));
        };
        tx.send(event).await.map_err(|_| ClientError::Closed)
    }

    /// Stop all background tasks and wait for them to finish. Returns
    /// immediately if the client was already dropped.
    pub async fn close(mut self) {
        // Drop the input sender so the inputs task unparks and exits.
        self.input_tx = None;
        // Abort any task that's still blocked on a socket read.
        for h in &self.tasks {
            h.abort();
        }
        for h in self.tasks.drain(..) {
            let _ = h.await;
        }
    }
}

impl Drop for SpiceClient {
    fn drop(&mut self) {
        for h in &self.tasks {
            h.abort();
        }
    }
}

/// Long-running task for the main channel: answer pings / acks, swallow
/// notifies. Emits `ClientEvent::Closed` when the socket drops.
async fn run_main(mut channel: Channel<SpiceStream>, events_tx: mpsc::Sender<ClientEvent>) {
    let mut ack_window: u32 = 0;
    let mut ack_remaining: u32 = 0;
    loop {
        let msg = match channel.read_message().await {
            Ok(m) => m,
            Err(e) => {
                let _ = events_tx
                    .send(ClientEvent::Closed(Some(ClientError::Net(e))))
                    .await;
                return;
            }
        };
        match msg.msg_type {
            common_msg::PING => {
                let Ok(ping) = common::Ping::decode(&msg.body) else {
                    continue;
                };
                let mut w = Writer::new();
                common::Pong::from_ping(&ping).encode(&mut w);
                if let Err(e) = channel
                    .write_message(common_msgc::PONG, w.as_slice())
                    .await
                {
                    let _ = events_tx
                        .send(ClientEvent::Closed(Some(ClientError::Net(e))))
                        .await;
                    return;
                }
            }
            common_msg::SET_ACK => {
                let Ok(ack) = common::SetAck::decode(&msg.body) else {
                    continue;
                };
                let mut w = Writer::new();
                common::AckSync {
                    generation: ack.generation,
                }
                .encode(&mut w);
                if let Err(e) = channel
                    .write_message(common_msgc::ACK_SYNC, w.as_slice())
                    .await
                {
                    let _ = events_tx
                        .send(ClientEvent::Closed(Some(ClientError::Net(e))))
                        .await;
                    return;
                }
                ack_window = ack.window;
                ack_remaining = ack.window;
            }
            main_msg::MOUSE_MODE => {
                let Ok(mm) = main_chan::MouseMode::decode(&msg.body) else {
                    continue;
                };
                let _ = events_tx
                    .send(ClientEvent::MouseMode(MouseMode::from_raw(mm.current_mode)))
                    .await;
            }
            _ => {
                tracing::trace!(msg_type = msg.msg_type, "main: ignored");
            }
        }
        // SPICE flow control: send MSGC_ACK every `window` non-control msgs.
        if ack_window > 0
            && !matches!(
                msg.msg_type,
                common_msg::SET_ACK | common_msg::PING | common_msg::MIGRATE | common_msg::MIGRATE_DATA
            )
        {
            ack_remaining = ack_remaining.saturating_sub(1);
            if ack_remaining == 0 {
                ack_remaining = ack_window;
                if let Err(e) = channel.write_message(common_msgc::ACK, &[]).await {
                    let _ = events_tx
                        .send(ClientEvent::Closed(Some(ClientError::Net(e))))
                        .await;
                    return;
                }
            }
        }
    }
}

//! Inputs-channel task: receives [`InputEvent`]s from the embedder and
//! forwards them to the server, and drains server-sent control messages.

use capsaicin_net::{Channel, SpiceStream};
use capsaicin_proto::common;
use capsaicin_proto::enums::{msg as common_msg, msgc as common_msgc};
use capsaicin_proto::inputs::{
    self, KeyCode, KeyModifiers, MouseButton, MouseMotion, MousePosition, client_msg,
};
use capsaicin_proto::types::Writer;
use tokio::sync::mpsc;

use crate::events::InputEvent;

/// Run the inputs channel: multiplex outbound InputEvent sends with
/// inbound server messages.
pub(crate) async fn run(
    mut channel: Channel<SpiceStream>,
    mut input_rx: mpsc::Receiver<InputEvent>,
) {
    loop {
        tokio::select! {
            maybe_evt = input_rx.recv() => {
                let Some(evt) = maybe_evt else {
                    // embedder dropped the client
                    return;
                };
                if let Err(e) = send_event(&mut channel, evt).await {
                    tracing::debug!(%e, "inputs: send failed; task exiting");
                    return;
                }
            }
            read = channel.read_message() => {
                let msg = match read {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::debug!(%e, "inputs: channel closed");
                        return;
                    }
                };
                if let Err(e) = handle_server_msg(&mut channel, msg).await {
                    tracing::debug!(%e, "inputs: handle failed; task exiting");
                    return;
                }
            }
        }
    }
}

async fn send_event(
    channel: &mut Channel<SpiceStream>,
    evt: InputEvent,
) -> capsaicin_net::Result<()> {
    let (msg_type, body) = match evt {
        InputEvent::KeyDown(code) => {
            let mut w = Writer::new();
            KeyCode { code }.encode(&mut w);
            (client_msg::INPUTS_KEY_DOWN, w.into_vec())
        }
        InputEvent::KeyUp(code) => {
            let mut w = Writer::new();
            KeyCode { code }.encode(&mut w);
            (client_msg::INPUTS_KEY_UP, w.into_vec())
        }
        InputEvent::KeyModifiers(mods) => {
            let mut w = Writer::new();
            KeyModifiers {
                keyboard_modifiers: mods,
            }
            .encode(&mut w);
            (client_msg::INPUTS_KEY_MODIFIERS, w.into_vec())
        }
        InputEvent::MouseMotion { dx, dy, buttons } => {
            let mut w = Writer::new();
            MouseMotion {
                dx,
                dy,
                buttons_state: buttons,
            }
            .encode(&mut w);
            (client_msg::INPUTS_MOUSE_MOTION, w.into_vec())
        }
        InputEvent::MousePosition {
            x,
            y,
            buttons,
            display,
        } => {
            let mut w = Writer::new();
            MousePosition {
                x,
                y,
                buttons_state: buttons,
                display_id: display,
            }
            .encode(&mut w);
            (client_msg::INPUTS_MOUSE_POSITION, w.into_vec())
        }
        InputEvent::MousePress { button, buttons } => {
            let mut w = Writer::new();
            MouseButton {
                button,
                buttons_state: buttons,
            }
            .encode(&mut w);
            (client_msg::INPUTS_MOUSE_PRESS, w.into_vec())
        }
        InputEvent::MouseRelease { button, buttons } => {
            let mut w = Writer::new();
            MouseButton {
                button,
                buttons_state: buttons,
            }
            .encode(&mut w);
            (client_msg::INPUTS_MOUSE_RELEASE, w.into_vec())
        }
    };
    channel.write_message(msg_type, &body).await
}

async fn handle_server_msg(
    channel: &mut Channel<SpiceStream>,
    msg: capsaicin_net::Message,
) -> capsaicin_net::Result<()> {
    match msg.msg_type {
        common_msg::PING => {
            let ping = common::Ping::decode(&msg.body)?;
            let mut w = Writer::new();
            common::Pong::from_ping(&ping).encode(&mut w);
            channel
                .write_message(common_msgc::PONG, w.as_slice())
                .await?;
        }
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
        inputs::server_msg::INPUTS_INIT
        | inputs::server_msg::INPUTS_KEY_MODIFIERS
        | inputs::server_msg::INPUTS_MOUSE_MOTION_ACK => {
            // Status messages; not yet surfaced to the embedder.
        }
        _ => {
            tracing::debug!(msg_type = msg.msg_type, "inputs: unhandled");
        }
    }
    Ok(())
}

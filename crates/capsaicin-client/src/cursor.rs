//! Cursor-channel task: parse cursor sprite / position / visibility
//! updates and forward them to the embedder as `CursorEvent`s.

use capsaicin_net::{Channel, SpiceStream};
use capsaicin_proto::common;
use capsaicin_proto::cursor::{
    Cursor, CursorInit, CursorInvalOne, CursorMove, CursorSet, cursor_flag, cursor_msg,
    cursor_type,
};
use capsaicin_proto::enums::{msg as common_msg, msgc as common_msgc};
use capsaicin_proto::types::Writer;
use tokio::sync::mpsc;

use crate::events::{ClientEvent, CursorEvent};

pub(crate) async fn run(
    mut channel: Channel<SpiceStream>,
    events_tx: mpsc::Sender<ClientEvent>,
) {
    loop {
        let msg = match channel.read_message().await {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(%e, "cursor: channel closed");
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
                if channel
                    .write_message(common_msgc::PONG, w.as_slice())
                    .await
                    .is_err()
                {
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
                if channel
                    .write_message(common_msgc::ACK_SYNC, w.as_slice())
                    .await
                    .is_err()
                {
                    return;
                }
            }
            cursor_msg::INIT => {
                let Ok(init) = CursorInit::decode(&msg.body) else {
                    continue;
                };
                if let Some(evt) = cursor_to_event(
                    init.position_x,
                    init.position_y,
                    init.visible != 0,
                    &init.cursor,
                ) {
                    if events_tx.send(ClientEvent::Cursor(evt)).await.is_err() {
                        return;
                    }
                }
            }
            cursor_msg::SET => {
                let Ok(set) = CursorSet::decode(&msg.body) else {
                    continue;
                };
                if let Some(evt) = cursor_to_event(
                    set.position_x,
                    set.position_y,
                    set.visible != 0,
                    &set.cursor,
                ) {
                    if events_tx.send(ClientEvent::Cursor(evt)).await.is_err() {
                        return;
                    }
                }
            }
            cursor_msg::MOVE => {
                let Ok(mv) = CursorMove::decode(&msg.body) else {
                    continue;
                };
                if events_tx
                    .send(ClientEvent::Cursor(CursorEvent::Move {
                        x: mv.position_x as i32,
                        y: mv.position_y as i32,
                    }))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            cursor_msg::HIDE => {
                if events_tx
                    .send(ClientEvent::Cursor(CursorEvent::Hide))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            cursor_msg::INVAL_ONE => {
                let Ok(inv) = CursorInvalOne::decode(&msg.body) else {
                    continue;
                };
                if events_tx
                    .send(ClientEvent::Cursor(CursorEvent::InvalidateOne {
                        unique: inv.unique,
                    }))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            cursor_msg::INVAL_ALL => {
                if events_tx
                    .send(ClientEvent::Cursor(CursorEvent::InvalidateAll))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            cursor_msg::RESET | cursor_msg::TRAIL => {
                // Trail is a legacy decoration; reset means "forget
                // current state" — caller can infer from Hide +
                // InvalidateAll, but we don't need a dedicated event.
            }
            _ => {
                tracing::trace!(msg_type = msg.msg_type, "cursor: ignored");
            }
        }
    }
}

/// Shared Init/Set → event mapping. Decodes ALPHA cursors, returns
/// `SetFromCache` for cache-ref messages, skips other types (MONO,
/// palette) with a debug log since they're exceedingly rare on modern
/// guests.
fn cursor_to_event(x: i16, y: i16, visible: bool, cursor: &Cursor) -> Option<CursorEvent> {
    let x = x as i32;
    let y = y as i32;
    let from_cache = cursor.flags
        & (cursor_flag::FROM_CACHE | cursor_flag::FROM_CACHE_LOSSLESS)
        != 0;
    if from_cache {
        return Some(CursorEvent::SetFromCache {
            x,
            y,
            unique: cursor.header.unique,
            visible,
        });
    }
    let cacheable = cursor.flags & cursor_flag::CACHE_ME != 0;
    let pixels = match cursor.header.kind {
        cursor_type::ALPHA => {
            // Expect width * height * 4 ARGB bytes. Truncate / pad so
            // the embedder gets a correctly-sized buffer regardless of
            // any wire oddities.
            let expected = (cursor.header.width as usize) * (cursor.header.height as usize) * 4;
            if cursor.data.len() < expected {
                tracing::debug!(
                    have = cursor.data.len(),
                    want = expected,
                    "cursor: alpha data shorter than expected"
                );
                return None;
            }
            cursor.data[..expected].to_vec()
        }
        other => {
            tracing::debug!(kind = other, "cursor: unsupported type, reporting empty sprite");
            Vec::new()
        }
    };
    Some(CursorEvent::Set {
        x,
        y,
        hot_x: cursor.header.hot_spot_x,
        hot_y: cursor.header.hot_spot_y,
        width: cursor.header.width,
        height: cursor.header.height,
        pixels,
        unique: cursor.header.unique,
        cacheable,
        visible,
    })
}

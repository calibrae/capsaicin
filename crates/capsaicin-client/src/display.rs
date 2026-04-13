//! Display-channel task: decode messages into [`DisplayEvent`].

use std::collections::HashMap;

use capsaicin_net::{Channel, Message};
use capsaicin_proto::common;
use capsaicin_proto::display::{
    DisplayInit, Mode, MonitorsConfig, SurfaceCreate, SurfaceDestroy,
    msg_type as display_msg, surface_flags,
};
use capsaicin_proto::draw::{CopyBits, DrawCopy, DrawFill};
use capsaicin_proto::enums::{msg as common_msg, msgc as common_msgc};
use capsaicin_proto::image::{
    Bitmap, ImageDescriptor, bitmap_bytes_per_pixel, image_type, read_chunks,
};
use capsaicin_lz::{LZ_HEADER_SIZE, LzHeader, LzImageType, decompress_rgb32, decompress_rgba};
use capsaicin_glz::{
    GLZ_HEADER_SIZE, GlzError, GlzHeader, GlzWindow, decompress_rgb32 as glz_decompress_rgb32,
};
use capsaicin_quic::{
    QuicHeader, QuicImageType, decompress_gray as quic_decompress_gray,
    decompress_rgb16 as quic_decompress_rgb16, decompress_rgb32 as quic_decompress_rgb32,
    decompress_rgba as quic_decompress_rgba,
};
use capsaicin_proto::stream::{
    StreamCreate, StreamData, StreamDataSized, StreamDestroy, VideoCodec,
};
use capsaicin_proto::types::Rect as ProtoRect;

use crate::mjpeg;
use capsaicin_proto::types::{Reader, Writer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::events::{ClientEvent, DisplayEvent, RegionPixels, SurfaceFormat};

/// Seed the display channel with `MSGC_DISPLAY_INIT` so the server starts
/// streaming surface creates and draws.
///
/// `glz_dictionary_id` and `pixmap_cache_id` are intentionally unique
/// per connection (timestamp-ish): the server uses them as keys for its
/// dictionary-sharing optimisation, and giving it a never-seen id
/// forces it to treat us as a brand-new client and (re)send everything
/// rather than assuming we hold prior dictionary state.
pub(crate) async fn send_init<S: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut Channel<S>,
) -> capsaicin_net::Result<()> {
    let salt = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_nanos() as u8).wrapping_add(1))
        .unwrap_or(1);
    let init = DisplayInit {
        pixmap_cache_id: salt,
        // 128 MiB — matches spice-gtk defaults. We don't honour the cache
        // yet, but advertising a non-zero value stops some servers from
        // bailing early.
        pixmap_cache_size: 128 * 1024 * 1024,
        glz_dictionary_id: salt,
        glz_dictionary_window_size: 16 * 1024 * 1024,
    };
    let mut w = Writer::new();
    init.encode(&mut w);
    channel
        .write_message(display_msg::INIT, w.as_slice())
        .await
}

/// Per-display state we must keep across messages: the primary-surface
/// table is needed so `DRAW_*` commands can be paired with the pixel
/// format the server picked at `SURFACE_CREATE` time. Streams are kept
/// here so `STREAM_DATA` can find its codec and `dest`. The GLZ
/// dictionary lives here too so cross-image back-references can resolve.
/// `ack` tracks the SPICE flow-control window: the server stops sending
/// after `window` messages until we send `MSGC_ACK`.
struct DisplayState {
    surfaces: HashMap<u32, SurfaceFormat>,
    streams: HashMap<u32, StreamInfo>,
    glz_window: GlzWindow,
    ack: AckState,
}

impl Default for DisplayState {
    fn default() -> Self {
        Self {
            surfaces: HashMap::new(),
            streams: HashMap::new(),
            glz_window: GlzWindow::default(),
            ack: AckState::default(),
        }
    }
}

/// SPICE per-channel flow control. Server emits `SET_ACK { generation,
/// window }`. Client must reply with one `ACK_SYNC` immediately, then
/// send `MSGC_ACK` after every `window` non-ack messages it processes.
#[derive(Default)]
struct AckState {
    /// 0 means no SET_ACK seen yet (no ACK needed).
    window: u32,
    /// Decrements with each processed message; when it hits 0 we send
    /// MSGC_ACK and reset to `window`.
    remaining: u32,
}

impl AckState {
    fn on_set_ack(&mut self, window: u32) {
        self.window = window;
        self.remaining = window;
    }

    /// Returns true when the window has been exhausted and the caller
    /// should send `MSGC_ACK`.
    fn note_message(&mut self) -> bool {
        if self.window == 0 {
            return false;
        }
        if self.remaining == 0 {
            self.remaining = self.window;
        }
        self.remaining = self.remaining.saturating_sub(1);
        if self.remaining == 0 {
            self.remaining = self.window;
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct StreamInfo {
    codec: VideoCodec,
    dest: ProtoRect,
}

/// Long-running task: pull messages, translate to events.
pub(crate) async fn run(
    mut channel: Channel<TcpStream>,
    events_tx: mpsc::Sender<ClientEvent>,
) {
    let mut state = DisplayState::default();
    loop {
        let msg = match channel.read_message().await {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(%e, "display: channel closed");
                return;
            }
        };
        if let Err(e) = handle(&mut channel, &mut state, msg, &events_tx).await {
            tracing::debug!(%e, "display: event handler error; task exiting");
            return;
        }
        if events_tx.is_closed() {
            return;
        }
    }
}

async fn handle(
    channel: &mut Channel<TcpStream>,
    state: &mut DisplayState,
    msg: Message,
    events_tx: &mpsc::Sender<ClientEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
            tracing::debug!(generation = ack.generation, window = ack.window, "display: SET_ACK");
            let mut w = Writer::new();
            common::AckSync {
                generation: ack.generation,
            }
            .encode(&mut w);
            channel
                .write_message(common_msgc::ACK_SYNC, w.as_slice())
                .await?;
            state.ack.on_set_ack(ack.window);
        }
        common_msg::NOTIFY | common_msg::MIGRATE | common_msg::MIGRATE_DATA => {}
        display_msg::MODE => {
            let m = Mode::decode(&msg.body)?;
            let _ = events_tx
                .send(ClientEvent::Display(DisplayEvent::Mode {
                    width: m.x_res,
                    height: m.y_res,
                    bits: m.bits,
                }))
                .await;
        }
        display_msg::MARK => {
            let _ = events_tx
                .send(ClientEvent::Display(DisplayEvent::Mark))
                .await;
        }
        display_msg::RESET => {
            state.surfaces.clear();
            let _ = events_tx
                .send(ClientEvent::Display(DisplayEvent::Reset))
                .await;
        }
        display_msg::SURFACE_CREATE => {
            let s = SurfaceCreate::decode(&msg.body)?;
            let fmt = SurfaceFormat::from_raw(s.format);
            state.surfaces.insert(s.surface_id, fmt);
            let _ = events_tx
                .send(ClientEvent::Display(DisplayEvent::SurfaceCreated {
                    id: s.surface_id,
                    width: s.width,
                    height: s.height,
                    format: fmt,
                    primary: s.flags & surface_flags::PRIMARY != 0,
                }))
                .await;
        }
        display_msg::SURFACE_DESTROY => {
            let s = SurfaceDestroy::decode(&msg.body)?;
            state.surfaces.remove(&s.surface_id);
            let _ = events_tx
                .send(ClientEvent::Display(DisplayEvent::SurfaceDestroyed {
                    id: s.surface_id,
                }))
                .await;
        }
        display_msg::MONITORS_CONFIG => {
            let m = MonitorsConfig::decode(&msg.body)?;
            let _ = events_tx
                .send(ClientEvent::Display(DisplayEvent::MonitorsConfig {
                    max_allowed: m.max_allowed,
                    heads: m.heads,
                }))
                .await;
        }
        display_msg::DRAW_FILL => {
            let evt = decode_draw_fill(state, &msg);
            let _ = events_tx.send(ClientEvent::Display(evt)).await;
        }
        display_msg::DRAW_COPY => {
            let evt = decode_draw_copy(state, &msg);
            let _ = events_tx.send(ClientEvent::Display(evt)).await;
        }
        display_msg::COPY_BITS => {
            if let Ok(cb) = CopyBits::decode(&msg.body) {
                let _ = events_tx
                    .send(ClientEvent::Display(DisplayEvent::CopyRect {
                        surface_id: cb.base.surface_id,
                        src_x: cb.src_pos.x,
                        src_y: cb.src_pos.y,
                        dest_rect: cb.base.bounds,
                    }))
                    .await;
            } else {
                tracing::debug!("copy_bits: decode failed");
            }
        }
        display_msg::STREAM_CREATE => {
            if let Ok(s) = StreamCreate::decode(&msg.body) {
                state.streams.insert(
                    s.stream_id,
                    StreamInfo {
                        codec: s.codec,
                        dest: s.dest,
                    },
                );
                let _ = events_tx
                    .send(ClientEvent::Display(DisplayEvent::StreamCreated {
                        stream_id: s.stream_id,
                        surface_id: s.surface_id,
                        codec: s.codec,
                        dest: s.dest,
                        src_width: s.src_width,
                        src_height: s.src_height,
                    }))
                    .await;
            }
        }
        display_msg::STREAM_DATA => {
            if let Ok(d) = StreamData::decode(&msg.body) {
                emit_stream_frame(
                    state,
                    d.header.stream_id,
                    d.header.multi_media_time,
                    None,
                    &d.data,
                    events_tx,
                )
                .await;
            }
        }
        display_msg::STREAM_DATA_SIZED => {
            if let Ok(d) = StreamDataSized::decode(&msg.body) {
                emit_stream_frame(
                    state,
                    d.header.stream_id,
                    d.header.multi_media_time,
                    Some(d.dest),
                    &d.data,
                    events_tx,
                )
                .await;
            }
        }
        display_msg::STREAM_DESTROY => {
            if let Ok(d) = StreamDestroy::decode(&msg.body) {
                state.streams.remove(&d.stream_id);
                let _ = events_tx
                    .send(ClientEvent::Display(DisplayEvent::StreamDestroyed {
                        stream_id: d.stream_id,
                    }))
                    .await;
            }
        }
        display_msg::STREAM_DESTROY_ALL => {
            let ids: Vec<u32> = state.streams.keys().copied().collect();
            state.streams.clear();
            for stream_id in ids {
                let _ = events_tx
                    .send(ClientEvent::Display(DisplayEvent::StreamDestroyed {
                        stream_id,
                    }))
                    .await;
            }
        }
        other => {
            let _ = events_tx
                .send(ClientEvent::Display(DisplayEvent::UnhandledDraw {
                    msg_type: other,
                    size: msg.body.len(),
                }))
                .await;
        }
    }
    // Flow control: every `window` non-ack messages, send MSGC_ACK so
    // the server keeps the stream flowing. SET_ACK / PING / ACK_SYNC
    // themselves don't count toward the window.
    if !matches!(
        msg.msg_type,
        common_msg::SET_ACK | common_msg::PING | common_msg::MIGRATE | common_msg::MIGRATE_DATA
    ) && state.ack.note_message()
    {
        channel.write_message(common_msgc::ACK, &[]).await?;
    }
    Ok(())
}

fn decode_draw_copy(state: &mut DisplayState, msg: &Message) -> DisplayEvent {
    let copy = match DrawCopy::decode(&msg.body) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(%e, body_len = msg.body.len(), "draw_copy: header decode failed");
            return unhandled(display_msg::DRAW_COPY, msg.body.len());
        }
    };
    tracing::debug!(
        surface_id = copy.base.surface_id,
        bounds_w = copy.base.bounds.right - copy.base.bounds.left,
        bounds_h = copy.base.bounds.bottom - copy.base.bounds.top,
        clip = ?copy.base.clip,
        src_bitmap_offset = copy.src_bitmap_offset,
        rop = format!("{:#x}", copy.rop_descriptor),
        mask_bitmap = copy.mask.bitmap_offset,
        body_len = msg.body.len(),
        "draw_copy: parsed"
    );
    // Render the source image into the bounding rect even when the
    // server attaches a clip-rect list or a mask — we just paint the
    // whole bounds, which is "good enough" for most real desktops.
    // The is_simple_copy predicate is no longer used as a gate.

    // Follow src_bitmap_offset into the message body.
    let img_start = copy.src_bitmap_offset as usize;
    if img_start == 0 || img_start + ImageDescriptor::SIZE > msg.body.len() {
        tracing::debug!(
            img_start,
            body_len = msg.body.len(),
            "draw_copy: src_bitmap_offset out of range"
        );
        return unhandled(display_msg::DRAW_COPY, msg.body.len());
    }
    let mut r = Reader::new(&msg.body[img_start..]);
    let desc = match ImageDescriptor::decode(&mut r) {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!(%e, "draw_copy: ImageDescriptor decode failed");
            return unhandled(display_msg::DRAW_COPY, msg.body.len());
        }
    };
    tracing::debug!(
        image_type = desc.image_type,
        image_w = desc.width,
        image_h = desc.height,
        "draw_copy: image descriptor"
    );

    if desc.image_type == image_type::LZ_RGB {
        return decode_lz_rgb(state, msg, &copy, &desc);
    }
    if desc.image_type == image_type::QUIC {
        return decode_quic(state, msg, &copy, &desc);
    }
    if desc.image_type == image_type::GLZ_RGB {
        return decode_glz(state, msg, &copy, &desc);
    }
    if desc.image_type != image_type::BITMAP {
        tracing::debug!(
            image_type = desc.image_type,
            "draw_copy: compressed / non-bitmap image, left as UnhandledDraw"
        );
        return unhandled(display_msg::DRAW_COPY, msg.body.len());
    }

    let Ok(bitmap) = Bitmap::decode(&mut r) else {
        return unhandled(display_msg::DRAW_COPY, msg.body.len());
    };

    // Reject paletted formats for now — they need a SpicePalette follow-up.
    if bitmap.palette_offset != 0 {
        return unhandled(display_msg::DRAW_COPY, msg.body.len());
    }
    let bpp = match bitmap_bytes_per_pixel(bitmap.format) {
        Some(bpp @ (2 | 3 | 4)) => bpp,
        _ => return unhandled(display_msg::DRAW_COPY, msg.body.len()),
    };

    let Ok(raw) = read_chunks(&msg.body, bitmap.data_offset) else {
        return unhandled(display_msg::DRAW_COPY, msg.body.len());
    };

    // Validate the buffer has enough data for the declared geometry.
    let expected = (bitmap.stride as usize).checked_mul(bitmap.height as usize);
    let Some(expected) = expected else {
        return unhandled(display_msg::DRAW_COPY, msg.body.len());
    };
    if raw.len() < expected {
        tracing::debug!(
            got = raw.len(),
            expected,
            stride = bitmap.stride,
            height = bitmap.height,
            "draw_copy: bitmap payload smaller than declared geometry"
        );
        return unhandled(display_msg::DRAW_COPY, msg.body.len());
    }

    // If the bitmap is bottom-up (!TOP_DOWN), flip rows so the embedder
    // can blit into a top-down framebuffer without knowing about SPICE
    // conventions.
    let pixels = if bitmap.is_top_down() {
        raw[..expected].to_vec()
    } else {
        flip_vertical(&raw[..expected], bitmap.stride as usize, bitmap.height as usize)
    };

    let _ = bpp; // validated above; stride carries the row layout for the embedder.

    let surface_format = *state
        .surfaces
        .get(&copy.base.surface_id)
        .unwrap_or(&SurfaceFormat::Unknown(0));

    state.glz_window.insert(desc.id, pixels.clone(), 4);
    DisplayEvent::Region {
        surface_id: copy.base.surface_id,
        rect: copy.base.bounds,
        pixels: RegionPixels::Raw {
            data: pixels,
            stride: bitmap.stride,
        },
        surface_format,
    }
}

async fn emit_stream_frame(
    state: &DisplayState,
    stream_id: u32,
    multi_media_time: u32,
    sized_dest: Option<ProtoRect>,
    payload: &[u8],
    events_tx: &tokio::sync::mpsc::Sender<ClientEvent>,
) {
    let Some(info) = state.streams.get(&stream_id).copied() else {
        tracing::debug!(stream_id, "stream_data for unknown stream");
        return;
    };
    let dest_rect = sized_dest.unwrap_or(info.dest);
    match info.codec {
        VideoCodec::Mjpeg => match mjpeg::decode(payload) {
            Ok(frame) => {
                let stride = (frame.width as u32) * 4;
                let _ = events_tx
                    .send(ClientEvent::Display(DisplayEvent::StreamFrame {
                        stream_id,
                        multi_media_time,
                        dest_rect,
                        pixels: RegionPixels::Raw {
                            data: frame.bgra,
                            stride,
                        },
                    }))
                    .await;
            }
            Err(e) => tracing::debug!(stream_id, %e, "mjpeg decode failed"),
        },
        other => {
            tracing::debug!(
                stream_id,
                ?other,
                payload_size = payload.len(),
                "stream_data for codec we don't decode yet"
            );
        }
    }
}

/// GLZ_RGB handler: parse the GLZ header, run the cross-image-aware
/// decoder against the session dictionary, and store the decoded image
/// in the dictionary so subsequent images can reference it.
fn decode_glz(
    state: &mut DisplayState,
    msg: &Message,
    copy: &capsaicin_proto::draw::DrawCopy,
    _desc: &ImageDescriptor,
) -> DisplayEvent {
    let header_start = copy.src_bitmap_offset as usize + ImageDescriptor::SIZE;
    if header_start + 4 > msg.body.len() {
        return unhandled(display_msg::DRAW_COPY, msg.body.len());
    }
    let data_size = u32::from_le_bytes(
        msg.body[header_start..header_start + 4]
            .try_into()
            .unwrap(),
    ) as usize;
    let data_start = header_start + 4;
    let data_end = data_start + data_size;
    if data_end > msg.body.len() {
        tracing::debug!(
            data_start,
            data_size,
            body_len = msg.body.len(),
            "glz: declared data_size overruns message body"
        );
        return unhandled(display_msg::DRAW_COPY, msg.body.len());
    }
    let compressed = &msg.body[data_start..data_end];
    if compressed.len() < GLZ_HEADER_SIZE {
        return unhandled(display_msg::DRAW_COPY, msg.body.len());
    }
    let hdr = match GlzHeader::decode(compressed) {
        Ok(h) => h,
        Err(e) => {
            tracing::debug!(%e, "glz: header decode failed");
            return unhandled(display_msg::DRAW_COPY, msg.body.len());
        }
    };
    if hdr.image_type != capsaicin_lz::LzImageType::Rgb32 {
        tracing::debug!(
            ?hdr.image_type,
            w = hdr.width, h = hdr.height,
            "glz: image type not yet supported"
        );
        return unhandled(display_msg::DRAW_COPY, msg.body.len());
    }
    let result = glz_decompress_rgb32(compressed, &hdr, &state.glz_window);
    match result {
        Ok(pixels_top_down) => {
            // Always store top-down BGRA in the dictionary; flip to
            // bottom-up for the embedder if the header asks for it.
            state.glz_window.insert(hdr.id, pixels_top_down.clone(), 4);
            let pixels = if hdr.top_down {
                pixels_top_down
            } else {
                flip_vertical(
                    &pixels_top_down,
                    (hdr.width * 4) as usize,
                    hdr.height as usize,
                )
            };
            let surface_format = *state
                .surfaces
                .get(&copy.base.surface_id)
                .unwrap_or(&SurfaceFormat::Unknown(0));
            DisplayEvent::Region {
                surface_id: copy.base.surface_id,
                rect: copy.base.bounds,
                pixels: RegionPixels::Raw {
                    data: pixels,
                    stride: hdr.width * 4,
                },
                surface_format,
            }
        }
        Err(GlzError::CrossImage { dist }) => {
            tracing::debug!(
                image_dist = dist,
                w = hdr.width,
                h = hdr.height,
                id = hdr.id,
                win_size = state.glz_window.len(),
                "glz: cross-image target missing — inserting placeholder to keep chain alive"
            );
            // Store zero pixels so subsequent images can still reference
            // *this* one. Visually wrong colour but geometry stays
            // synchronised, preventing whole-screen cascade failures.
            // Use checked arithmetic and a cap to defend against a
            // malicious server triggering a multi-GiB placeholder.
            if let Some(placeholder) = bounded_placeholder(hdr.width, hdr.height) {
                state.glz_window.insert(hdr.id, placeholder, 4);
            }
            unhandled(display_msg::DRAW_COPY, msg.body.len())
        }
        Err(e) => {
            tracing::debug!(%e, "glz: body decode failed");
            // Same placeholder strategy for any other decode failure.
            if let Some(placeholder) = bounded_placeholder(hdr.width, hdr.height) {
                state.glz_window.insert(hdr.id, placeholder, 4);
            }
            unhandled(display_msg::DRAW_COPY, msg.body.len())
        }
    }
}

/// QUIC handler: parse the stream header, dispatch to body decoder.
///
/// `SpiceQUICData` on the wire is `u32 data_size` followed by `data_size`
/// bytes of QUIC payload **inline** (the C struct's `SpiceChunks *data`
/// pointer is `@nomarshal` — present only in the C-side runtime view).
fn decode_quic(
    state: &mut DisplayState,
    msg: &Message,
    copy: &capsaicin_proto::draw::DrawCopy,
    desc: &ImageDescriptor,
) -> DisplayEvent {
    let header_start = copy.src_bitmap_offset as usize + ImageDescriptor::SIZE;
    if header_start + 4 > msg.body.len() {
        tracing::debug!(header_start, body_len = msg.body.len(), "quic: body cut short");
        return unhandled(display_msg::DRAW_COPY, msg.body.len());
    }
    let data_size = u32::from_le_bytes(
        msg.body[header_start..header_start + 4]
            .try_into()
            .unwrap(),
    ) as usize;
    let data_start = header_start + 4;
    let data_end = data_start + data_size;
    if data_end > msg.body.len() {
        tracing::debug!(
            data_start,
            data_size,
            body_len = msg.body.len(),
            "quic: declared data_size overruns message body"
        );
        return unhandled(display_msg::DRAW_COPY, msg.body.len());
    }
    let compressed = &msg.body[data_start..data_end];
    tracing::debug!(compressed_len = compressed.len(), "quic: inline payload");
    let hdr = match QuicHeader::decode(&compressed) {
        Ok(h) => h,
        Err(e) => {
            tracing::debug!(%e, "quic: header decode failed");
            return unhandled(display_msg::DRAW_COPY, msg.body.len());
        }
    };

    // RGB32, RGB24 and RGBA all share the same per-channel decoder
    // (RGB24 just doesn't write a pad byte; we still emit BGRA for
    // embedder consistency, with alpha=0xFF for opaque RGB24).
    let result = match hdr.image_type {
        QuicImageType::Rgb32 => quic_decompress_rgb32(&compressed, hdr.width, hdr.height),
        QuicImageType::Rgb24 => quic_decompress_rgb32(&compressed, hdr.width, hdr.height).map(|mut p| {
            for px in p.chunks_exact_mut(4) {
                px[3] = 0xFF;
            }
            p
        }),
        QuicImageType::Rgba => quic_decompress_rgba(&compressed, hdr.width, hdr.height),
        QuicImageType::Rgb16 => quic_decompress_rgb16(&compressed, hdr.width, hdr.height),
        QuicImageType::Gray => quic_decompress_gray(&compressed, hdr.width, hdr.height),
    };
    let pixels = match result {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(%e, "quic: body decode failed");
            return unhandled(display_msg::DRAW_COPY, msg.body.len());
        }
    };

    let surface_format = *state
        .surfaces
        .get(&copy.base.surface_id)
        .unwrap_or(&SurfaceFormat::Unknown(0));

    let stride = hdr.width * 4;
    // Store this decoded image in the dictionary so subsequent GLZ
    // images can reference it cross-image.
    state.glz_window.insert(desc.id, pixels.clone(), 4);
    DisplayEvent::Region {
        surface_id: copy.base.surface_id,
        rect: copy.base.bounds,
        pixels: RegionPixels::Raw {
            data: pixels,
            stride,
        },
        surface_format,
    }
}

/// Decode `SpiceLZRGBData` (BinaryData: u32 data_size + `data_size`
/// inline bytes) and run the appropriate LZ decompressor.
fn decode_lz_rgb(
    state: &mut DisplayState,
    msg: &Message,
    copy: &capsaicin_proto::draw::DrawCopy,
    desc: &ImageDescriptor,
) -> DisplayEvent {
    // SpiceLZRGBData layout: u32 data_size + `data_size` bytes inline.
    let header_start = copy.src_bitmap_offset as usize + ImageDescriptor::SIZE;
    if header_start + 4 > msg.body.len() {
        return unhandled(display_msg::DRAW_COPY, msg.body.len());
    }
    let data_size = u32::from_le_bytes(
        msg.body[header_start..header_start + 4]
            .try_into()
            .unwrap(),
    ) as usize;
    let data_start = header_start + 4;
    let data_end = data_start + data_size;
    if data_end > msg.body.len() {
        tracing::debug!(
            data_start,
            data_size,
            body_len = msg.body.len(),
            "lz_rgb: declared data_size overruns message body"
        );
        return unhandled(display_msg::DRAW_COPY, msg.body.len());
    }
    let compressed = &msg.body[data_start..data_end];
    tracing::debug!(compressed_len = compressed.len(), "lz_rgb: inline payload");

    if compressed.len() < LZ_HEADER_SIZE {
        tracing::debug!("lz_rgb: payload smaller than header");
        return unhandled(display_msg::DRAW_COPY, msg.body.len());
    }
    let hdr = match LzHeader::decode(compressed) {
        Ok(h) => h,
        Err(e) => {
            tracing::debug!(%e, "lz_rgb: header decode failed");
            return unhandled(display_msg::DRAW_COPY, msg.body.len());
        }
    };
    tracing::debug!(
        ?hdr.image_type,
        w = hdr.width,
        h = hdr.height,
        stride = hdr.stride,
        top_down = hdr.top_down,
        "lz_rgb: header parsed"
    );

    if hdr.width != desc.width || hdr.height != desc.height {
        tracing::debug!(
            outer_w = desc.width,
            outer_h = desc.height,
            lz_w = hdr.width,
            lz_h = hdr.height,
            "lz_rgb: outer descriptor / inner header mismatch"
        );
    }

    let num_pixels = match (hdr.width as usize).checked_mul(hdr.height as usize) {
        Some(n) if n > 0 => n,
        _ => return unhandled(display_msg::DRAW_COPY, msg.body.len()),
    };

    let stream = &compressed[LZ_HEADER_SIZE..];
    let decoded = match hdr.image_type {
        LzImageType::Rgb32 => decompress_rgb32(stream, num_pixels),
        LzImageType::Rgba => decompress_rgba(stream, num_pixels),
        // RGB16 / RGB24 / paletted / alpha-only: deferred.
        other => {
            tracing::debug!(
                ?other,
                "lz_rgb: image format not yet supported, leaving as UnhandledDraw"
            );
            return unhandled(display_msg::DRAW_COPY, msg.body.len());
        }
    };
    let mut pixels = match decoded {
        Ok(p) => {
            tracing::debug!(out_len = p.len(), "lz_rgb: decoded ok");
            p
        }
        Err(e) => {
            tracing::debug!(%e, "lz_rgb: body decode failed");
            return unhandled(display_msg::DRAW_COPY, msg.body.len());
        }
    };

    // LZ output is always top-down internally. If the header says the
    // image is bottom-up, flip rows so the embedder sees top-down.
    let stride = (hdr.width * 4) as usize;
    if !hdr.top_down {
        pixels = flip_vertical(&pixels, stride, hdr.height as usize);
    }

    let surface_format = *state
        .surfaces
        .get(&copy.base.surface_id)
        .unwrap_or(&SurfaceFormat::Unknown(0));

    state.glz_window.insert(desc.id, pixels.clone(), 4);
    DisplayEvent::Region {
        surface_id: copy.base.surface_id,
        rect: copy.base.bounds,
        pixels: RegionPixels::Raw {
            data: pixels,
            stride: stride as u32,
        },
        surface_format,
    }
}

fn flip_vertical(rows: &[u8], stride: usize, height: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(rows.len());
    for y in (0..height).rev() {
        out.extend_from_slice(&rows[y * stride..(y + 1) * stride]);
    }
    out
}

/// Allocate a zero-byte placeholder for a GLZ image whose decode
/// failed, but **only** if the geometry is sane. Returns `None` if
/// the dimensions overflow or exceed our caps so a malicious server
/// can't trigger multi-GiB allocations through repeated decode
/// failures. 64 MiB cap matches the codec crates' MAX_IMAGE_BYTES.
fn bounded_placeholder(width: u32, height: u32) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || width > 16384 || height > 16384 {
        return None;
    }
    let bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))?;
    if bytes > 64 * 1024 * 1024 {
        return None;
    }
    Some(vec![0u8; bytes])
}

fn unhandled(msg_type: u16, size: usize) -> DisplayEvent {
    DisplayEvent::UnhandledDraw { msg_type, size }
}

fn decode_draw_fill(state: &DisplayState, msg: &Message) -> DisplayEvent {
    let Ok(fill) = DrawFill::decode(&msg.body) else {
        return DisplayEvent::UnhandledDraw {
            msg_type: display_msg::DRAW_FILL,
            size: msg.body.len(),
        };
    };
    // Only emit `Region` when the fill is expressible as a single solid
    // color. Anything requiring destination blending or clipping stays an
    // `UnhandledDraw` until the renderer layer learns those modes.
    if !fill.is_simple_solid() {
        return DisplayEvent::UnhandledDraw {
            msg_type: display_msg::DRAW_FILL,
            size: msg.body.len(),
        };
    }
    let Some(color) = fill.solid_color() else {
        return DisplayEvent::UnhandledDraw {
            msg_type: display_msg::DRAW_FILL,
            size: msg.body.len(),
        };
    };
    let surface_format = *state
        .surfaces
        .get(&fill.base.surface_id)
        .unwrap_or(&SurfaceFormat::Unknown(0));
    DisplayEvent::Region {
        surface_id: fill.base.surface_id,
        rect: fill.base.bounds,
        pixels: RegionPixels::SolidColor(color),
        surface_format,
    }
}

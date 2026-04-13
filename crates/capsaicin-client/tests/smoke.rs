//! End-to-end tests exercising the `SpiceClient` API against a real
//! `capsaicin-server` instance over loopback TCP.

use std::sync::Arc;
use std::time::Duration;

use capsaicin_client::{ClientEvent, DisplayEvent, InputEvent, RegionPixels, SpiceClient, SurfaceFormat};
use capsaicin_proto::display::{SurfaceCreate, msg_type as display_msg, surface_fmt, surface_flags};
use capsaicin_proto::draw::{
    Brush, Clip, DrawBase, DrawCopy, DrawFill, QMask, ropd, scale_mode,
};
use capsaicin_proto::image::{
    Bitmap, ImageDescriptor, bitmap_flags, bitmap_fmt, encode_single_chunk, image_type,
};
use capsaicin_glz::{
    GlzHeader, compress_rgb32_literal as glz_compress_rgb32_literal,
};
use capsaicin_lz::{LzHeader, LzImageType, compress_alpha_literal, compress_rgb32_literal};
use capsaicin_proto::stream::{
    StreamCreate, StreamData, StreamDataHeader, StreamDestroy, VideoCodec, stream_flags,
};
use capsaicin_proto::enums::{ChannelType, msg as common_msg};
use capsaicin_proto::inputs::{MousePosition, client_msg as inputs_client};
use capsaicin_proto::types::{Point, Rect, Writer};
use capsaicin_server::{Server, default_channels, serve_main_bootstrap};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::timeout;

async fn bind_loopback() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    (listener, addr)
}

/// Minimal server shell: accepts connections, runs main bootstrap, holds
/// sub-channels open and optionally injects test traffic into them.
struct TestServer {
    /// Injected into the display channel after attach.
    display_script: Vec<(u16, Vec<u8>)>,
    /// Captures one message received on the inputs channel.
    inputs_tx: Option<oneshot::Sender<(u16, Vec<u8>)>>,
}

async fn run_test_server(listener: TcpListener, server: Arc<Server>, ts: Arc<tokio::sync::Mutex<TestServer>>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let server = server.clone();
        let ts = ts.clone();
        tokio::spawn(async move {
            let Ok(accepted) = server.accept(stream).await else {
                return;
            };
            match accepted.channel_type {
                ChannelType::Main => {
                    let session_id = if accepted.connection_id == 0 {
                        server.new_session_id()
                    } else {
                        accepted.connection_id
                    };
                    let mut ch = accepted.channel;
                    let _ = serve_main_bootstrap(&mut ch, session_id, &default_channels(), None).await;
                    while ch.read_message().await.is_ok() {}
                }
                ChannelType::Display => {
                    let mut ch = accepted.channel;
                    // Drain client's MSGC_DISPLAY_INIT first.
                    let _ = ch.read_message().await;
                    // Replay the scripted messages.
                    let script = {
                        let guard = ts.lock().await;
                        guard.display_script.clone()
                    };
                    for (t, b) in script {
                        if ch.write_message(t, &b).await.is_err() {
                            break;
                        }
                    }
                    while ch.read_message().await.is_ok() {}
                }
                ChannelType::Inputs => {
                    let mut ch = accepted.channel;
                    // Capture exactly one inbound message from the client.
                    if let Ok(msg) = ch.read_message().await {
                        let tx = {
                            let mut guard = ts.lock().await;
                            guard.inputs_tx.take()
                        };
                        if let Some(tx) = tx {
                            let _ = tx.send((msg.msg_type, msg.body.to_vec()));
                        }
                    }
                    while ch.read_message().await.is_ok() {}
                }
                _ => {
                    let mut ch = accepted.channel;
                    while ch.read_message().await.is_ok() {}
                }
            }
        });
    }
}

fn build_surface_create() -> Vec<u8> {
    let mut w = Writer::new();
    SurfaceCreate {
        surface_id: 0,
        width: 1024,
        height: 768,
        format: surface_fmt::_32_xRGB,
        flags: surface_flags::PRIMARY,
    }
    .encode(&mut w);
    w.into_vec()
}

#[tokio::test]
async fn client_receives_surface_created_event() {
    let (listener, addr) = bind_loopback().await;
    let server = Arc::new(Server::new("pw").unwrap());
    let ts = Arc::new(tokio::sync::Mutex::new(TestServer {
        display_script: vec![(display_msg::SURFACE_CREATE, build_surface_create())],
        inputs_tx: None,
    }));
    let server_handle = tokio::spawn(run_test_server(listener, server.clone(), ts));

    let mut client = SpiceClient::connect(&addr, "pw").await.unwrap();

    // Walk events until we see the SurfaceCreated we scripted.
    let evt = timeout(Duration::from_secs(5), async {
        loop {
            let e = client.next_event().await;
            match e {
                Some(ClientEvent::Display(DisplayEvent::SurfaceCreated { .. })) => return e,
                Some(ClientEvent::Closed(_)) | None => return e,
                _ => continue,
            }
        }
    })
    .await
    .expect("timed out waiting for SurfaceCreated");

    match evt {
        Some(ClientEvent::Display(DisplayEvent::SurfaceCreated {
            id,
            width,
            height,
            format,
            primary,
        })) => {
            assert_eq!(id, 0);
            assert_eq!(width, 1024);
            assert_eq!(height, 768);
            assert_eq!(format, SurfaceFormat::Xrgb8888);
            assert!(primary);
        }
        other => panic!("expected SurfaceCreated, got {other:?}"),
    }

    client.close().await;
    server_handle.abort();
}

fn build_draw_fill_solid(surface_id: u32, rect: Rect, color: u32) -> Vec<u8> {
    let fill = DrawFill {
        base: DrawBase {
            surface_id,
            bounds: rect,
            clip: Clip::None,
        },
        brush: Brush::Solid(color),
        rop_descriptor: ropd::OP_PUT,
        mask: QMask {
            flags: 0,
            pos: Point { x: 0, y: 0 },
            bitmap_offset: 0,
        },
    };
    let mut w = Writer::new();
    fill.encode(&mut w);
    w.into_vec()
}

#[tokio::test]
async fn solid_draw_fill_becomes_region_solid_color_event() {
    let (listener, addr) = bind_loopback().await;
    let server = Arc::new(Server::new("pw").unwrap());
    let rect = Rect {
        top: 10,
        left: 20,
        bottom: 110,
        right: 220,
    };
    let color = 0x00_12_34_56;
    let ts = Arc::new(tokio::sync::Mutex::new(TestServer {
        display_script: vec![
            (display_msg::SURFACE_CREATE, build_surface_create()),
            (display_msg::DRAW_FILL, build_draw_fill_solid(0, rect, color)),
        ],
        inputs_tx: None,
    }));
    let server_handle = tokio::spawn(run_test_server(listener, server.clone(), ts));

    let mut client = SpiceClient::connect(&addr, "pw").await.unwrap();

    let evt = timeout(Duration::from_secs(5), async {
        while let Some(e) = client.next_event().await {
            if matches!(e, ClientEvent::Display(DisplayEvent::Region { .. })) {
                return Some(e);
            }
        }
        None
    })
    .await
    .expect("timed out")
    .expect("no Region event");

    match evt {
        ClientEvent::Display(DisplayEvent::Region {
            surface_id,
            rect: got_rect,
            pixels,
            surface_format,
        }) => {
            assert_eq!(surface_id, 0);
            assert_eq!(got_rect, rect);
            assert!(matches!(pixels, RegionPixels::SolidColor(c) if c == color));
            assert_eq!(surface_format, SurfaceFormat::Xrgb8888);
        }
        other => panic!("expected Region, got {other:?}"),
    }

    client.close().await;
    server_handle.abort();
}

/// Build a DRAW_COPY body containing an inline 32-bit BGRX bitmap of the
/// given width/height with the given per-pixel color. Row order is
/// top-down.
fn build_draw_copy_32bit(width: u32, height: u32, color: u32) -> Vec<u8> {
    use capsaicin_proto::types::{Point, Writer};

    let stride = width * 4;
    // Pixel payload: every pixel = `color` LE.
    let mut payload = Vec::with_capacity((stride * height) as usize);
    for _ in 0..(width * height) {
        payload.extend_from_slice(&color.to_le_bytes());
    }
    let chunks_blob = encode_single_chunk(&payload);

    // Lay out the message body: header, then (right after) the
    // SpiceImage + SpiceBitmap + SpiceChunks.
    let header_size = 57; // DrawCopy with NONE clip (see proto tests)
    let image_desc_size = ImageDescriptor::SIZE; // 18
    let bitmap_size = Bitmap::SIZE; // 22

    let src_bitmap_offset = header_size as u32;
    let bitmap_offset_in_msg = src_bitmap_offset + image_desc_size as u32;
    let data_offset_in_msg = bitmap_offset_in_msg + bitmap_size as u32;

    let copy = DrawCopy {
        base: DrawBase {
            surface_id: 0,
            bounds: Rect {
                top: 0,
                left: 0,
                bottom: height as i32,
                right: width as i32,
            },
            clip: Clip::None,
        },
        src_bitmap_offset,
        src_area: Rect {
            top: 0,
            left: 0,
            bottom: height as i32,
            right: width as i32,
        },
        rop_descriptor: ropd::OP_PUT,
        scale_mode: scale_mode::NEAREST,
        mask: QMask {
            flags: 0,
            pos: Point { x: 0, y: 0 },
            bitmap_offset: 0,
        },
    };

    let mut w = Writer::new();
    copy.encode(&mut w);
    assert_eq!(w.as_slice().len(), header_size);

    ImageDescriptor {
        id: 0xdead_beef,
        image_type: image_type::BITMAP,
        flags: 0,
        width,
        height,
    }
    .encode(&mut w);

    Bitmap {
        format: bitmap_fmt::_32BIT,
        flags: bitmap_flags::TOP_DOWN,
        width,
        height,
        stride,
        palette_offset: 0,
        data_offset: data_offset_in_msg,
    }
    .encode(&mut w);

    w.bytes(&chunks_blob);
    w.into_vec()
}

#[tokio::test]
async fn draw_copy_emits_raw_region_with_bitmap_pixels() {
    let (listener, addr) = bind_loopback().await;
    let server = Arc::new(Server::new("pw").unwrap());
    let width = 8;
    let height = 4;
    let color = 0xFF_00_80_40_u32; // B=0x40, G=0x80, R=0x00, A=0xFF
    let ts = Arc::new(tokio::sync::Mutex::new(TestServer {
        display_script: vec![
            (display_msg::SURFACE_CREATE, build_surface_create()),
            (
                display_msg::DRAW_COPY,
                build_draw_copy_32bit(width, height, color),
            ),
        ],
        inputs_tx: None,
    }));
    let server_handle = tokio::spawn(run_test_server(listener, server.clone(), ts));

    let mut client = SpiceClient::connect(&addr, "pw").await.unwrap();

    let evt = timeout(Duration::from_secs(5), async {
        while let Some(e) = client.next_event().await {
            if matches!(
                e,
                ClientEvent::Display(DisplayEvent::Region {
                    pixels: RegionPixels::Raw { .. },
                    ..
                })
            ) {
                return Some(e);
            }
        }
        None
    })
    .await
    .expect("timed out")
    .expect("no Region::Raw event");

    match evt {
        ClientEvent::Display(DisplayEvent::Region {
            surface_id,
            rect,
            pixels: RegionPixels::Raw { data, stride },
            surface_format,
        }) => {
            assert_eq!(surface_id, 0);
            assert_eq!(rect.width(), width as i32);
            assert_eq!(rect.height(), height as i32);
            assert_eq!(stride, width * 4);
            assert_eq!(data.len(), (width * 4 * height) as usize);
            assert_eq!(surface_format, SurfaceFormat::Xrgb8888);
            // Every pixel should match the color we injected.
            for px in data.chunks_exact(4) {
                assert_eq!(px, color.to_le_bytes());
            }
        }
        other => panic!("expected Region(Raw), got {other:?}"),
    }

    client.close().await;
    server_handle.abort();
}

#[tokio::test]
async fn draw_copy_bottom_up_is_flipped_to_top_down() {
    let (listener, addr) = bind_loopback().await;
    let server = Arc::new(Server::new("pw").unwrap());
    let width = 2u32;
    let height = 3u32;
    let stride = width * 4;

    // Row 0 = red, row 1 = green, row 2 = blue (in top-down order).
    let red: u32 = 0xFF_00_00_FF;
    let green: u32 = 0xFF_00_FF_00;
    let blue: u32 = 0xFF_FF_00_00;
    let mut top_down = Vec::<u8>::new();
    for &c in &[red, green, blue] {
        for _ in 0..width {
            top_down.extend_from_slice(&c.to_le_bytes());
        }
    }
    // Server-side payload is bottom-up (flag NOT set): row 2 first.
    let mut bottom_up_payload = Vec::new();
    for row in (0..height).rev() {
        let start = (row * stride) as usize;
        let end = start + stride as usize;
        bottom_up_payload.extend_from_slice(&top_down[start..end]);
    }

    use capsaicin_proto::types::{Point, Writer};
    let chunks = encode_single_chunk(&bottom_up_payload);
    let header_size = 57;
    let src_bitmap_offset = header_size as u32;
    let bitmap_offset_in_msg = src_bitmap_offset + ImageDescriptor::SIZE as u32;
    let data_offset_in_msg = bitmap_offset_in_msg + Bitmap::SIZE as u32;

    let copy = DrawCopy {
        base: DrawBase {
            surface_id: 0,
            bounds: Rect {
                top: 0,
                left: 0,
                bottom: height as i32,
                right: width as i32,
            },
            clip: Clip::None,
        },
        src_bitmap_offset,
        src_area: Rect {
            top: 0,
            left: 0,
            bottom: height as i32,
            right: width as i32,
        },
        rop_descriptor: ropd::OP_PUT,
        scale_mode: scale_mode::NEAREST,
        mask: QMask {
            flags: 0,
            pos: Point { x: 0, y: 0 },
            bitmap_offset: 0,
        },
    };

    let mut w = Writer::new();
    copy.encode(&mut w);
    ImageDescriptor {
        id: 1,
        image_type: image_type::BITMAP,
        flags: 0,
        width,
        height,
    }
    .encode(&mut w);
    Bitmap {
        format: bitmap_fmt::_32BIT,
        flags: 0, // bottom-up
        width,
        height,
        stride,
        palette_offset: 0,
        data_offset: data_offset_in_msg,
    }
    .encode(&mut w);
    w.bytes(&chunks);

    let ts = Arc::new(tokio::sync::Mutex::new(TestServer {
        display_script: vec![
            (display_msg::SURFACE_CREATE, build_surface_create()),
            (display_msg::DRAW_COPY, w.into_vec()),
        ],
        inputs_tx: None,
    }));
    let server_handle = tokio::spawn(run_test_server(listener, server.clone(), ts));
    let mut client = SpiceClient::connect(&addr, "pw").await.unwrap();

    let evt = timeout(Duration::from_secs(5), async {
        while let Some(e) = client.next_event().await {
            if matches!(
                e,
                ClientEvent::Display(DisplayEvent::Region {
                    pixels: RegionPixels::Raw { .. },
                    ..
                })
            ) {
                return Some(e);
            }
        }
        None
    })
    .await
    .expect("timed out")
    .expect("no Region event");

    if let ClientEvent::Display(DisplayEvent::Region {
        pixels: RegionPixels::Raw { data, .. },
        ..
    }) = evt
    {
        assert_eq!(
            data, top_down,
            "bottom-up bitmap should be flipped to top-down"
        );
    } else {
        panic!();
    }

    client.close().await;
    server_handle.abort();
}

/// Build a DRAW_COPY whose `src_bitmap` is an LZ_RGB32 image of `width`x
/// `height`, encoded with literal-only LZ runs (round-trip safe).
fn build_draw_copy_lz_rgb32(width: u32, height: u32, pixels_bgra: &[u8]) -> Vec<u8> {
    use capsaicin_proto::types::{Point, Writer};
    assert_eq!(pixels_bgra.len(), (width * height * 4) as usize);

    let mut compressed_payload = Vec::new();
    LzHeader {
        image_type: LzImageType::Rgb32,
        width,
        height,
        stride: width * 4,
        top_down: true,
    }
    .encode(&mut compressed_payload);
    compressed_payload.extend_from_slice(&compress_rgb32_literal(pixels_bgra));

    let header_size = 57; // DrawCopy header with NONE clip
    let src_bitmap_offset = header_size as u32;

    let copy = DrawCopy {
        base: DrawBase {
            surface_id: 0,
            bounds: Rect {
                top: 0,
                left: 0,
                bottom: height as i32,
                right: width as i32,
            },
            clip: Clip::None,
        },
        src_bitmap_offset,
        src_area: Rect {
            top: 0,
            left: 0,
            bottom: height as i32,
            right: width as i32,
        },
        rop_descriptor: ropd::OP_PUT,
        scale_mode: scale_mode::NEAREST,
        mask: QMask {
            flags: 0,
            pos: Point { x: 0, y: 0 },
            bitmap_offset: 0,
        },
    };

    let mut w = Writer::new();
    copy.encode(&mut w);
    ImageDescriptor {
        id: 0xCAFE_BABE,
        image_type: image_type::LZ_RGB,
        flags: 0,
        width,
        height,
    }
    .encode(&mut w);
    // SpiceLZRGBData: u32 data_size + `data_size` bytes inline.
    w.u32(compressed_payload.len() as u32);
    w.bytes(&compressed_payload);
    w.into_vec()
}

#[tokio::test]
async fn draw_copy_lz_rgb32_decompresses_to_raw_region() {
    let (listener, addr) = bind_loopback().await;
    let server = Arc::new(Server::new("pw").unwrap());

    let width = 6u32;
    let height = 4u32;
    // Construct a recognisable BGRA pattern; alpha must be 0 because LZ_RGB32
    // ignores it.
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            pixels.push((x * 17) as u8); // B
            pixels.push((y * 31) as u8); // G
            pixels.push(((x + y) * 13) as u8); // R
            pixels.push(0); // A (always 0 for RGB32)
        }
    }

    let ts = Arc::new(tokio::sync::Mutex::new(TestServer {
        display_script: vec![
            (display_msg::SURFACE_CREATE, build_surface_create()),
            (display_msg::DRAW_COPY, build_draw_copy_lz_rgb32(width, height, &pixels)),
        ],
        inputs_tx: None,
    }));
    let server_handle = tokio::spawn(run_test_server(listener, server.clone(), ts));

    let mut client = SpiceClient::connect(&addr, "pw").await.unwrap();
    let evt = timeout(Duration::from_secs(5), async {
        while let Some(e) = client.next_event().await {
            if matches!(
                e,
                ClientEvent::Display(DisplayEvent::Region {
                    pixels: RegionPixels::Raw { .. },
                    ..
                })
            ) {
                return Some(e);
            }
        }
        None
    })
    .await
    .expect("timed out")
    .expect("no Region event");

    if let ClientEvent::Display(DisplayEvent::Region {
        pixels: RegionPixels::Raw { data, stride },
        rect,
        ..
    }) = evt
    {
        assert_eq!(stride, width * 4);
        assert_eq!(rect.width(), width as i32);
        assert_eq!(rect.height(), height as i32);
        assert_eq!(data, pixels, "decompressed pixels should match input");
    } else {
        panic!();
    }

    client.close().await;
    server_handle.abort();
}

#[tokio::test]
async fn draw_copy_lz_rgba_decompresses_with_alpha() {
    use capsaicin_proto::types::{Point, Writer};

    let (listener, addr) = bind_loopback().await;
    let server = Arc::new(Server::new("pw").unwrap());

    let width = 4u32;
    let height = 3u32;
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for i in 0..(width * height) {
        pixels.push((i * 7) as u8); // B
        pixels.push((i * 11) as u8); // G
        pixels.push((i * 13) as u8); // R
        pixels.push((i * 19 + 1) as u8); // A
    }

    // Build LZ_RGBA: header + RGB32 pass + alpha pass.
    let mut compressed_payload = Vec::new();
    LzHeader {
        image_type: LzImageType::Rgba,
        width,
        height,
        stride: width * 4,
        top_down: true,
    }
    .encode(&mut compressed_payload);
    compressed_payload.extend_from_slice(&compress_rgb32_literal(&pixels));
    compressed_payload.extend_from_slice(&compress_alpha_literal(&pixels));

    let header_size = 57;
    let src_bitmap_offset = header_size as u32;

    let copy = DrawCopy {
        base: DrawBase {
            surface_id: 0,
            bounds: Rect {
                top: 0,
                left: 0,
                bottom: height as i32,
                right: width as i32,
            },
            clip: Clip::None,
        },
        src_bitmap_offset,
        src_area: Rect {
            top: 0,
            left: 0,
            bottom: height as i32,
            right: width as i32,
        },
        rop_descriptor: ropd::OP_PUT,
        scale_mode: scale_mode::NEAREST,
        mask: QMask {
            flags: 0,
            pos: Point { x: 0, y: 0 },
            bitmap_offset: 0,
        },
    };
    let mut w = Writer::new();
    copy.encode(&mut w);
    ImageDescriptor {
        id: 1,
        image_type: image_type::LZ_RGB,
        flags: 0,
        width,
        height,
    }
    .encode(&mut w);
    w.u32(compressed_payload.len() as u32);
    w.bytes(&compressed_payload);

    let ts = Arc::new(tokio::sync::Mutex::new(TestServer {
        display_script: vec![
            (display_msg::SURFACE_CREATE, build_surface_create()),
            (display_msg::DRAW_COPY, w.into_vec()),
        ],
        inputs_tx: None,
    }));
    let server_handle = tokio::spawn(run_test_server(listener, server.clone(), ts));

    let mut client = SpiceClient::connect(&addr, "pw").await.unwrap();
    let evt = timeout(Duration::from_secs(5), async {
        while let Some(e) = client.next_event().await {
            if matches!(
                e,
                ClientEvent::Display(DisplayEvent::Region {
                    pixels: RegionPixels::Raw { .. },
                    ..
                })
            ) {
                return Some(e);
            }
        }
        None
    })
    .await
    .expect("timed out")
    .expect("no Region event");

    if let ClientEvent::Display(DisplayEvent::Region {
        pixels: RegionPixels::Raw { data, .. },
        ..
    }) = evt
    {
        assert_eq!(data, pixels);
    } else {
        panic!();
    }

    client.close().await;
    server_handle.abort();
}

fn encode_solid_jpeg(width: u16, height: u16, r: u8, g: u8, b: u8) -> Vec<u8> {
    use jpeg_encoder::{ColorType, Encoder as JEnc};
    let mut rgb = Vec::with_capacity((width as usize) * (height as usize) * 3);
    for _ in 0..(width as usize * height as usize) {
        rgb.push(r);
        rgb.push(g);
        rgb.push(b);
    }
    let mut buf = Vec::new();
    JEnc::new(&mut buf, 95).encode(&rgb, width, height, ColorType::Rgb).unwrap();
    buf
}

#[tokio::test]
async fn mjpeg_stream_create_then_data_emits_decoded_frame() {
    use capsaicin_proto::types::Writer;
    use capsaicin_proto::draw::Clip;
    let (listener, addr) = bind_loopback().await;
    let server = Arc::new(Server::new("pw").unwrap());

    let dest = Rect { top: 50, left: 100, bottom: 50 + 8, right: 100 + 16 };
    let create = StreamCreate {
        surface_id: 0,
        stream_id: 7,
        flags: stream_flags::TOP_DOWN,
        codec: VideoCodec::Mjpeg,
        stamp: 0,
        stream_width: 16,
        stream_height: 8,
        src_width: 16,
        src_height: 8,
        dest,
        clip: Clip::None,
    };
    let mut create_buf = Writer::new();
    create.encode(&mut create_buf);

    let jpeg = encode_solid_jpeg(16, 8, 220, 140, 60);
    let data = StreamData {
        header: StreamDataHeader {
            stream_id: 7,
            multi_media_time: 12345,
        },
        data: jpeg,
    };
    let mut data_buf = Writer::new();
    data.encode(&mut data_buf);

    let destroy = StreamDestroy { stream_id: 7 };
    let mut destroy_buf = Writer::new();
    destroy.encode(&mut destroy_buf);

    let ts = Arc::new(tokio::sync::Mutex::new(TestServer {
        display_script: vec![
            (display_msg::SURFACE_CREATE, build_surface_create()),
            (display_msg::STREAM_CREATE, create_buf.into_vec()),
            (display_msg::STREAM_DATA, data_buf.into_vec()),
            (display_msg::STREAM_DESTROY, destroy_buf.into_vec()),
        ],
        inputs_tx: None,
    }));
    let server_handle = tokio::spawn(run_test_server(listener, server.clone(), ts));

    let mut client = SpiceClient::connect(&addr, "pw").await.unwrap();
    let mut saw_create = false;
    let mut saw_frame = false;
    let mut saw_destroy = false;

    timeout(Duration::from_secs(5), async {
        while let Some(e) = client.next_event().await {
            match &e {
                ClientEvent::Display(DisplayEvent::StreamCreated {
                    stream_id, codec, dest: got_dest, ..
                }) => {
                    assert_eq!(*stream_id, 7);
                    assert!(matches!(codec, VideoCodec::Mjpeg));
                    assert_eq!(*got_dest, dest);
                    saw_create = true;
                }
                ClientEvent::Display(DisplayEvent::StreamFrame {
                    stream_id,
                    multi_media_time,
                    dest_rect,
                    pixels,
                }) => {
                    assert_eq!(*stream_id, 7);
                    assert_eq!(*multi_media_time, 12345);
                    assert_eq!(*dest_rect, dest);
                    if let RegionPixels::Raw { data, stride } = pixels {
                        assert_eq!(*stride, 16 * 4);
                        assert_eq!(data.len(), 16 * 8 * 4);
                        // Spot-check a centre pixel: BGRA, JPEG-lossy
                        // around our (220, 140, 60) source colour.
                        let centre = ((4 * 16) + 8) * 4;
                        let (b, g, r, a) = (
                            data[centre],
                            data[centre + 1],
                            data[centre + 2],
                            data[centre + 3],
                        );
                        assert!((b as i32 - 60).abs() < 14, "B={b}");
                        assert!((g as i32 - 140).abs() < 14, "G={g}");
                        assert!((r as i32 - 220).abs() < 14, "R={r}");
                        assert_eq!(a, 0xFF);
                    } else {
                        panic!("expected Raw pixels");
                    }
                    saw_frame = true;
                }
                ClientEvent::Display(DisplayEvent::StreamDestroyed { stream_id }) => {
                    assert_eq!(*stream_id, 7);
                    saw_destroy = true;
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("timed out");

    assert!(saw_create, "missing StreamCreated");
    assert!(saw_frame, "missing StreamFrame");
    assert!(saw_destroy, "missing StreamDestroyed");

    client.close().await;
    server_handle.abort();
}

#[tokio::test]
async fn unknown_codec_emits_no_stream_frame() {
    use capsaicin_proto::types::Writer;
    use capsaicin_proto::draw::Clip;
    let (listener, addr) = bind_loopback().await;
    let server = Arc::new(Server::new("pw").unwrap());

    let create = StreamCreate {
        surface_id: 0,
        stream_id: 1,
        flags: 0,
        codec: VideoCodec::H264, // we don't decode this
        stamp: 0,
        stream_width: 8,
        stream_height: 8,
        src_width: 8,
        src_height: 8,
        dest: Rect { top: 0, left: 0, bottom: 8, right: 8 },
        clip: Clip::None,
    };
    let mut create_buf = Writer::new();
    create.encode(&mut create_buf);

    let data = StreamData {
        header: StreamDataHeader {
            stream_id: 1,
            multi_media_time: 0,
        },
        data: vec![0u8; 32], // arbitrary, will be ignored by codec dispatcher
    };
    let mut data_buf = Writer::new();
    data.encode(&mut data_buf);

    let ts = Arc::new(tokio::sync::Mutex::new(TestServer {
        display_script: vec![
            (display_msg::SURFACE_CREATE, build_surface_create()),
            (display_msg::STREAM_CREATE, create_buf.into_vec()),
            (display_msg::STREAM_DATA, data_buf.into_vec()),
        ],
        inputs_tx: None,
    }));
    let server_handle = tokio::spawn(run_test_server(listener, server.clone(), ts));

    let mut client = SpiceClient::connect(&addr, "pw").await.unwrap();

    let mut saw_create = false;
    timeout(Duration::from_millis(750), async {
        while let Some(e) = client.next_event().await {
            match &e {
                ClientEvent::Display(DisplayEvent::StreamCreated { codec, .. }) => {
                    assert!(matches!(codec, VideoCodec::H264));
                    saw_create = true;
                }
                ClientEvent::Display(DisplayEvent::StreamFrame { .. }) => {
                    panic!("must not emit StreamFrame for unsupported codec");
                }
                _ => {}
            }
        }
    })
    .await
    .ok();

    assert!(saw_create);
    client.close().await;
    server_handle.abort();
}

/// Build a DRAW_COPY whose `src_bitmap` is a literal-only GLZ_RGB32
/// image, exercising the client's GLZ dispatcher end-to-end.
fn build_draw_copy_glz_rgb32(width: u32, height: u32, pixels_bgra: &[u8]) -> Vec<u8> {
    use capsaicin_proto::types::{Point, Writer};
    assert_eq!(pixels_bgra.len(), (width * height * 4) as usize);

    let mut compressed_payload = Vec::new();
    GlzHeader {
        image_type: LzImageType::Rgb32,
        top_down: true,
        width,
        height,
        stride: width * 4,
        id: 1,
        win_head_dist: 0,
    }
    .encode(&mut compressed_payload);
    compressed_payload.extend_from_slice(&glz_compress_rgb32_literal(pixels_bgra));

    let header_size = 57;
    let src_bitmap_offset = header_size as u32;

    let copy = DrawCopy {
        base: DrawBase {
            surface_id: 0,
            bounds: Rect {
                top: 0,
                left: 0,
                bottom: height as i32,
                right: width as i32,
            },
            clip: Clip::None,
        },
        src_bitmap_offset,
        src_area: Rect {
            top: 0,
            left: 0,
            bottom: height as i32,
            right: width as i32,
        },
        rop_descriptor: ropd::OP_PUT,
        scale_mode: scale_mode::NEAREST,
        mask: QMask {
            flags: 0,
            pos: Point { x: 0, y: 0 },
            bitmap_offset: 0,
        },
    };

    let mut w = Writer::new();
    copy.encode(&mut w);
    ImageDescriptor {
        id: 0xCAFE,
        image_type: image_type::GLZ_RGB,
        flags: 0,
        width,
        height,
    }
    .encode(&mut w);
    w.u32(compressed_payload.len() as u32);
    w.bytes(&compressed_payload);
    w.into_vec()
}

#[tokio::test]
async fn draw_copy_glz_rgb32_intra_decodes_to_raw_region() {
    let (listener, addr) = bind_loopback().await;
    let server = Arc::new(Server::new("pw").unwrap());
    let width = 5u32;
    let height = 3u32;
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for i in 0..(width * height) {
        pixels.push((i * 9) as u8); // B
        pixels.push((i * 19) as u8); // G
        pixels.push((i * 23) as u8); // R
        pixels.push(0); // A
    }

    let ts = Arc::new(tokio::sync::Mutex::new(TestServer {
        display_script: vec![
            (display_msg::SURFACE_CREATE, build_surface_create()),
            (
                display_msg::DRAW_COPY,
                build_draw_copy_glz_rgb32(width, height, &pixels),
            ),
        ],
        inputs_tx: None,
    }));
    let server_handle = tokio::spawn(run_test_server(listener, server.clone(), ts));

    let mut client = SpiceClient::connect(&addr, "pw").await.unwrap();
    let evt = timeout(Duration::from_secs(5), async {
        while let Some(e) = client.next_event().await {
            if matches!(
                e,
                ClientEvent::Display(DisplayEvent::Region {
                    pixels: RegionPixels::Raw { .. },
                    ..
                })
            ) {
                return Some(e);
            }
        }
        None
    })
    .await
    .expect("timed out")
    .expect("no Region event");

    if let ClientEvent::Display(DisplayEvent::Region {
        pixels: RegionPixels::Raw { data, stride },
        rect,
        ..
    }) = evt
    {
        assert_eq!(stride, width * 4);
        assert_eq!(rect.width(), width as i32);
        assert_eq!(rect.height(), height as i32);
        assert_eq!(data, pixels);
    } else {
        panic!();
    }

    client.close().await;
    server_handle.abort();
}

#[tokio::test]
async fn draw_copy_with_compressed_image_falls_through_to_unhandled() {
    use capsaicin_proto::types::{Point, Writer};
    let (listener, addr) = bind_loopback().await;
    let server = Arc::new(Server::new("pw").unwrap());
    let src_bitmap_offset = 57u32;

    let copy = DrawCopy {
        base: DrawBase {
            surface_id: 0,
            bounds: Rect { top: 0, left: 0, bottom: 8, right: 8 },
            clip: Clip::None,
        },
        src_bitmap_offset,
        src_area: Rect { top: 0, left: 0, bottom: 8, right: 8 },
        rop_descriptor: ropd::OP_PUT,
        scale_mode: scale_mode::NEAREST,
        mask: QMask {
            flags: 0,
            pos: Point { x: 0, y: 0 },
            bitmap_offset: 0,
        },
    };
    let mut w = Writer::new();
    copy.encode(&mut w);
    ImageDescriptor {
        id: 1,
        image_type: image_type::LZ_RGB, // compressed — we don't decode
        flags: 0,
        width: 8,
        height: 8,
    }
    .encode(&mut w);

    let ts = Arc::new(tokio::sync::Mutex::new(TestServer {
        display_script: vec![
            (display_msg::SURFACE_CREATE, build_surface_create()),
            (display_msg::DRAW_COPY, w.into_vec()),
        ],
        inputs_tx: None,
    }));
    let server_handle = tokio::spawn(run_test_server(listener, server.clone(), ts));
    let mut client = SpiceClient::connect(&addr, "pw").await.unwrap();

    let cls = timeout(Duration::from_secs(5), async {
        while let Some(e) = client.next_event().await {
            match &e {
                ClientEvent::Display(DisplayEvent::Region { .. }) => return Some("region"),
                ClientEvent::Display(DisplayEvent::UnhandledDraw { msg_type, .. })
                    if *msg_type == display_msg::DRAW_COPY =>
                {
                    return Some("unhandled");
                }
                _ => {}
            }
        }
        None
    })
    .await
    .expect("timed out")
    .expect("no classification event");

    assert_eq!(cls, "unhandled", "compressed images must not emit Region");

    client.close().await;
    server_handle.abort();
}

#[tokio::test]
async fn non_simple_draw_fill_stays_unhandled() {
    // Pattern brush — we don't render those yet; must fall through to UnhandledDraw.
    let (listener, addr) = bind_loopback().await;
    let server = Arc::new(Server::new("pw").unwrap());
    let fill = DrawFill {
        base: DrawBase {
            surface_id: 0,
            bounds: Rect { top: 0, left: 0, bottom: 10, right: 10 },
            clip: Clip::None,
        },
        brush: Brush::Pattern {
            offset: 48,
            pos: Point { x: 0, y: 0 },
        },
        rop_descriptor: ropd::OP_PUT,
        mask: QMask {
            flags: 0,
            pos: Point { x: 0, y: 0 },
            bitmap_offset: 0,
        },
    };
    let mut w = Writer::new();
    fill.encode(&mut w);
    let ts = Arc::new(tokio::sync::Mutex::new(TestServer {
        display_script: vec![
            (display_msg::SURFACE_CREATE, build_surface_create()),
            (display_msg::DRAW_FILL, w.into_vec()),
        ],
        inputs_tx: None,
    }));
    let server_handle = tokio::spawn(run_test_server(listener, server.clone(), ts));

    let mut client = SpiceClient::connect(&addr, "pw").await.unwrap();
    let evt = timeout(Duration::from_secs(5), async {
        while let Some(e) = client.next_event().await {
            match &e {
                ClientEvent::Display(DisplayEvent::Region { .. }) => return Some(("region", e)),
                ClientEvent::Display(DisplayEvent::UnhandledDraw { msg_type, .. })
                    if *msg_type == display_msg::DRAW_FILL =>
                {
                    return Some(("unhandled", e));
                }
                _ => {}
            }
        }
        None
    })
    .await
    .expect("timed out")
    .expect("no classification event");

    assert_eq!(evt.0, "unhandled", "pattern brush should not emit Region");

    client.close().await;
    server_handle.abort();
}

#[tokio::test]
async fn unknown_display_command_becomes_unhandled_event_and_keeps_stream_healthy() {
    let (listener, addr) = bind_loopback().await;
    let server = Arc::new(Server::new("pw").unwrap());
    // Script: send an unknown DRAW_FILL (we don't parse the body),
    // followed by a SURFACE_CREATE. The client should skip the first
    // gracefully and still see the second.
    let fake_draw_fill = (display_msg::DRAW_FILL, vec![0u8; 16]);
    let ts = Arc::new(tokio::sync::Mutex::new(TestServer {
        display_script: vec![
            fake_draw_fill,
            (display_msg::SURFACE_CREATE, build_surface_create()),
        ],
        inputs_tx: None,
    }));
    let server_handle = tokio::spawn(run_test_server(listener, server.clone(), ts));

    let mut client = SpiceClient::connect(&addr, "pw").await.unwrap();

    let mut saw_unhandled = false;
    let mut saw_surface = false;
    timeout(Duration::from_secs(5), async {
        while let Some(e) = client.next_event().await {
            match e {
                ClientEvent::Display(DisplayEvent::UnhandledDraw { msg_type, .. }) => {
                    assert_eq!(msg_type, display_msg::DRAW_FILL);
                    saw_unhandled = true;
                }
                ClientEvent::Display(DisplayEvent::SurfaceCreated { .. }) => {
                    saw_surface = true;
                    break;
                }
                ClientEvent::Closed(e) => panic!("client closed: {e:?}"),
                _ => {}
            }
        }
    })
    .await
    .expect("timed out");

    assert!(saw_unhandled, "UnhandledDraw should have been emitted");
    assert!(saw_surface, "SurfaceCreated should still have arrived");

    client.close().await;
    server_handle.abort();
}

#[tokio::test]
async fn send_input_reaches_the_server() {
    let (listener, addr) = bind_loopback().await;
    let server = Arc::new(Server::new("pw").unwrap());
    let (tx, rx) = oneshot::channel();
    let ts = Arc::new(tokio::sync::Mutex::new(TestServer {
        display_script: vec![],
        inputs_tx: Some(tx),
    }));
    let server_handle = tokio::spawn(run_test_server(listener, server.clone(), ts));

    let client = SpiceClient::connect(&addr, "pw").await.unwrap();

    client
        .send_input(InputEvent::MousePosition {
            x: 320,
            y: 240,
            buttons: 0,
            display: 0,
        })
        .await
        .unwrap();

    let (msg_type, body) = timeout(Duration::from_secs(5), rx).await.unwrap().unwrap();
    assert_eq!(msg_type, inputs_client::INPUTS_MOUSE_POSITION);
    let pos = MousePosition::decode(&body).unwrap();
    assert_eq!(pos.x, 320);
    assert_eq!(pos.y, 240);

    client.close().await;
    server_handle.abort();
}

#[tokio::test]
async fn missing_channel_returns_clear_error() {
    // Spin up a bare server that ONLY advertises main (no display/inputs).
    let (listener, addr) = bind_loopback().await;
    let server = Arc::new(Server::new("pw").unwrap());

    let server_handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let sv = server.clone();
            tokio::spawn(async move {
                let Ok(accepted) = sv.accept(stream).await else {
                    return;
                };
                if accepted.channel_type == ChannelType::Main {
                    let session_id = if accepted.connection_id == 0 {
                        sv.new_session_id()
                    } else {
                        accepted.connection_id
                    };
                    let mut ch = accepted.channel;
                    let _ = serve_main_bootstrap(
                        &mut ch,
                        session_id,
                        &[capsaicin_proto::types::ChannelId {
                            channel_type: ChannelType::Main as u8,
                            id: 0,
                        }],
                        None,
                    )
                    .await;
                    while ch.read_message().await.is_ok() {}
                }
            });
        }
    });

    let res = SpiceClient::connect(&addr, "pw").await;
    match res {
        Err(capsaicin_client::ClientError::MissingChannel(name)) => {
            assert!(name == "display" || name == "inputs");
        }
        Err(other) => panic!("expected MissingChannel, got {other:?}"),
        Ok(_) => panic!("expected MissingChannel, connect succeeded"),
    }

    // Silence unused import warnings when test mix shifts.
    let _ = common_msg::PING;
    server_handle.abort();
}

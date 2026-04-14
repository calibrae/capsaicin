//! Event types crossing the client / embedder boundary.

use capsaicin_proto::display::{Head, surface_fmt};
use capsaicin_proto::stream::VideoCodec;
use capsaicin_proto::types::Rect as ProtoRect;

use crate::error::ClientError;

/// Everything the client can tell the embedder about.
#[derive(Debug)]
pub enum ClientEvent {
    /// Display-channel event (framebuffer / surface lifecycle).
    Display(DisplayEvent),
    /// Cursor sprite / position / visibility update.
    Cursor(CursorEvent),
    /// Server switched mouse reporting mode. The embedder must look at
    /// this to decide whether to send `MousePosition` (CLIENT mode) or
    /// `MouseMotion` (SERVER mode).
    MouseMode(MouseMode),
    /// Connection ended — either cleanly (`None`) or with an error.
    Closed(Option<ClientError>),
}

/// Cursor-channel events.
#[derive(Debug, Clone)]
pub enum CursorEvent {
    /// New cursor sprite. Embedder should cache by `unique` if
    /// `cacheable` is true — the server may later send a bare
    /// `SetFromCache { unique }` referring to it.
    Set {
        x: i32,
        y: i32,
        hot_x: u16,
        hot_y: u16,
        width: u16,
        height: u16,
        /// Argb8888 pixels, top-down, stride `width * 4`. Empty if
        /// `kind` isn't one we decode (e.g. legacy monochrome).
        pixels: Vec<u8>,
        /// Identifier the server uses to refer to this sprite later if
        /// it chooses to cache it.
        unique: u64,
        /// Whether the server asked us to cache this one (flag
        /// `CACHE_ME`).
        cacheable: bool,
        visible: bool,
    },
    /// Server referred to a previously-cached sprite. Embedder should
    /// look up `unique` and reuse its pixels.
    SetFromCache {
        x: i32,
        y: i32,
        unique: u64,
        visible: bool,
    },
    /// Move the cursor without changing the sprite.
    Move { x: i32, y: i32 },
    /// Hide the cursor.
    Hide,
    /// Drop the cached sprite keyed by `unique`.
    InvalidateOne { unique: u64 },
    /// Drop all cached sprites.
    InvalidateAll,
}

/// Mouse reporting mode negotiated on the main channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseMode {
    /// Guest has an absolute pointing device (usb-tablet /
    /// virtio-tablet). Send `InputEvent::MousePosition`.
    Client,
    /// No absolute device; guest wants relative deltas. Send
    /// `InputEvent::MouseMotion` driven from pointer-lock or similar.
    Server,
}

impl MouseMode {
    pub fn from_raw(v: u32) -> Self {
        use capsaicin_proto::main_chan::mouse_mode;
        if v & mouse_mode::CLIENT != 0 {
            Self::Client
        } else {
            Self::Server
        }
    }
}

/// Surface pixel format as exposed to the embedder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceFormat {
    /// 32-bit little-endian X_R_G_B (alpha byte ignored).
    Xrgb8888,
    /// 32-bit little-endian A_R_G_B with a real alpha channel.
    Argb8888,
    /// 16-bit 5:6:5 packed RGB.
    Rgb565,
    /// 16-bit 5:5:5 packed RGB.
    Rgb555,
    /// 8-bit alpha mask.
    A8,
    /// 1-bit alpha mask.
    A1,
    /// Format we don't interpret yet; caller can still report the surface
    /// existed and allocate backing storage of the right size.
    Unknown(u32),
}

impl SurfaceFormat {
    pub fn from_raw(v: u32) -> Self {
        match v {
            surface_fmt::_32_xRGB => Self::Xrgb8888,
            surface_fmt::_32_ARGB => Self::Argb8888,
            surface_fmt::_16_565 => Self::Rgb565,
            surface_fmt::_16_555 => Self::Rgb555,
            surface_fmt::_8_A => Self::A8,
            surface_fmt::_1_A => Self::A1,
            other => Self::Unknown(other),
        }
    }

    /// Bytes per pixel for formats we understand.
    pub fn bytes_per_pixel(&self) -> Option<u32> {
        Some(match self {
            Self::Xrgb8888 | Self::Argb8888 => 4,
            Self::Rgb565 | Self::Rgb555 => 2,
            Self::A8 => 1,
            Self::A1 | Self::Unknown(_) => return None,
        })
    }
}

/// Axis-aligned rectangle in surface coordinates: inclusive top/left,
/// exclusive bottom/right. Re-exported here so the embedder needs one
/// crate, not two.
pub type Rect = ProtoRect;

/// Pixel payload carried by a [`DisplayEvent::Region`].
///
/// Solid-fill draws (the simplest SPICE primitive) take the `SolidColor`
/// form so the caller can `memset` efficiently instead of allocating a
/// full buffer. Bitmap blits produce `Raw`.
#[derive(Debug, Clone)]
pub enum RegionPixels {
    /// Every pixel in the rect is this 32-bit color. Layout in memory on
    /// little-endian platforms is `B, G, R, A` — paintable directly into
    /// `Xrgb8888` / `Argb8888` surfaces.
    SolidColor(u32),
    /// Packed pixels in the surface's native format. `stride` is the
    /// byte pitch between rows.
    Raw { data: Vec<u8>, stride: u32 },
}

/// Display-channel event surface.
#[derive(Debug, Clone)]
pub enum DisplayEvent {
    /// Legacy `MSG_DISPLAY_MODE` (some servers still send it as a hint).
    Mode {
        width: u32,
        height: u32,
        bits: u32,
    },
    /// A new surface has come into existence. Allocate a framebuffer.
    SurfaceCreated {
        id: u32,
        width: u32,
        height: u32,
        format: SurfaceFormat,
        /// True for the primary/desktop surface (id 0 in practice).
        primary: bool,
    },
    /// A previously created surface is going away.
    SurfaceDestroyed {
        id: u32,
    },
    /// Server-driven monitor layout — useful for window sizing in the GUI.
    MonitorsConfig {
        max_allowed: u16,
        heads: Vec<Head>,
    },
    /// Server says "the frame is now consistent, present it."
    Mark,
    /// Server says "discard all cached pixmaps / palettes / surfaces."
    Reset,
    /// A drawing / stream command arrived that we don't parse yet. The
    /// connection stays healthy; the embedder may want to log this.
    UnhandledDraw {
        msg_type: u16,
        size: usize,
    },
    /// A region of a surface has new pixel content — blit it into your
    /// framebuffer for surface `surface_id` at `rect`.
    Region {
        surface_id: u32,
        rect: Rect,
        pixels: RegionPixels,
        /// Surface format the pixels belong to, as reported by the
        /// preceding `SurfaceCreated` event.
        surface_format: SurfaceFormat,
    },
    /// Server is opening a video stream into a region of `surface_id`.
    /// `codec` may be one we don't decode (VP8/H.264/VP9/H.265) — in that
    /// case the embedder will only receive `StreamCreated` /
    /// `StreamDestroyed`, never `StreamFrame`, for that stream.
    StreamCreated {
        stream_id: u32,
        surface_id: u32,
        codec: VideoCodec,
        dest: Rect,
        /// Source frame dimensions emitted by the server's encoder.
        src_width: u32,
        src_height: u32,
    },
    /// A decoded frame for a previously-created stream. `pixels` is BGRA
    /// (`stride = width * 4`), top-down, ready to blit at
    /// `dest_rect` on the surface that owned the stream.
    StreamFrame {
        stream_id: u32,
        multi_media_time: u32,
        dest_rect: Rect,
        pixels: RegionPixels,
    },
    /// The server tore down `stream_id` (or all streams if the message
    /// was `STREAM_DESTROY_ALL`).
    StreamDestroyed { stream_id: u32 },
    /// Copy a rectangle of pixels from one location to another within
    /// the same surface. Used by compositors for window movement,
    /// scrolling, etc. Embedder must read the source pixels from its
    /// own framebuffer and write them to `dest_rect`.
    CopyRect {
        surface_id: u32,
        /// Source top-left in surface coordinates. Width/height are
        /// implied by `dest_rect`.
        src_x: i32,
        src_y: i32,
        dest_rect: Rect,
    },
}

/// Everything the embedder can push to the guest.
#[derive(Debug, Clone, Copy)]
pub enum InputEvent {
    /// PC-AT set-1 scancode (multi-byte codes packed big-endian into the
    /// low bytes; e.g. the right-arrow key is `0xe04d`).
    KeyDown(u32),
    KeyUp(u32),
    /// Server-visible modifier/LED state (caps/num/scroll lock).
    KeyModifiers(u32),
    /// Relative mouse motion (server mode).
    MouseMotion {
        dx: i32,
        dy: i32,
        buttons: u32,
    },
    /// Absolute mouse position (client mode).
    MousePosition {
        x: u32,
        y: u32,
        buttons: u32,
        display: u8,
    },
    /// Mouse button press. `button` uses SPICE numbering (1=left, 2=middle,
    /// 3=right, 4=wheel up, 5=wheel down, 6/7=side/extra).
    MousePress {
        button: u8,
        buttons: u32,
    },
    MouseRelease {
        button: u8,
        buttons: u32,
    },
}

//! Live SPICE viewer: a winit window driven by `SpiceClient` events.
//!
//! Threading model:
//!
//! ```text
//!   main thread (winit event loop)
//!         │  ─── std::sync::mpsc<PaintMsg> ───▶  ViewerApp framebuffer
//!         │
//!         ├──── tokio::sync::mpsc<InputEvent> ──▶  spice task
//!         │
//!   sidecar thread (tokio runtime)
//!         │       SpiceClient::next_event
//!         │       SpiceClient::send_input
//! ```
//!
//! Painting is best-effort CPU rasterisation — we maintain a single
//! `Vec<u32>` framebuffer in softbuffer's native `0RGB` layout and
//! convert from BGRA at blit time. SPICE solid-color fills use
//! `slice::fill` per row.

use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use capsaicin_client::{
    ClientEvent, ClientError, DisplayEvent, InputEvent, Rect, RegionPixels,
};
use crate::{TlsPolicy, connect_with_policy};
use capsaicin_proto::inputs::{button as spice_button, button_mask};
use softbuffer::Surface;
use tokio::sync::mpsc as tokio_mpsc;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

/// Messages the spice task pushes to the winit thread.
enum PaintMsg {
    SurfaceCreated { width: u32, height: u32 },
    Region { rect: Rect, pixels: RegionPixels },
    StreamFrame { rect: Rect, pixels: RegionPixels },
    /// Copy a rectangle from `(src_x, src_y)` to `dest_rect` within the
    /// framebuffer. Used by SPICE `COPY_BITS`.
    CopyRect { src_x: i32, src_y: i32, dest_rect: Rect },
    Closed { error: Option<ClientError> },
}

/// Public entry point. Blocks until the window is closed or the SPICE
/// connection drops.
pub fn run(addr: &str, password: &str, policy: TlsPolicy) -> Result<(), Box<dyn std::error::Error>> {
    let (paint_tx, paint_rx) = std_mpsc::channel::<PaintMsg>();
    let (input_tx, input_rx) = tokio_mpsc::channel::<InputEvent>(256);

    // Tokio runtime in a sidecar thread so it doesn't fight winit for
    // the main thread.
    let addr_owned = addr.to_string();
    let password_owned = password.to_string();
    let _spice_thread = std::thread::Builder::new()
        .name("capsaicin-spice".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = paint_tx.send(PaintMsg::Closed {
                        error: Some(ClientError::Net(capsaicin_net::NetError::Io(
                            std::io::Error::other(e.to_string()),
                        ))),
                    });
                    return;
                }
            };
            rt.block_on(spice_task(addr_owned, password_owned, policy, paint_tx, input_rx));
        })
        .expect("spawn spice thread");

    // Winit owns the main thread.
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = ViewerApp::new(paint_rx, input_tx);
    event_loop.run_app(&mut app)?;
    Ok(())
}

async fn spice_task(
    addr: String,
    password: String,
    policy: TlsPolicy,
    paint_tx: std_mpsc::Sender<PaintMsg>,
    mut input_rx: tokio_mpsc::Receiver<InputEvent>,
) {
    tracing::info!(%addr, "viewer: connecting");
    let mut client = match connect_with_policy(&addr, &password, policy).await {
        Ok(c) => c,
        Err(e) => {
            let _ = paint_tx.send(PaintMsg::Closed { error: Some(e) });
            return;
        }
    };
    tracing::info!(session_id = client.session_id(), "viewer: SPICE client connected");

    loop {
        tokio::select! {
            evt = client.next_event() => {
                match evt {
                    Some(ClientEvent::Display(de)) => {
                        if let Some(msg) = display_event_to_paint(de) {
                            if paint_tx.send(msg).is_err() {
                                // window closed
                                break;
                            }
                        }
                    }
                    Some(ClientEvent::MouseMode(mode)) => {
                        tracing::info!(?mode, "viewer: mouse mode");
                    }
                    Some(ClientEvent::Cursor(evt)) => {
                        tracing::debug!(?evt, "viewer: cursor event");
                    }
                    Some(ClientEvent::Closed(err)) => {
                        let _ = paint_tx.send(PaintMsg::Closed { error: err });
                        break;
                    }
                    None => {
                        let _ = paint_tx.send(PaintMsg::Closed { error: None });
                        break;
                    }
                }
            }
            Some(input) = input_rx.recv() => {
                if let Err(e) = client.send_input(input).await {
                    tracing::warn!(%e, "viewer: input dispatch failed");
                }
            }
        }
    }
    client.close().await;
}

fn display_event_to_paint(evt: DisplayEvent) -> Option<PaintMsg> {
    match evt {
        DisplayEvent::SurfaceCreated {
            id,
            width,
            height,
            format,
            primary,
        } => {
            tracing::info!(
                id, width, height, ?format, primary,
                "viewer: SurfaceCreated"
            );
            primary.then_some(PaintMsg::SurfaceCreated { width, height })
        }
        DisplayEvent::Region { rect, pixels, .. } => {
            tracing::debug!(
                w = rect.right - rect.left,
                h = rect.bottom - rect.top,
                solid = matches!(pixels, RegionPixels::SolidColor(_)),
                "viewer: Region"
            );
            Some(PaintMsg::Region { rect, pixels })
        }
        DisplayEvent::StreamFrame {
            dest_rect, pixels, ..
        } => {
            tracing::debug!(
                w = dest_rect.right - dest_rect.left,
                h = dest_rect.bottom - dest_rect.top,
                "viewer: StreamFrame"
            );
            Some(PaintMsg::StreamFrame {
                rect: dest_rect,
                pixels,
            })
        }
        DisplayEvent::CopyRect {
            src_x,
            src_y,
            dest_rect,
            ..
        } => Some(PaintMsg::CopyRect {
            src_x,
            src_y,
            dest_rect,
        }),
        DisplayEvent::UnhandledDraw { msg_type, size } => {
            tracing::debug!(msg_type, size, "viewer: unhandled draw");
            None
        }
        DisplayEvent::Mark => {
            tracing::debug!("viewer: mark");
            None
        }
        other => {
            tracing::debug!(?other, "viewer: other display event");
            None
        }
    }
}

struct ViewerApp {
    window: Option<Arc<Window>>,
    surface: Option<Surface<Arc<Window>, Arc<Window>>>,
    fb: Vec<u32>,
    fb_w: u32,
    fb_h: u32,
    paint_rx: std_mpsc::Receiver<PaintMsg>,
    input_tx: tokio_mpsc::Sender<InputEvent>,
    /// Last cursor position in surface coordinates (for buttons we
    /// re-send position so the server has up-to-date coords).
    cursor_x: u32,
    cursor_y: u32,
    /// SPICE button mask state.
    buttons_state: u32,
    /// True once we've successfully created the window.
    window_created: bool,
    /// Set when the spice task closes; we'll exit the event loop on the
    /// next pump.
    should_exit: bool,
}

impl ViewerApp {
    fn new(
        paint_rx: std_mpsc::Receiver<PaintMsg>,
        input_tx: tokio_mpsc::Sender<InputEvent>,
    ) -> Self {
        Self {
            window: None,
            surface: None,
            fb: Vec::new(),
            fb_w: 0,
            fb_h: 0,
            paint_rx,
            input_tx,
            cursor_x: 0,
            cursor_y: 0,
            buttons_state: 0,
            window_created: false,
            should_exit: false,
        }
    }

    fn ensure_window(&mut self, event_loop: &ActiveEventLoop, w: u32, h: u32) {
        if self.window_created {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(format!("capsaicin viewer — {}×{}", w, h))
            .with_inner_size(PhysicalSize::new(w.max(320), h.max(200)));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                tracing::error!(%e, "viewer: failed to create window");
                event_loop.exit();
                return;
            }
        };
        let context = match softbuffer::Context::new(window.clone()) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(?e, "viewer: failed to init softbuffer context");
                event_loop.exit();
                return;
            }
        };
        let surface = match Surface::new(&context, window.clone()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(?e, "viewer: failed to create softbuffer surface");
                event_loop.exit();
                return;
            }
        };
        self.window = Some(window);
        self.surface = Some(surface);
        self.window_created = true;
    }

    fn resize_fb(&mut self, w: u32, h: u32) {
        if w == self.fb_w && h == self.fb_h {
            return;
        }
        // Guard against a malicious server claiming SurfaceCreated with
        // 65536×65536 (u32 multiply wraps to 0; subsequent indexing
        // would panic). Cap at 16384 per axis / 256 MiB total.
        let pixels = (w as u64).checked_mul(h as u64).unwrap_or(0);
        if w == 0 || h == 0 || w > 16384 || h > 16384 || pixels > 64 * 1024 * 1024 {
            tracing::warn!(w, h, "viewer: SurfaceCreated dimensions out of range, ignoring");
            return;
        }
        self.fb_w = w;
        self.fb_h = h;
        self.fb = vec![0u32; pixels as usize];
    }

    fn fill_rect(&mut self, rect: Rect, color: u32) {
        let (rl, rt, rr, rb) = clamp_rect(&rect, self.fb_w, self.fb_h);
        for y in rt..rb {
            let row_start = (y * self.fb_w + rl) as usize;
            let row_end = (y * self.fb_w + rr) as usize;
            self.fb[row_start..row_end].fill(color);
        }
    }

    /// Copy a rect from `(src_x, src_y)` to `dest_rect` within the
    /// framebuffer. Handles overlapping source/destination by choosing
    /// the iteration order that doesn't trample yet-to-be-read source
    /// pixels (memmove semantics).
    fn copy_rect(&mut self, src_x: i32, src_y: i32, dest_rect: Rect) {
        let (dl, dt, dr, db) = clamp_rect(&dest_rect, self.fb_w, self.fb_h);
        let dst_w = dr - dl;
        let dst_h = db - dt;
        if dst_w == 0 || dst_h == 0 {
            return;
        }
        // Translate dest top-left clipping back to source coords.
        let sx_off = dl as i32 - dest_rect.left;
        let sy_off = dt as i32 - dest_rect.top;
        let sx0 = src_x + sx_off;
        let sy0 = src_y + sy_off;
        // Source must be fully in bounds.
        if sx0 < 0 || sy0 < 0 {
            return;
        }
        let sx0 = sx0 as u32;
        let sy0 = sy0 as u32;
        if sx0 + dst_w > self.fb_w || sy0 + dst_h > self.fb_h {
            return;
        }
        // Pick row iteration order: if dest is below source we must
        // walk rows bottom-up to avoid trampling. Same logic for cols.
        let rows: Box<dyn Iterator<Item = u32>> = if dt > sy0 {
            Box::new((0..dst_h).rev())
        } else {
            Box::new(0..dst_h)
        };
        let col_reverse = dl > sx0;
        for ry in rows {
            let src_row = ((sy0 + ry) * self.fb_w) as usize;
            let dst_row = ((dt + ry) * self.fb_w) as usize;
            if col_reverse {
                for rx in (0..dst_w).rev() {
                    self.fb[dst_row + (dl + rx) as usize] =
                        self.fb[src_row + (sx0 + rx) as usize];
                }
            } else {
                for rx in 0..dst_w {
                    self.fb[dst_row + (dl + rx) as usize] =
                        self.fb[src_row + (sx0 + rx) as usize];
                }
            }
        }
    }

    fn blit_raw(&mut self, rect: Rect, data: &[u8], stride: u32) {
        let (rl, rt, rr, rb) = clamp_rect(&rect, self.fb_w, self.fb_h);
        let src_w = (rect.right - rect.left) as u32;
        let src_h = (rect.bottom - rect.top) as u32;
        if src_w == 0 || src_h == 0 {
            return;
        }
        for y in rt..rb {
            let src_row = (y as i32 - rect.top) as u32;
            if src_row >= src_h {
                break;
            }
            let src_offset = (src_row * stride) as usize;
            let dst_row = (y * self.fb_w + rl) as usize;
            for x in rl..rr {
                let src_col = (x as i32 - rect.left) as u32;
                if src_col >= src_w {
                    break;
                }
                let src_idx = src_offset + (src_col * 4) as usize;
                let b = data[src_idx];
                let g = data[src_idx + 1];
                let r = data[src_idx + 2];
                self.fb[dst_row + (x - rl) as usize] = bgra_to_softbuffer(r, g, b);
            }
        }
    }

    fn redraw(&mut self) {
        let (Some(window), Some(surface)) = (self.window.clone(), self.surface.as_mut()) else {
            return;
        };
        let size = window.inner_size();
        let (sw, sh) = (size.width.max(1), size.height.max(1));
        if let Err(e) = surface.resize(
            std::num::NonZeroU32::new(sw).unwrap(),
            std::num::NonZeroU32::new(sh).unwrap(),
        ) {
            tracing::warn!(?e, "viewer: surface resize failed");
            return;
        }
        let mut buf = match surface.buffer_mut() {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(?e, "viewer: buffer_mut failed");
                return;
            }
        };
        // If our framebuffer matches the window size, copy directly.
        // Otherwise, nearest-neighbour scale (cheap).
        if sw == self.fb_w && sh == self.fb_h && !self.fb.is_empty() {
            buf.copy_from_slice(&self.fb);
        } else if !self.fb.is_empty() {
            for y in 0..sh {
                let src_y = (y as u64 * self.fb_h as u64 / sh as u64) as u32;
                let src_row = (src_y * self.fb_w) as usize;
                let dst_row = (y * sw) as usize;
                for x in 0..sw {
                    let src_x = (x as u64 * self.fb_w as u64 / sw as u64) as u32;
                    buf[dst_row + x as usize] = self.fb[src_row + src_x as usize];
                }
            }
        } else {
            buf.fill(0);
        }
        if let Err(e) = buf.present() {
            tracing::warn!(?e, "viewer: present failed");
        }
    }

    fn handle_key(&mut self, event: KeyEvent) {
        let PhysicalKey::Code(code) = event.physical_key else {
            return;
        };
        let Some(scancode) = winit_keycode_to_spice(code) else {
            return;
        };
        let input = match event.state {
            ElementState::Pressed => InputEvent::KeyDown(scancode),
            // SPICE convention: KEY_UP carries the scancode with the
            // 0x80 break-bit set on the LOW byte. For extended keys
            // (0xE0-prefixed) the high byte stays unchanged.
            ElementState::Released => {
                InputEvent::KeyUp((scancode & 0xFFFF_FF00) | ((scancode & 0xFF) | 0x80))
            }
        };
        self.send_input_nonblocking(input);
    }

    fn handle_cursor_moved(&mut self, x: f64, y: f64) {
        let Some(window) = &self.window else { return };
        if self.fb_w == 0 || self.fb_h == 0 {
            return;
        }
        let size = window.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        // Map window coords → surface coords (handle scaling).
        let sx = ((x * self.fb_w as f64 / size.width as f64) as i64)
            .clamp(0, (self.fb_w - 1) as i64) as u32;
        let sy = ((y * self.fb_h as f64 / size.height as f64) as i64)
            .clamp(0, (self.fb_h - 1) as i64) as u32;
        self.cursor_x = sx;
        self.cursor_y = sy;
        self.send_input_nonblocking(InputEvent::MousePosition {
            x: sx,
            y: sy,
            buttons: self.buttons_state,
            display: 0,
        });
    }

    fn handle_mouse_button(&mut self, state: ElementState, button: MouseButton) {
        let (spice_btn, mask) = match button {
            MouseButton::Left => (spice_button::LEFT, button_mask::LEFT),
            MouseButton::Right => (spice_button::RIGHT, button_mask::RIGHT),
            MouseButton::Middle => (spice_button::MIDDLE, button_mask::MIDDLE),
            _ => return,
        };
        match state {
            ElementState::Pressed => {
                self.buttons_state |= mask;
                self.send_input_nonblocking(InputEvent::MousePress {
                    button: spice_btn,
                    buttons: self.buttons_state,
                });
            }
            ElementState::Released => {
                self.buttons_state &= !mask;
                self.send_input_nonblocking(InputEvent::MouseRelease {
                    button: spice_btn,
                    buttons: self.buttons_state,
                });
            }
        }
    }

    fn send_input_nonblocking(&self, event: InputEvent) {
        match self.input_tx.try_send(event) {
            Ok(()) => {}
            Err(tokio_mpsc::error::TrySendError::Full(_)) => {
                tracing::trace!("viewer: input queue full, dropping event");
            }
            Err(tokio_mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!("viewer: input channel closed");
            }
        }
    }

    fn drain_paint(&mut self) -> bool {
        let mut needs_redraw = false;
        loop {
            match self.paint_rx.try_recv() {
                Ok(PaintMsg::SurfaceCreated { width, height }) => {
                    self.resize_fb(width, height);
                    if let Some(window) = &self.window {
                        let _ = window.request_inner_size(PhysicalSize::new(width, height));
                    }
                    needs_redraw = true;
                }
                Ok(PaintMsg::Region { rect, pixels })
                | Ok(PaintMsg::StreamFrame { rect, pixels }) => {
                    match pixels {
                        RegionPixels::SolidColor(c) => {
                            self.fill_rect(rect, bgra32_to_softbuffer(c));
                        }
                        RegionPixels::Raw { data, stride } => {
                            self.blit_raw(rect, &data, stride);
                        }
                    }
                    needs_redraw = true;
                }
                Ok(PaintMsg::CopyRect { src_x, src_y, dest_rect }) => {
                    self.copy_rect(src_x, src_y, dest_rect);
                    needs_redraw = true;
                }
                Ok(PaintMsg::Closed { error }) => {
                    if let Some(e) = error {
                        tracing::warn!(%e, "viewer: spice task closed");
                    } else {
                        tracing::info!("viewer: spice task closed cleanly");
                    }
                    self.should_exit = true;
                }
                Err(std_mpsc::TryRecvError::Empty) => break,
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    self.should_exit = true;
                    break;
                }
            }
        }
        needs_redraw
    }
}

impl ApplicationHandler for ViewerApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // Open with a default placeholder size; real size arrives via
        // SurfaceCreated.
        self.ensure_window(event_loop, 1024, 768);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Resized(_) => {
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => self.handle_key(event),
            WindowEvent::CursorMoved { position, .. } => {
                self.handle_cursor_moved(position.x, position.y);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                self.handle_mouse_button(state, button);
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.drain_paint() {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
        if self.should_exit {
            event_loop.exit();
            return;
        }
        // Throttle the loop a touch so we don't spin at 100% CPU.
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            std::time::Instant::now() + Duration::from_millis(8),
        ));
    }
}

#[inline]
fn clamp_rect(rect: &Rect, fb_w: u32, fb_h: u32) -> (u32, u32, u32, u32) {
    let l = rect.left.max(0).min(fb_w as i32) as u32;
    let t = rect.top.max(0).min(fb_h as i32) as u32;
    let r = rect.right.max(0).min(fb_w as i32) as u32;
    let b = rect.bottom.max(0).min(fb_h as i32) as u32;
    (l, t, r, b)
}

/// Pack BGR bytes into softbuffer's `0RGB` u32 format.
#[inline]
fn bgra_to_softbuffer(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

/// Convert a SPICE 32-bit BGRA color to softbuffer's `0RGB` u32.
/// SPICE stores `0xAARRGGBB` (where the byte order in memory is B,G,R,A
/// for a 32-bit xRGB surface). We just need the R, G, B bytes.
#[inline]
fn bgra32_to_softbuffer(spice_color: u32) -> u32 {
    let b = (spice_color & 0xFF) as u8;
    let g = ((spice_color >> 8) & 0xFF) as u8;
    let r = ((spice_color >> 16) & 0xFF) as u8;
    bgra_to_softbuffer(r, g, b)
}

/// Map a winit physical key code to a PC AT set-1 scancode that SPICE
/// understands. Multi-byte (extended) scancodes use the high byte for
/// the `0xE0` prefix per SPICE convention.
fn winit_keycode_to_spice(code: KeyCode) -> Option<u32> {
    use KeyCode::*;
    Some(match code {
        Escape => 0x01,
        Digit1 => 0x02,
        Digit2 => 0x03,
        Digit3 => 0x04,
        Digit4 => 0x05,
        Digit5 => 0x06,
        Digit6 => 0x07,
        Digit7 => 0x08,
        Digit8 => 0x09,
        Digit9 => 0x0a,
        Digit0 => 0x0b,
        Minus => 0x0c,
        Equal => 0x0d,
        Backspace => 0x0e,
        Tab => 0x0f,
        KeyQ => 0x10,
        KeyW => 0x11,
        KeyE => 0x12,
        KeyR => 0x13,
        KeyT => 0x14,
        KeyY => 0x15,
        KeyU => 0x16,
        KeyI => 0x17,
        KeyO => 0x18,
        KeyP => 0x19,
        BracketLeft => 0x1a,
        BracketRight => 0x1b,
        Enter => 0x1c,
        ControlLeft => 0x1d,
        KeyA => 0x1e,
        KeyS => 0x1f,
        KeyD => 0x20,
        KeyF => 0x21,
        KeyG => 0x22,
        KeyH => 0x23,
        KeyJ => 0x24,
        KeyK => 0x25,
        KeyL => 0x26,
        Semicolon => 0x27,
        Quote => 0x28,
        Backquote => 0x29,
        ShiftLeft => 0x2a,
        Backslash => 0x2b,
        KeyZ => 0x2c,
        KeyX => 0x2d,
        KeyC => 0x2e,
        KeyV => 0x2f,
        KeyB => 0x30,
        KeyN => 0x31,
        KeyM => 0x32,
        Comma => 0x33,
        Period => 0x34,
        Slash => 0x35,
        ShiftRight => 0x36,
        AltLeft => 0x38,
        Space => 0x39,
        CapsLock => 0x3a,
        F1 => 0x3b,
        F2 => 0x3c,
        F3 => 0x3d,
        F4 => 0x3e,
        F5 => 0x3f,
        F6 => 0x40,
        F7 => 0x41,
        F8 => 0x42,
        F9 => 0x43,
        F10 => 0x44,
        // Extended (0xE0-prefixed) scancodes. SPICE encodes the prefix
        // in the high byte.
        ControlRight => 0xe01d,
        AltRight => 0xe038,
        ArrowUp => 0xe048,
        ArrowLeft => 0xe04b,
        ArrowRight => 0xe04d,
        ArrowDown => 0xe050,
        Home => 0xe047,
        End => 0xe04f,
        PageUp => 0xe049,
        PageDown => 0xe051,
        Insert => 0xe052,
        Delete => 0xe053,
        SuperLeft => 0xe05b,
        SuperRight => 0xe05c,
        _ => return None,
    })
}

//! Input-channel messages (keyboard + mouse).

use crate::types::{Reader, Writer};
use crate::Result;

// ---------- server -> client ----------

/// `SPICE_MSG_INPUTS_INIT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputsInit {
    pub keyboard_modifiers: u32,
}

impl InputsInit {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self {
            keyboard_modifiers: r.u32()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.keyboard_modifiers);
    }
}

/// `SPICE_MSG_INPUTS_KEY_MODIFIERS` — server-driven LED/modifier state.
pub type KeyModifiers = InputsInit;

// ---------- client -> server ----------

/// `SPICE_MSGC_INPUTS_KEY_DOWN` / `..._KEY_UP`. The value is the PC AT
/// scancode (set 1); multi-byte scancodes are packed big-endian into the
/// low-order bytes so the high byte is 0 for 1-byte scancodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyCode {
    pub code: u32,
}

impl KeyCode {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self { code: r.u32()? })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.code);
    }
}

/// `SPICE_MSGC_INPUTS_MOUSE_MOTION` — relative mouse movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseMotion {
    pub dx: i32,
    pub dy: i32,
    pub buttons_state: u32,
}

impl MouseMotion {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self {
            dx: r.i32()?,
            dy: r.i32()?,
            buttons_state: r.u32()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.i32(self.dx);
        w.i32(self.dy);
        w.u32(self.buttons_state);
    }
}

/// `SPICE_MSGC_INPUTS_MOUSE_POSITION` — absolute coordinates (client mouse
/// mode). 13 bytes on the wire (packed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MousePosition {
    pub x: u32,
    pub y: u32,
    pub buttons_state: u32,
    pub display_id: u8,
}

impl MousePosition {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self {
            x: r.u32()?,
            y: r.u32()?,
            buttons_state: r.u32()?,
            display_id: r.u8()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.x);
        w.u32(self.y);
        w.u32(self.buttons_state);
        w.u8(self.display_id);
    }
}

/// `SPICE_MSGC_INPUTS_MOUSE_PRESS` / `..._MOUSE_RELEASE`. 5 bytes (packed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseButton {
    pub button: u8,
    pub buttons_state: u32,
}

impl MouseButton {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        Ok(Self {
            button: r.u8()?,
            buttons_state: r.u32()?,
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u8(self.button);
        w.u32(self.buttons_state);
    }
}

/// SPICE mouse button identifiers.
pub mod button {
    pub const INVALID: u8 = 0;
    pub const LEFT: u8 = 1;
    pub const MIDDLE: u8 = 2;
    pub const RIGHT: u8 = 3;
    pub const UP: u8 = 4; // wheel up
    pub const DOWN: u8 = 5; // wheel down
    pub const SIDE: u8 = 6;
    pub const EXTRA: u8 = 7;
}

/// Bitmask values for `buttons_state`.
pub mod button_mask {
    pub const LEFT: u32 = 1 << 0;
    pub const MIDDLE: u32 = 1 << 1;
    pub const RIGHT: u32 = 1 << 2;
    pub const SIDE: u32 = 1 << 3;
    pub const EXTRA: u32 = 1 << 4;
}

/// Message-type constants for the input channel.
pub mod server_msg {
    pub const INPUTS_INIT: u16 = 101;
    pub const INPUTS_KEY_MODIFIERS: u16 = 102;
    pub const INPUTS_MOUSE_MOTION_ACK: u16 = 111;
}

pub mod client_msg {
    pub const INPUTS_KEY_DOWN: u16 = 101;
    pub const INPUTS_KEY_UP: u16 = 102;
    pub const INPUTS_KEY_MODIFIERS: u16 = 103;
    pub const INPUTS_MOUSE_MOTION: u16 = 111;
    pub const INPUTS_MOUSE_POSITION: u16 = 112;
    pub const INPUTS_MOUSE_PRESS: u16 = 113;
    pub const INPUTS_MOUSE_RELEASE: u16 = 114;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_code_roundtrip() {
        for code in [0x1e_u32, 0xe0_48, 0x0000_001d] {
            let k = KeyCode { code };
            let mut w = Writer::new();
            k.encode(&mut w);
            assert_eq!(KeyCode::decode(w.as_slice()).unwrap(), k);
        }
    }

    #[test]
    fn mouse_position_roundtrip() {
        let m = MousePosition {
            x: 1920,
            y: 1080,
            buttons_state: button_mask::LEFT,
            display_id: 0,
        };
        let mut w = Writer::new();
        m.encode(&mut w);
        assert_eq!(w.as_slice().len(), 13);
        assert_eq!(MousePosition::decode(w.as_slice()).unwrap(), m);
    }

    #[test]
    fn mouse_button_roundtrip() {
        let m = MouseButton {
            button: button::LEFT,
            buttons_state: button_mask::LEFT,
        };
        let mut w = Writer::new();
        m.encode(&mut w);
        assert_eq!(w.as_slice().len(), 5);
        assert_eq!(MouseButton::decode(w.as_slice()).unwrap(), m);
    }
}

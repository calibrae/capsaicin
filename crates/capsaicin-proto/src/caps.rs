//! Capability bitfield helpers.
//!
//! Each capability is a single bit in a flat array of `u32` words
//! transmitted with `LinkMess` / `LinkReply`. Bit `n` is stored in
//! word `n / 32`, bit `n % 32`.

/// Indices of the well-known common capabilities (every channel).
pub mod common {
    pub const PROTOCOL_AUTH_SELECTION: u32 = 0;
    pub const AUTH_SPICE: u32 = 1;
    pub const AUTH_SASL: u32 = 2;
    pub const MINI_HEADER: u32 = 3;
}

/// Indices of the well-known main-channel capabilities.
pub mod main {
    pub const SEMI_SEAMLESS_MIGRATE: u32 = 0;
    pub const NAME_AND_UUID: u32 = 1;
    pub const AGENT_CONNECTED_TOKENS: u32 = 2;
    pub const SEAMLESS_MIGRATE: u32 = 3;
}

/// Indices of the well-known display-channel capabilities.
pub mod display {
    pub const SIZED_STREAM: u32 = 0;
    pub const MONITORS_CONFIG: u32 = 1;
    pub const COMPOSITE: u32 = 2;
    pub const A8_SURFACE: u32 = 3;
    pub const STREAM_REPORT: u32 = 4;
    pub const LZ4_COMPRESSION: u32 = 5;
    pub const PREF_COMPRESSION: u32 = 6;
    pub const GL_SCANOUT: u32 = 7;
    pub const MULTI_CODEC: u32 = 8;
    pub const CODEC_MJPEG: u32 = 9;
    pub const CODEC_VP8: u32 = 10;
    pub const CODEC_H264: u32 = 11;
    pub const PREF_VIDEO_CODEC_TYPE: u32 = 12;
    pub const CODEC_VP9: u32 = 13;
    pub const CODEC_H265: u32 = 14;
}

/// Resizable view over the cap word array.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CapSet(pub Vec<u32>);

impl CapSet {
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    pub fn with_caps<I: IntoIterator<Item = u32>>(caps: I) -> Self {
        let mut s = Self::new();
        for c in caps {
            s.set(c);
        }
        s
    }

    pub fn has(&self, cap: u32) -> bool {
        let word = (cap / 32) as usize;
        let bit = cap % 32;
        self.0.get(word).is_some_and(|w| w & (1 << bit) != 0)
    }

    pub fn set(&mut self, cap: u32) {
        let word = (cap / 32) as usize;
        let bit = cap % 32;
        if self.0.len() <= word {
            self.0.resize(word + 1, 0);
        }
        self.0[word] |= 1 << bit;
    }

    pub fn words(&self) -> &[u32] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_has() {
        let mut c = CapSet::new();
        c.set(common::AUTH_SPICE);
        c.set(common::MINI_HEADER);
        assert!(c.has(common::AUTH_SPICE));
        assert!(c.has(common::MINI_HEADER));
        assert!(!c.has(common::AUTH_SASL));
    }

    #[test]
    fn spans_multiple_words() {
        let mut c = CapSet::new();
        c.set(33);
        assert_eq!(c.0.len(), 2);
        assert!(c.has(33));
        assert!(!c.has(32));
    }

    #[test]
    fn with_caps_builder() {
        let c = CapSet::with_caps([common::AUTH_SPICE, common::MINI_HEADER]);
        assert!(c.has(common::AUTH_SPICE));
        assert!(c.has(common::MINI_HEADER));
    }
}

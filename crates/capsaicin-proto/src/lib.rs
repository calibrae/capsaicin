//! SPICE wire protocol messages and parsing.
//!
//! All SPICE integers are little-endian. Structs on the wire are packed
//! (no alignment padding). This crate provides `encode`/`decode` helpers
//! that read and write directly from byte buffers — no intermediate copies
//! beyond what `bytes::Buf`/`BufMut` require.

pub mod caps;
pub mod common;
pub mod display;
pub mod draw;
pub mod image;
pub mod stream;
pub mod enums;
pub mod error;
pub mod header;
pub mod limits;
pub mod inputs;
pub mod link;
pub mod main_chan;
pub mod types;

pub use error::{ProtoError, Result};

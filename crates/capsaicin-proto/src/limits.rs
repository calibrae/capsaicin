//! Hard bounds enforced when decoding peer-controlled wire fields.
//!
//! Every allocator that sizes itself from a value read off the wire
//! should consult one of these constants (or [`bounded_size`] /
//! [`bounded_count`]) before calling `Vec::with_capacity`, `vec![…; n]`
//! or `BytesMut::zeroed`. Without these, a single hostile peer can
//! claim a 4 GiB allocation in the link handshake before authentication
//! has had a chance to run.

use crate::{ProtoError, Result};

/// Cap on `LinkHeader::size` (bytes of the message that follows the
/// link header). The biggest legitimate value is `LinkReply` plus a
/// modest cap array — well under 1 KiB. 4 KiB leaves headroom.
pub const MAX_LINK_PAYLOAD: usize = 4 * 1024;

/// Cap on each `num_caps` field in a link mess/reply (`u32` → bound to
/// 64 entries × 4 bytes = 256 bytes).
pub const MAX_CAPS_ENTRIES: usize = 64;

/// Cap on `MAIN_CHANNELS_LIST` channel-id entries (each is 2 bytes).
pub const MAX_CHANNELS_LIST: usize = 256;

/// Cap on chunked-data total size (used by `read_chunks` for
/// `SpiceChunks` payloads). Mirrors the Channel framing cap.
pub const MAX_CHUNK_BYTES: usize = 32 * 1024 * 1024;

/// Cap on chunk count inside a `SpiceChunks` (each adds at least 4
/// bytes of header — match the byte cap).
pub const MAX_CHUNK_COUNT: usize = MAX_CHUNK_BYTES / 4;

/// Validate that `claimed` is within `[0, max]` and return it as
/// `usize`. `max` is `usize` because callers commonly want it as an
/// allocation cap.
pub fn bounded_size(claimed: u32, max: usize) -> Result<usize> {
    let n = claimed as usize;
    if n > max {
        return Err(ProtoError::SizeTooLarge {
            declared: claimed,
            max: max.min(u32::MAX as usize) as u32,
        });
    }
    Ok(n)
}

/// Variant of [`bounded_size`] that also propagates `usize` overflow on
/// 32-bit hosts (where `u32 as usize` is identity but addition can
/// wrap).
pub fn bounded_count(claimed: u32, max: usize) -> Result<usize> {
    bounded_size(claimed, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_size_rejects_oversize() {
        assert!(matches!(
            bounded_size(0xFFFF_FFFF, 1024),
            Err(ProtoError::SizeTooLarge { .. })
        ));
        assert_eq!(bounded_size(512, 1024).unwrap(), 512);
        assert_eq!(bounded_size(1024, 1024).unwrap(), 1024);
    }
}

//! SPICE QUIC (Quantized Universal Image Coder) — predictive lossless
//! image compression. Reference: `spice-common/common/quic.c`.
//!
//! Wire layout of a QUIC-compressed image inside `SpiceQUICData`:
//!
//! ```text
//! magic:    u32 (little-endian) = QUIC_MAGIC ("QUIC" = 0x43495551)
//! version:  u32 (little-endian)
//! type:     u32 (little-endian) — QuicImageType
//! width:    u32 (little-endian)
//! height:   u32 (little-endian)
//! ...arithmetic-coded body — sequence of LE 32-bit words...
//! ```
//!
//! Unlike LZ (which is big-endian internally), QUIC reads words as
//! little-endian via `GUINT32_FROM_LE(*io_now)` in the C reference.
//! Bit-level decoding within each loaded word still proceeds MSB-first.
//!
//! The body uses Golomb-Rice + MELCODE state-run coding with per-channel
//! linear prediction. A faithful Rust port is ~1500 lines and is staged
//! out across iterations; today this module exposes the header and the
//! bit-reader plumbing so callers can detect QUIC payloads, report
//! geometry, and skip cleanly until the body decoder lands.

use thiserror::Error;

/// Errors raised by this decoder.
#[derive(Debug, Error)]
pub enum QuicError {
    #[error("buffer too short: need {need} bytes, have {have}")]
    Short { need: usize, have: usize },

    #[error("invalid QUIC magic: expected {expected:#x}, got {got:#x}")]
    BadMagic { expected: u32, got: u32 },

    #[error("unsupported QUIC version {major}.{minor}")]
    BadVersion { major: u32, minor: u32 },

    #[error("invalid QUIC image type {0}")]
    BadImageType(u32),
}

pub type Result<T> = std::result::Result<T, QuicError>;

pub const QUIC_MAGIC: u32 = 0x4349_5551; // "QUIC"
pub const QUIC_VERSION: u32 = 0; // major=0 minor=0

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum QuicImageType {
    Gray = 1,
    Rgb16 = 2,
    Rgb24 = 3,
    Rgb32 = 4,
    Rgba = 5,
}

impl QuicImageType {
    pub fn from_u32(v: u32) -> Result<Self> {
        Ok(match v {
            1 => Self::Gray,
            2 => Self::Rgb16,
            3 => Self::Rgb24,
            4 => Self::Rgb32,
            5 => Self::Rgba,
            _ => return Err(QuicError::BadImageType(v)),
        })
    }

    /// Bytes per output pixel after decoding to its native format.
    pub fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Gray => 1,
            Self::Rgb16 => 2,
            Self::Rgb24 => 3,
            Self::Rgb32 | Self::Rgba => 4,
        }
    }
}

/// Per-stream QUIC header. All fields are big-endian on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuicHeader {
    pub image_type: QuicImageType,
    pub width: u32,
    pub height: u32,
}

pub const QUIC_HEADER_SIZE: usize = 4 * 5;

impl QuicHeader {
    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < QUIC_HEADER_SIZE {
            return Err(QuicError::Short {
                need: QUIC_HEADER_SIZE,
                have: buf.len(),
            });
        }
        let magic = le_u32(&buf[0..4]);
        if magic != QUIC_MAGIC {
            return Err(QuicError::BadMagic {
                expected: QUIC_MAGIC,
                got: magic,
            });
        }
        let version = le_u32(&buf[4..8]);
        if version != QUIC_VERSION {
            return Err(QuicError::BadVersion {
                major: version >> 16,
                minor: version & 0xffff,
            });
        }
        let image_type = QuicImageType::from_u32(le_u32(&buf[8..12]))?;
        let width = le_u32(&buf[12..16]);
        let height = le_u32(&buf[16..20]);
        Ok(Self {
            image_type,
            width,
            height,
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&QUIC_MAGIC.to_le_bytes());
        out.extend_from_slice(&QUIC_VERSION.to_le_bytes());
        out.extend_from_slice(&(self.image_type as u32).to_le_bytes());
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
    }
}

/// Bit-level reader over QUIC's body stream (sequence of LE 32-bit
/// words). Tracks a bit position into the byte stream and recomputes
/// the MSB-aligned 32-bit window on demand. This sidesteps the dual-
/// register dance the C reference does and trivially handles codewords
/// that span word boundaries (relevant: max codeword is 26 bits).
pub struct BitReader<'a> {
    bytes: &'a [u8],
    /// Byte offset of the first body word.
    body_start: usize,
    /// Total bits consumed from `bytes[body_start..]`.
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new_after_header(bytes: &'a [u8]) -> Result<Self> {
        if bytes.len() < QUIC_HEADER_SIZE {
            return Err(QuicError::Short {
                need: QUIC_HEADER_SIZE,
                have: bytes.len(),
            });
        }
        Ok(Self {
            bytes,
            body_start: QUIC_HEADER_SIZE,
            bit_pos: 0,
        })
    }

    fn read_word_at(&self, byte_offset: usize) -> u32 {
        if byte_offset + 4 <= self.bytes.len() {
            u32::from_le_bytes(self.bytes[byte_offset..byte_offset + 4].try_into().unwrap())
        } else {
            0
        }
    }

    /// Peek the next 32 bits, MSB-first. Bit 31 of the result is the
    /// next bit to be consumed.
    pub fn peek32(&self) -> u32 {
        let word_idx = self.bit_pos / 32;
        let bit_in_word = (self.bit_pos % 32) as u32;
        let off = self.body_start + word_idx * 4;
        let w0 = self.read_word_at(off);
        if bit_in_word == 0 {
            w0
        } else {
            let w1 = self.read_word_at(off + 4);
            (w0 << bit_in_word) | (w1 >> (32 - bit_in_word))
        }
    }

    /// Consume `len` bits (1..=32). The reader is now positioned to
    /// peek the bits AFTER the consumed range.
    pub fn eat(&mut self, len: u32) -> Result<()> {
        debug_assert!(len <= 32);
        let new_pos = self.bit_pos + len as usize;
        // Soft bounds-check: don't allow walking past the buffer end by
        // more than one word (decoder may peek past, but we shouldn't
        // *consume* past the buffer).
        let max_bits = (self.bytes.len() - self.body_start) * 8;
        if new_pos > max_bits + 32 {
            return Err(QuicError::Short {
                need: (new_pos - max_bits) / 8,
                have: 0,
            });
        }
        self.bit_pos = new_pos;
        Ok(())
    }

    /// Consume 32 bits.
    pub fn eat32(&mut self) -> Result<()> {
        self.eat(32)
    }
}

// ============================================================
// QUIC body decoder pieces — ports of `spice-common/common/quic.c`.
//
// Layered as:
//  - `Family`: lookup tables for 8bpc and 5bpc Golomb-Rice variants
//    built once at decoder construction time.
//  - `golomb_decode`: variable-length codeword extraction.
//  - `MelcState` / `decode_state_run`: MELCODE escape runs for long
//    zero residuals.
//  - `CommonState`: per-channel adaptive coding state (waitcnt, wmidx,
//    tabrand seed, MELCODE state).
//  - `Channel`: per-channel decoding context (bucket model, correlate
//    row buffer).
//
// The per-channel `uncompress_row0` / `uncompress_row` predictors that
// turn all of this into pixels are the next slice — see
// `spice-common/common/quic_tmpl.c` for the C reference.
// ============================================================

/// Number of distinct codes Family pre-computes lookup tables for.
const MAXNUMCODES: usize = 8;
/// MELCODE state-machine size.
const MELCSTATES: usize = 32;
/// Limit parameter passed by `family_init`.
const DEFMAXCLEN: u32 = 26;

/// `bppmask[i]` = `i` ones in the LSBs (matches the C const array).
pub(crate) const fn bppmask(i: u32) -> u32 {
    if i >= 32 {
        0xFFFF_FFFF
    } else {
        (1u32 << i) - 1
    }
}

/// MELCODE J table — exponent of `2^melclen` for each state.
pub(crate) const J_TABLE: [u8; MELCSTATES] = [
    0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 9, 10, 11, 12, 13,
    14, 15,
];

/// Lookup tables for one Golomb-Rice family (8 bits per channel for
/// RGB32/24/A8, 5 bits for RGB16). Built once via [`Family::init`].
#[derive(Debug)]
pub struct Family {
    pub bpc: u32,
    /// Number of unmodified GR codewords in code `l`.
    n_gr_codewords: [u32; MAXNUMCODES],
    /// Codeword length for the not-GR (escape) codeword in code `l`.
    not_gr_cwlen: [u32; MAXNUMCODES],
    /// Bitmask used by the decoder to tell GR vs not-GR codewords apart.
    not_gr_prefixmask: [u32; MAXNUMCODES],
    /// Suffix length of the not-GR codeword in code `l`.
    not_gr_suffixlen: [u32; MAXNUMCODES],
    /// Translation table for symbol distribution remapping (L → U).
    pub xlat_l2u: [u32; 256],
    /// Translation table U → L (used by encoder; kept for symmetry).
    pub xlat_u2l: [u8; 256],
}

impl Family {
    pub fn init(bpc: u32) -> Self {
        let mut f = Self {
            bpc,
            n_gr_codewords: [0; MAXNUMCODES],
            not_gr_cwlen: [0; MAXNUMCODES],
            not_gr_prefixmask: [0; MAXNUMCODES],
            not_gr_suffixlen: [0; MAXNUMCODES],
            xlat_l2u: [0; 256],
            xlat_u2l: [0; 256],
        };
        let limit = DEFMAXCLEN;
        for l in 0..bpc as usize {
            let mut altprefixlen = limit as i32 - bpc as i32;
            let cap = bppmask(bpc - l as u32) as i32;
            if altprefixlen > cap {
                altprefixlen = cap;
            }
            let altprefixlen = altprefixlen as u32;
            let altcodewords = bppmask(bpc) + 1 - (altprefixlen << l);
            f.n_gr_codewords[l] = altprefixlen << l;
            f.not_gr_suffixlen[l] = ceil_log2(altcodewords);
            f.not_gr_cwlen[l] = altprefixlen + f.not_gr_suffixlen[l];
            f.not_gr_prefixmask[l] = bppmask(32 - altprefixlen);
        }
        f.decorrelate_init();
        f.correlate_init();
        f
    }

    fn decorrelate_init(&mut self) {
        let pixelbitmask = bppmask(self.bpc);
        let pixelbitmaskshr = pixelbitmask >> 1;
        for s in 0..=pixelbitmask {
            let v = if s <= pixelbitmaskshr {
                s << 1
            } else {
                ((pixelbitmask - s) << 1) + 1
            };
            self.xlat_u2l[s as usize] = v as u8;
        }
    }

    fn correlate_init(&mut self) {
        let pixelbitmask = bppmask(self.bpc);
        for s in 0..=pixelbitmask {
            let v = if s & 1 != 0 {
                pixelbitmask - (s >> 1)
            } else {
                s >> 1
            };
            self.xlat_l2u[s as usize] = v;
        }
    }

    /// Decode one Golomb-Rice codeword from the top `bits` of the bit
    /// stream. Returns `(value, codeword_len)` so the caller can advance
    /// past it via `BitReader::eat`.
    pub fn golomb_decode(&self, l: u32, bits: u32) -> (u32, u32) {
        let l_idx = l as usize;
        if bits > self.not_gr_prefixmask[l_idx] {
            // Standard GR codeword: leading zeros + 1 + l-bit suffix.
            let zeroprefix = bits.leading_zeros();
            let cwlen = zeroprefix + 1 + l;
            let value = (zeroprefix << l) | ((bits >> (32 - cwlen)) & bppmask(l));
            (value, cwlen)
        } else {
            // Escape (not-GR): fixed cwlen, value is offset by nGRcodewords.
            let cwlen = self.not_gr_cwlen[l_idx];
            let value = self.n_gr_codewords[l_idx]
                + ((bits >> (32 - cwlen)) & bppmask(self.not_gr_suffixlen[l_idx]));
            (value, cwlen)
        }
    }
}

fn ceil_log2(val: u32) -> u32 {
    if val <= 1 {
        return 0;
    }
    32 - (val - 1).leading_zeros()
}

/// Per-channel bookkeeping shared by all rows.
#[derive(Debug, Clone)]
pub struct CommonState {
    pub waitcnt: u32,
    pub tabrand_seed: u32,
    pub wm_trigger: u32,
    pub wmidx: u32,
    pub wmileft: u32,
    pub melcstate: u8,
    pub melclen: u8,
    pub melcorder: u32,
}

impl Default for CommonState {
    fn default() -> Self {
        let mut s = Self {
            waitcnt: 0,
            tabrand_seed: TABRAND_SEEDMASK,
            wm_trigger: 0,
            wmidx: 0,
            wmileft: DEFWMINEXT,
            melcstate: 0,
            melclen: J_TABLE[0],
            melcorder: 1 << J_TABLE[0],
        };
        s.set_wm_trigger();
        s
    }
}

const DEFWMINEXT: u32 = 2048;
const DEFWMIMAX: u32 = 6;
const DEFEVOL: u32 = 3;

const BESTTRIGTAB: [[u16; 11]; 3] = [
    [550, 900, 800, 700, 500, 350, 300, 200, 180, 180, 160], // DEFevol = 1
    [110, 550, 900, 800, 550, 400, 350, 250, 140, 160, 140], // DEFevol = 3
    [100, 120, 550, 900, 700, 500, 400, 300, 220, 250, 160], // DEFevol = 5
];

impl CommonState {
    /// Update `wm_trigger` from the current `wmidx` per `besttrigtab`.
    pub fn set_wm_trigger(&mut self) {
        let mut wm = self.wmidx as usize;
        if wm > 10 {
            wm = 10;
        }
        self.wm_trigger = BESTTRIGTAB[(DEFEVOL / 2) as usize][wm] as u32;
    }

    /// Reset the MELCODE state to its initial config.
    pub fn reset_rle(&mut self) {
        self.melcstate = 0;
        self.melclen = J_TABLE[0];
        self.melcorder = 1 << self.melclen;
    }
}

const TABRAND_TABSIZE: usize = 256;
const TABRAND_SEEDMASK: u32 = 0x0FF;

/// Constant used by `tabrand` for waitmask randomization.
pub const TABRAND_CHAOS: [u32; TABRAND_TABSIZE] = [
    0x02c57542, 0x35427717, 0x2f5a2153, 0x9244f155, 0x7bd26d07, 0x354c6052, 0x57329b28, 0x2993868e,
    0x6cd8808c, 0x147b46e0, 0x99db66af, 0xe32b4cac, 0x1b671264, 0x9d433486, 0x62a4c192, 0x06089a4b,
    0x9e3dce44, 0xdaabee13, 0x222425ea, 0xa46f331d, 0xcd589250, 0x8bb81d7f, 0xc8b736b9, 0x35948d33,
    0xd7ac7fd0, 0x5fbe2803, 0x2cfbc105, 0x013dbc4e, 0x7a37820f, 0x39f88e9e, 0xedd58794, 0xc5076689,
    0xfcada5a4, 0x64c2f46d, 0xb3ba3243, 0x8974b4f9, 0x5a05aebd, 0x20afcd00, 0x39e2b008, 0x88a18a45,
    0x600bde29, 0xf3971ace, 0xf37b0a6b, 0x7041495b, 0x70b707ab, 0x06beffbb, 0x4206051f, 0xe13c4ee3,
    0xc1a78327, 0x91aa067c, 0x8295f72a, 0x732917a6, 0x1d871b4d, 0x4048f136, 0xf1840e7e, 0x6a6048c1,
    0x696cb71a, 0x7ff501c3, 0x0fc6310b, 0x57e0f83d, 0x8cc26e74, 0x11a525a2, 0x946934c7, 0x7cd888f0,
    0x8f9d8604, 0x4f86e73b, 0x04520316, 0xdeeea20c, 0xf1def496, 0x67687288, 0xf540c5b2, 0x22401484,
    0x3478658a, 0xc2385746, 0x01979c2c, 0x5dad73c8, 0x0321f58b, 0xf0fedbee, 0x92826ddf, 0x284bec73,
    0x5b1a1975, 0x03df1e11, 0x20963e01, 0xa17cf12b, 0x740d776e, 0xa7a6bf3c, 0x01b5cce4, 0x1118aa76,
    0xfc6fac0a, 0xce927e9b, 0x00bf2567, 0x806f216c, 0xbca69056, 0x795bd3e9, 0xc9dc4557, 0x8929b6c2,
    0x789d52ec, 0x3f3fbf40, 0xb9197368, 0xa38c15b5, 0xc3b44fa8, 0xca8333b0, 0xb7e8d590, 0xbe807feb,
    0xbf5f8360, 0xd99e2f5c, 0x372928e1, 0x7c757c4c, 0x0db5b154, 0xc01ede02, 0x1fc86e78, 0x1f3985be,
    0xb4805c77, 0x00c880fa, 0x974c1b12, 0x35ab0214, 0xb2dc840d, 0x5b00ae37, 0xd313b026, 0xb260969d,
    0x7f4c8879, 0x1734c4d3, 0x49068631, 0xb9f6a021, 0x6b863e6f, 0xcee5debf, 0x29f8c9fb, 0x53dd6880,
    0x72b61223, 0x1f67a9fd, 0x0a0f6993, 0x13e59119, 0x11cca12e, 0xfe6b6766, 0x16b6effc, 0x97918fc4,
    0xc2b8a563, 0x94f2f741, 0x0bfa8c9a, 0xd1537ae8, 0xc1da349c, 0x873c60ca, 0x95005b85, 0x9b5c080e,
    0xbc8abbd9, 0xe1eab1d2, 0x6dac9070, 0x4ea9ebf1, 0xe0cf30d4, 0x1ef5bd7b, 0xd161043e, 0x5d2fa2e2,
    0xff5d3cae, 0x86ed9f87, 0x2aa1daa1, 0xbd731a34, 0x9e8f4b22, 0xb1c2c67a, 0xc21758c9, 0xa182215d,
    0xccb01948, 0x8d168df7, 0x04238cfe, 0x368c3dbc, 0x0aeadca5, 0xbad21c24, 0x0a71fee5, 0x9fc5d872,
    0x54c152c6, 0xfc329483, 0x6783384a, 0xeddb3e1c, 0x65f90e30, 0x884ad098, 0xce81675a, 0x4b372f7d,
    0x68bf9a39, 0x43445f1e, 0x40f8d8cb, 0x90d5acb6, 0x4cd07282, 0x349eeb06, 0x0c9d5332, 0x520b24ef,
    0x80020447, 0x67976491, 0x2f931ca3, 0xfe9b0535, 0xfcd30220, 0x61a9e6cc, 0xa487d8d7, 0x3f7c5dd1,
    0x7d0127c5, 0x48f51d15, 0x60dea871, 0xc9a91cb7, 0x58b53bb3, 0x9d5e0b2d, 0x624a78b4, 0x30dbee1b,
    0x9bdf22e7, 0x1df5c299, 0x2d5643a7, 0xf4dd35ff, 0x03ca8fd6, 0x53b47ed8, 0x6f2c19aa, 0xfeb0c1f4,
    0x49e54438, 0x2f2577e6, 0xbf876969, 0x72440ea9, 0xfa0bafb8, 0x74f5b3a0, 0x7dd357cd, 0x89ce1358,
    0x6ef2cdda, 0x1e7767f3, 0xa6be9fdb, 0x4f5f88f8, 0xba994a3a, 0x08ca6b65, 0xe0893818, 0x9e00a16a,
    0xf42bfc8f, 0x9972eedc, 0x749c8b51, 0x32c05f5e, 0xd706805f, 0x6bfbb7cf, 0xd9210a10, 0x31a1db97,
    0x923a9559, 0x37a7a1f6, 0x059f8861, 0xca493e62, 0x65157e81, 0x8f6467dd, 0xab85ff9f, 0x9331aff2,
    0x8616b9f5, 0xedbd5695, 0xee7e29b1, 0x313ac44f, 0xb903112f, 0x432ef649, 0xdc0a36c0, 0x61cf2bba,
    0x81474925, 0xa8b6c7ad, 0xee5931de, 0xb2f8158d, 0x59fb7409, 0x2e3dfaed, 0x9af25a3f, 0xe1fed4d5,
];

/// Pseudo-random sequence used to vary `waitcnt`. Mirrors `tabrand` in
/// the C reference: increments the seed before indexing.
pub fn tabrand(seed: &mut u32) -> u32 {
    *seed = seed.wrapping_add(1);
    TABRAND_CHAOS[(*seed & TABRAND_SEEDMASK) as usize]
}

/// MELCODE state-run decoder. Consumes leading-ones runs from the bit
/// stream and returns the resulting accumulated `runlen`.
pub fn decode_state_run(reader: &mut BitReader<'_>, state: &mut CommonState) -> Result<u32> {
    let mut runlen = 0u32;
    loop {
        let inverted = !(reader.peek32() >> 24) as u8;
        // `u8::leading_zeros()` returns 0..=8 — the count of leading 1s
        // in the MSB byte (since we inverted before counting zeros).
        let temp = inverted.leading_zeros();
        for _ in 0..temp {
            runlen = runlen.wrapping_add(state.melcorder);
            if (state.melcstate as usize) < MELCSTATES - 1 {
                state.melcstate += 1;
                state.melclen = J_TABLE[state.melcstate as usize];
                state.melcorder = 1u32 << state.melclen;
            }
        }
        if temp != 8 {
            // Consume the leading 1s plus the terminating 0.
            reader.eat(temp + 1)?;
            break;
        }
        // All 8 bits were 1s; eat them and loop.
        reader.eat(8)?;
    }
    if state.melclen != 0 {
        let len = state.melclen as u32;
        runlen = runlen.wrapping_add(reader.peek32() >> (32 - len));
        reader.eat(len)?;
    }
    if state.melcstate > 0 {
        state.melcstate -= 1;
        state.melclen = J_TABLE[state.melcstate as usize];
        state.melcorder = 1u32 << state.melclen;
    }
    Ok(runlen)
}

// ============================================================
// Per-channel adaptive coding model + decompression
// ============================================================

/// One adaptive coding bucket: counters for each candidate code length
/// plus the current best (lowest-codelength) selection.
#[derive(Debug, Clone)]
struct Bucket {
    counters: [u32; MAXNUMCODES],
    bestcode: u8,
}

impl Bucket {
    fn new(bpc: u32) -> Self {
        Self {
            counters: [0; MAXNUMCODES],
            bestcode: (bpc - 1) as u8,
        }
    }
}

/// Per-channel decoder state — bucket model + working `correlate_row`
/// (previous row's residuals, with index `-1` valid via `prev_minus1`).
#[derive(Debug)]
struct ChannelDecoder {
    /// Buckets owned by this channel.
    buckets: Vec<Bucket>,
    /// `value (0..=255 for 8bpc) → index into `buckets`.
    bucket_ptrs: Vec<u16>,
    /// Residual row buffer, length `width`.
    correlate_row: Vec<u8>,
    /// Stand-in for the C `correlate_row[-1]` slot.
    prev_minus1: u8,
}

impl ChannelDecoder {
    fn new(bpc: u32, width: usize) -> Self {
        let levels = 1u32 << bpc;
        let (bucket_sizes, _total_pos) = build_bucket_sizes(levels);
        let mut buckets: Vec<Bucket> = bucket_sizes
            .iter()
            .map(|_| Bucket::new(bpc))
            .collect();
        let mut bucket_ptrs = vec![0u16; levels as usize];
        let mut pos = 0usize;
        for (i, &sz) in bucket_sizes.iter().enumerate() {
            for _ in 0..sz {
                bucket_ptrs[pos] = i as u16;
                pos += 1;
                if pos == levels as usize {
                    break;
                }
            }
        }
        // Fill any remaining slots with the last bucket.
        let last = (buckets.len() - 1) as u16;
        for slot in &mut bucket_ptrs[pos..] {
            *slot = last;
        }
        // Pre-zero counters (already zero from Bucket::new).
        let _ = &mut buckets;
        Self {
            buckets,
            bucket_ptrs,
            correlate_row: vec![0u8; width],
            prev_minus1: 0,
        }
    }

    fn find_bucket(&mut self, val: u8) -> &mut Bucket {
        let idx = self.bucket_ptrs[val as usize] as usize;
        &mut self.buckets[idx]
    }

    fn find_bucket_idx(&self, val: u8) -> usize {
        self.bucket_ptrs[val as usize] as usize
    }
}

/// Compute the bucket sizes for the `DEFEVOL=3` evolution path:
/// `1, 2, 4, 8, 16, 32, 64, 128`. Returns the size array and the total
/// number of value slots covered.
fn build_bucket_sizes(levels: u32) -> (Vec<u32>, u32) {
    let mut sizes = Vec::new();
    let mut bend = 0u32;
    let mut bsize = 1u32; // firstsize
    let mut repcntr = 1u32 + 1; // repfirst + 1
    loop {
        let bstart = if sizes.is_empty() { 0 } else { bend + 1 };
        repcntr -= 1;
        if repcntr == 0 {
            repcntr = 1; // repnext
            bsize *= 2; // mulsize
        }
        let mut new_bend = bstart + bsize - 1;
        if new_bend + bsize >= levels {
            new_bend = levels - 1;
        }
        bend = new_bend;
        let actual_size = bend - bstart + 1;
        sizes.push(actual_size);
        if bend >= levels - 1 {
            break;
        }
    }
    let total = sizes.iter().sum();
    (sizes, total)
}

/// Adaptive update of `bucket.bestcode` after observing residual `curval`.
/// Mirrors `update_model` in `quic_family_tmpl.c`.
fn update_model(family: &Family, state: &CommonState, bucket: &mut Bucket, curval: u8) {
    let bpc = family.bpc as usize;
    let mut bestcode = bpc - 1;
    bucket.counters[bestcode] = bucket.counters[bestcode].wrapping_add(
        family_golomb_code_len(family, curval, bestcode as u32),
    );
    let mut best_codelen = bucket.counters[bestcode];
    for i in (0..bpc - 1).rev() {
        bucket.counters[i] = bucket.counters[i].wrapping_add(
            family_golomb_code_len(family, curval, i as u32),
        );
        if bucket.counters[i] < best_codelen {
            bestcode = i;
            best_codelen = bucket.counters[i];
        }
    }
    bucket.bestcode = bestcode as u8;
    if best_codelen > state.wm_trigger {
        for c in &mut bucket.counters[..bpc] {
            *c >>= 1;
        }
    }
}

/// Compute the Golomb-Rice codeword length for symbol `n` at code `l`,
/// as `golomb_code_len[n][l]` would in the C reference (computed
/// on-the-fly to avoid 256×8 lookup tables).
fn family_golomb_code_len(family: &Family, n: u8, l: u32) -> u32 {
    if (n as u32) < family.n_gr_codewords[l as usize] {
        ((n as u32) >> l) + l + 1
    } else {
        family.not_gr_cwlen[l as usize]
    }
}

/// Decode one row of an RGB32-format image. `prev_row_bgra` is `None`
/// for the first row, otherwise the previous row's BGRA pixels (read
/// only, 4 bytes per pixel). Writes `width` BGRA pixels to `cur_bgra`.
#[allow(clippy::too_many_arguments)]
fn decompress_row_rgb32(
    reader: &mut BitReader<'_>,
    family: &Family,
    state: &mut CommonState,
    chan_r: &mut ChannelDecoder,
    chan_g: &mut ChannelDecoder,
    chan_b: &mut ChannelDecoder,
    prev_row_bgra: Option<&[u8]>,
    cur_bgra: &mut [u8],
    width: u32,
) -> Result<()> {
    let mask = bppmask(family.bpc);
    // Walk waitmask segments — each segment uses a different `waitmask`
    // value drawn from `bppmask[wmidx]`.
    let mut pos = 0u32;
    let mut remaining = width;
    while DEFWMIMAX > state.wmidx && state.wmileft <= remaining {
        if state.wmileft > 0 {
            let seg_end = pos + state.wmileft;
            decompress_seg_rgb32(
                reader,
                family,
                state,
                chan_r,
                chan_g,
                chan_b,
                prev_row_bgra,
                cur_bgra,
                pos,
                seg_end,
                mask,
            )?;
            pos += state.wmileft;
            remaining -= state.wmileft;
        }
        state.wmidx += 1;
        state.set_wm_trigger();
        state.wmileft = DEFWMINEXT;
    }
    if remaining > 0 {
        let seg_end = pos + remaining;
        decompress_seg_rgb32(
            reader,
            family,
            state,
            chan_r,
            chan_g,
            chan_b,
            prev_row_bgra,
            cur_bgra,
            pos,
            seg_end,
            mask,
        )?;
        if DEFWMIMAX > state.wmidx {
            state.wmileft = state.wmileft.wrapping_sub(remaining);
        }
    }
    Ok(())
}

/// Decode a single segment of pixels (`pos..end`). Handles both the
/// first-row (no `prev`) and subsequent-row (predicting from `prev` +
/// RLE escape via `decode_state_run`) cases.
#[allow(clippy::too_many_arguments)]
fn decompress_seg_rgb32(
    reader: &mut BitReader<'_>,
    family: &Family,
    state: &mut CommonState,
    chan_r: &mut ChannelDecoder,
    chan_g: &mut ChannelDecoder,
    chan_b: &mut ChannelDecoder,
    prev_row_bgra: Option<&[u8]>,
    cur_bgra: &mut [u8],
    start: u32,
    end: u32,
    mask: u32,
) -> Result<()> {
    debug_assert!(end > start);
    let waitmask = bppmask(state.wmidx);
    let mut i = start;
    let mut stopidx;
    // Sentinel: no recent run. Set to the last position run started at
    // each time we exit `do_run`.
    let mut run_index: i64 = -1;

    if i == 0 {
        decode_one_pixel_pos0(reader, family, chan_r, chan_g, chan_b, prev_row_bgra, cur_bgra)?;
        if state.waitcnt > 0 {
            state.waitcnt -= 1;
        } else {
            state.waitcnt = tabrand(&mut state.tabrand_seed) & waitmask;
            update_models_at(family, state, chan_r, chan_g, chan_b, 0);
        }
        i += 1;
        stopidx = i + state.waitcnt;
    } else {
        stopidx = i + state.waitcnt;
    }

    'outer: loop {
        while stopidx < end {
            let mut j = i;
            while j <= stopidx {
                if rle_match(prev_row_bgra, cur_bgra, j, run_index) {
                    state.waitcnt = stopidx.wrapping_sub(j);
                    run_index = j as i64;
                    let run_len = decode_state_run(reader, state)?;
                    let run_end = j + run_len;
                    if run_end > end {
                        return Err(QuicError::Short {
                            need: (run_end - end) as usize,
                            have: 0,
                        });
                    }
                    do_run_copy_rgb32(cur_bgra, j, run_end);
                    j = run_end;
                    i = j;
                    if i == end {
                        return Ok(());
                    }
                    stopidx = i + state.waitcnt;
                    continue 'outer;
                }
                decode_one_pixel(
                    reader, family, chan_r, chan_g, chan_b, prev_row_bgra, cur_bgra, j, mask,
                )?;
                j += 1;
            }
            update_models_at(family, state, chan_r, chan_g, chan_b, stopidx);
            i = stopidx + 1;
            stopidx = i + (tabrand(&mut state.tabrand_seed) & waitmask);
        }
        // Tail: i..end (no waitcnt update at the end of this segment).
        let mut j = i;
        while j < end {
            if rle_match(prev_row_bgra, cur_bgra, j, run_index) {
                state.waitcnt = stopidx.wrapping_sub(j);
                run_index = j as i64;
                let run_len = decode_state_run(reader, state)?;
                let run_end = j + run_len;
                if run_end > end {
                    return Err(QuicError::Short {
                        need: (run_end - end) as usize,
                        have: 0,
                    });
                }
                do_run_copy_rgb32(cur_bgra, j, run_end);
                j = run_end;
                i = j;
                if i == end {
                    return Ok(());
                }
                stopidx = i + state.waitcnt;
                continue 'outer;
            }
            decode_one_pixel(
                reader, family, chan_r, chan_g, chan_b, prev_row_bgra, cur_bgra, j, mask,
            )?;
            j += 1;
        }
        state.waitcnt = stopidx.wrapping_sub(end);
        return Ok(());
    }
}

/// `RLE_PRED_IMP` predicate: do we want to escape into a run-length
/// segment at position `j`? Requires:
/// - we have a prev row (RLE only kicks in past the first row),
/// - `j > 2`,
/// - we didn't just emerge from a run at this position,
/// - prev row's [j-1] and [j] have the same BGR pixel,
/// - cur row's [j-1] and [j-2] have the same BGR pixel.
fn rle_match(prev_row: Option<&[u8]>, cur: &[u8], j: u32, run_index: i64) -> bool {
    let Some(prev) = prev_row else {
        return false;
    };
    if j <= 2 {
        return false;
    }
    if run_index == j as i64 {
        return false;
    }
    let i = j as usize;
    same_bgr(prev, i - 1, prev, i) && same_bgr(cur, i - 1, cur, i - 2)
}

/// Whether the BGR (3-byte) prefixes of two pixels at given indices in
/// distinct buffers are equal.
#[inline]
fn same_bgr(a: &[u8], ai: usize, b: &[u8], bi: usize) -> bool {
    a[ai * 4] == b[bi * 4]
        && a[ai * 4 + 1] == b[bi * 4 + 1]
        && a[ai * 4 + 2] == b[bi * 4 + 2]
}

/// Copy `cur_bgra[k - 1]` into `cur_bgra[k]` for each `k` in
/// `start..end`, BGR + zero pad. Mirrors the inner `do_run` body.
fn do_run_copy_rgb32(cur_bgra: &mut [u8], start: u32, end: u32) {
    for k in start..end {
        let k_us = k as usize;
        let src = (k_us - 1) * 4;
        let dst = k_us * 4;
        cur_bgra[dst] = cur_bgra[src];
        cur_bgra[dst + 1] = cur_bgra[src + 1];
        cur_bgra[dst + 2] = cur_bgra[src + 2];
        cur_bgra[dst + 3] = 0;
    }
}

fn update_models_at(
    family: &Family,
    state: &CommonState,
    chan_r: &mut ChannelDecoder,
    chan_g: &mut ChannelDecoder,
    chan_b: &mut ChannelDecoder,
    idx: u32,
) {
    let i = idx as usize;
    let prev = i.wrapping_sub(1);
    for chan in [chan_r, chan_g, chan_b] {
        let bucket_val = if i == 0 {
            chan.prev_minus1
        } else {
            chan.correlate_row[prev]
        };
        let curval = chan.correlate_row[i];
        let bidx = chan.find_bucket_idx(bucket_val);
        update_model(family, state, &mut chan.buckets[bidx], curval);
    }
}

/// Decode one pixel at position 0 (no `cur_row[i-1]` available).
fn decode_one_pixel_pos0(
    reader: &mut BitReader<'_>,
    family: &Family,
    chan_r: &mut ChannelDecoder,
    chan_g: &mut ChannelDecoder,
    chan_b: &mut ChannelDecoder,
    prev_row_bgra: Option<&[u8]>,
    cur_bgra: &mut [u8],
) -> Result<()> {
    cur_bgra[3] = 0; // pad
    for (chan, dst_byte_offset) in [
        (chan_r, 2usize),
        (chan_g, 1usize),
        (chan_b, 0usize),
    ] {
        let prev_minus1 = chan.prev_minus1;
        let bucket = chan.find_bucket(prev_minus1);
        let l = bucket.bestcode as u32;
        let (residual, cwlen) = family.golomb_decode(l, reader.peek32());
        chan.correlate_row[0] = residual as u8;
        let pixel_val = if let Some(prev_row) = prev_row_bgra {
            // CORRELATE_0: family.xlatL2U[res] + GET_chan(prev_row[0])
            let prev_byte = prev_row[dst_byte_offset];
            ((family.xlat_l2u[residual as usize] + prev_byte as u32) & 0xFF) as u8
        } else {
            // First row, position 0: just the L→U-translated residual.
            family.xlat_l2u[residual as usize] as u8
        };
        cur_bgra[dst_byte_offset] = pixel_val;
        reader.eat(cwlen)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_one_pixel(
    reader: &mut BitReader<'_>,
    family: &Family,
    chan_r: &mut ChannelDecoder,
    chan_g: &mut ChannelDecoder,
    chan_b: &mut ChannelDecoder,
    prev_row_bgra: Option<&[u8]>,
    cur_bgra: &mut [u8],
    i: u32,
    mask: u32,
) -> Result<()> {
    let i_us = i as usize;
    let pos = i_us * 4;
    cur_bgra[pos + 3] = 0; // pad
    for (chan, dst_byte_offset) in [
        (chan_r, 2usize),
        (chan_g, 1usize),
        (chan_b, 0usize),
    ] {
        let bucket_val = chan.correlate_row[i_us - 1];
        let bucket = chan.find_bucket(bucket_val);
        let l = bucket.bestcode as u32;
        let (residual, cwlen) = family.golomb_decode(l, reader.peek32());
        chan.correlate_row[i_us] = residual as u8;
        let a = cur_bgra[(i_us - 1) * 4 + dst_byte_offset] as u32;
        let pixel_val = if let Some(prev_row) = prev_row_bgra {
            // CORRELATE: family.xlatL2U[res] + ((a + b) >> 1)
            let b = prev_row[pos + dst_byte_offset] as u32;
            ((family.xlat_l2u[residual as usize] + ((a + b) >> 1)) & mask) as u8
        } else {
            // CORRELATE_0: family.xlatL2U[res] + a
            ((family.xlat_l2u[residual as usize] + a) & mask) as u8
        };
        cur_bgra[pos + dst_byte_offset] = pixel_val;
        reader.eat(cwlen)?;
    }
    Ok(())
}

// ============================================================
// QUIC encoder — symmetric to the decoder, lets us round-trip-test
// the per-channel adaptive coder including the RLE branch.
// ============================================================

/// Bit-writer dual to [`BitReader`]. Bits accumulate into `io_word`
/// MSB-first; once 32 are pending the word is emitted as little-endian
/// to `buf`, matching the decoder's word-load convention.
pub struct BitWriter {
    buf: Vec<u8>,
    /// Bits accumulated so far (MSB-aligned).
    io_word: u32,
    /// LSB-side bits still free (32 = empty, 0 = ready to flush).
    available: u32,
}

impl Default for BitWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl BitWriter {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            io_word: 0,
            available: 32,
        }
    }

    /// Write `len` LSB-aligned bits of `word`.
    pub fn write(&mut self, word: u32, len: u32) {
        debug_assert!(len > 0 && len < 32);
        debug_assert!(word & !bppmask(len) == 0);
        if len < self.available {
            self.available -= len;
            self.io_word |= word << self.available;
            return;
        }
        let delta = len - self.available;
        self.io_word |= word >> delta;
        self.flush_word();
        self.available = 32 - delta;
        self.io_word = if delta == 0 {
            0
        } else {
            word.wrapping_shl(self.available)
        };
    }

    fn flush_word(&mut self) {
        self.buf.extend_from_slice(&self.io_word.to_le_bytes());
        self.io_word = 0;
        self.available = 32;
    }

    /// Finalise the stream and return the byte buffer.
    pub fn finish(mut self) -> Vec<u8> {
        if self.available != 32 {
            self.flush_word();
        }
        self.buf
    }

    /// Emit `n` consecutive 1-bits.
    pub fn write_ones(&mut self, n: u32) {
        let mut remaining = n;
        while remaining >= 31 {
            self.write(bppmask(31), 31);
            remaining -= 31;
        }
        if remaining > 0 {
            self.write(bppmask(remaining), remaining);
        }
    }
}

impl Family {
    /// Encode symbol `n` at code `l`. Returns `(codeword, len)`.
    pub fn golomb_encode(&self, n: u8, l: u32) -> (u32, u32) {
        let l_idx = l as usize;
        let nv = n as u32;
        if nv < self.n_gr_codewords[l_idx] {
            // Standard GR: `(n >> l)` zeros then a 1 then `l`-bit
            // suffix carrying `n & mask`.
            let codeword = (1u32 << l) | (nv & bppmask(l));
            let len = (nv >> l) + l + 1;
            (codeword, len)
        } else {
            // Escape: fixed `notGRcwlen[l]` bits encoding the value
            // offset by `nGRcodewords[l]`.
            let value = nv - self.n_gr_codewords[l_idx];
            (value, self.not_gr_cwlen[l_idx])
        }
    }
}

/// Mirror of [`decode_state_run`] for the encoder side.
pub fn encode_state_run(writer: &mut BitWriter, state: &mut CommonState, mut runlen: u32) {
    let mut hits = 0u32;
    while runlen >= state.melcorder {
        hits += 1;
        runlen -= state.melcorder;
        if (state.melcstate as usize) < MELCSTATES - 1 {
            state.melcstate += 1;
            state.melclen = J_TABLE[state.melcstate as usize];
            state.melcorder = 1u32 << state.melclen;
        }
    }
    writer.write_ones(hits);
    let trailing_len = (state.melclen as u32) + 1;
    writer.write(runlen, trailing_len);
    if state.melcstate > 0 {
        state.melcstate -= 1;
        state.melclen = J_TABLE[state.melcstate as usize];
        state.melcorder = 1u32 << state.melclen;
    }
}

/// Top-level entry: compress a `width × height` BGRA buffer (top-down,
/// alpha ignored) into a complete QUIC RGB32 stream including the
/// 5-word header.
pub fn compress_rgb32(pixels: &[u8], width: u32, height: u32) -> Vec<u8> {
    assert_eq!(pixels.len(), (width * height * 4) as usize);
    let mut header = Vec::with_capacity(QUIC_HEADER_SIZE);
    QuicHeader {
        image_type: QuicImageType::Rgb32,
        width,
        height,
    }
    .encode(&mut header);

    let family = Family::init(8);
    let mut state = CommonState::default();
    state.reset_rle();
    let mut chan_r = ChannelDecoder::new(8, width as usize);
    let mut chan_g = ChannelDecoder::new(8, width as usize);
    let mut chan_b = ChannelDecoder::new(8, width as usize);
    let mut writer = BitWriter::new();
    let stride = (width as usize) * 4;

    compress_row_rgb32(
        &mut writer,
        &family,
        &mut state,
        &mut chan_r,
        &mut chan_g,
        &mut chan_b,
        None,
        &pixels[..stride],
        width,
    );
    chan_r.prev_minus1 = chan_r.correlate_row[0];
    chan_g.prev_minus1 = chan_g.correlate_row[0];
    chan_b.prev_minus1 = chan_b.correlate_row[0];

    for row in 1..height as usize {
        let prev = &pixels[(row - 1) * stride..row * stride];
        let cur = &pixels[row * stride..(row + 1) * stride];
        compress_row_rgb32(
            &mut writer,
            &family,
            &mut state,
            &mut chan_r,
            &mut chan_g,
            &mut chan_b,
            Some(prev),
            cur,
            width,
        );
        chan_r.prev_minus1 = chan_r.correlate_row[0];
        chan_g.prev_minus1 = chan_g.correlate_row[0];
        chan_b.prev_minus1 = chan_b.correlate_row[0];
    }

    let body = writer.finish();
    header.extend(body);
    header
}

#[allow(clippy::too_many_arguments)]
fn compress_row_rgb32(
    writer: &mut BitWriter,
    family: &Family,
    state: &mut CommonState,
    chan_r: &mut ChannelDecoder,
    chan_g: &mut ChannelDecoder,
    chan_b: &mut ChannelDecoder,
    prev: Option<&[u8]>,
    cur: &[u8],
    width: u32,
) {
    let mask = bppmask(family.bpc);
    let mut pos = 0u32;
    let mut remaining = width;
    while DEFWMIMAX > state.wmidx && state.wmileft <= remaining {
        if state.wmileft > 0 {
            let end = pos + state.wmileft;
            compress_seg_rgb32(
                writer, family, state, chan_r, chan_g, chan_b, prev, cur, pos, end, mask,
            );
            pos += state.wmileft;
            remaining -= state.wmileft;
        }
        state.wmidx += 1;
        state.set_wm_trigger();
        state.wmileft = DEFWMINEXT;
    }
    if remaining > 0 {
        let end = pos + remaining;
        compress_seg_rgb32(
            writer, family, state, chan_r, chan_g, chan_b, prev, cur, pos, end, mask,
        );
        if DEFWMIMAX > state.wmidx {
            state.wmileft = state.wmileft.wrapping_sub(remaining);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn compress_seg_rgb32(
    writer: &mut BitWriter,
    family: &Family,
    state: &mut CommonState,
    chan_r: &mut ChannelDecoder,
    chan_g: &mut ChannelDecoder,
    chan_b: &mut ChannelDecoder,
    prev: Option<&[u8]>,
    cur: &[u8],
    start: u32,
    end: u32,
    mask: u32,
) {
    debug_assert!(end > start);
    let waitmask = bppmask(state.wmidx);
    let mut i = start;
    let mut stopidx;
    let mut run_index: i64 = -1;

    if i == 0 {
        encode_one_pixel_pos0(writer, family, chan_r, chan_g, chan_b, prev, cur);
        if state.waitcnt > 0 {
            state.waitcnt -= 1;
        } else {
            state.waitcnt = tabrand(&mut state.tabrand_seed) & waitmask;
            update_models_at(family, state, chan_r, chan_g, chan_b, 0);
        }
        i += 1;
        stopidx = i + state.waitcnt;
    } else {
        stopidx = i + state.waitcnt;
    }

    'outer: loop {
        while stopidx < end {
            let mut j = i;
            while j <= stopidx {
                if encode_rle_match(prev, cur, j, run_index) {
                    state.waitcnt = stopidx.wrapping_sub(j);
                    run_index = j as i64;
                    let mut run_size = 0u32;
                    while j < end && same_bgr(cur, j as usize, cur, j as usize - 1) {
                        run_size += 1;
                        j += 1;
                    }
                    encode_state_run(writer, state, run_size);
                    if j == end {
                        return;
                    }
                    i = j;
                    stopidx = i + state.waitcnt;
                    continue 'outer;
                }
                encode_one_pixel(writer, family, chan_r, chan_g, chan_b, prev, cur, j, mask);
                j += 1;
            }
            update_models_at(family, state, chan_r, chan_g, chan_b, stopidx);
            i = stopidx + 1;
            stopidx = i + (tabrand(&mut state.tabrand_seed) & waitmask);
        }
        let mut j = i;
        while j < end {
            if encode_rle_match(prev, cur, j, run_index) {
                state.waitcnt = stopidx.wrapping_sub(j);
                run_index = j as i64;
                let mut run_size = 0u32;
                while j < end && same_bgr(cur, j as usize, cur, j as usize - 1) {
                    run_size += 1;
                    j += 1;
                }
                encode_state_run(writer, state, run_size);
                if j == end {
                    return;
                }
                i = j;
                stopidx = i + state.waitcnt;
                continue 'outer;
            }
            encode_one_pixel(writer, family, chan_r, chan_g, chan_b, prev, cur, j, mask);
            j += 1;
        }
        state.waitcnt = stopidx.wrapping_sub(end);
        return;
    }
}

/// Encoder-side RLE predicate. Same shape as the decoder's
/// [`rle_match`]: only triggers past index 2, after we haven't just
/// emerged from a run, when prev row's neighbours match and current
/// row's previous two pixels match.
fn encode_rle_match(prev_row: Option<&[u8]>, cur: &[u8], j: u32, run_index: i64) -> bool {
    let Some(prev) = prev_row else {
        return false;
    };
    if j <= 2 {
        return false;
    }
    if run_index == j as i64 {
        return false;
    }
    let i = j as usize;
    same_bgr(prev, i - 1, prev, i) && same_bgr(cur, i - 1, cur, i - 2)
}

fn encode_one_pixel_pos0(
    writer: &mut BitWriter,
    family: &Family,
    chan_r: &mut ChannelDecoder,
    chan_g: &mut ChannelDecoder,
    chan_b: &mut ChannelDecoder,
    prev: Option<&[u8]>,
    cur: &[u8],
) {
    for (chan, dst_byte_offset) in [(chan_r, 2usize), (chan_g, 1usize), (chan_b, 0usize)] {
        let cur_val = cur[dst_byte_offset] as u32;
        let predicted = if let Some(p) = prev {
            p[dst_byte_offset] as u32
        } else {
            0 // first row, position 0: predictor is 0
        };
        let residual = family.xlat_u2l[((cur_val.wrapping_sub(predicted)) & 0xFF) as usize];
        chan.correlate_row[0] = residual;
        let prev_minus1 = chan.prev_minus1;
        let bucket = chan.find_bucket(prev_minus1);
        let l = bucket.bestcode as u32;
        let (codeword, len) = family.golomb_encode(residual, l);
        writer.write(codeword, len);
    }
}

#[allow(clippy::too_many_arguments)]
fn encode_one_pixel(
    writer: &mut BitWriter,
    family: &Family,
    chan_r: &mut ChannelDecoder,
    chan_g: &mut ChannelDecoder,
    chan_b: &mut ChannelDecoder,
    prev: Option<&[u8]>,
    cur: &[u8],
    i: u32,
    mask: u32,
) {
    let pos = (i as usize) * 4;
    for (chan, dst_byte_offset) in [(chan_r, 2usize), (chan_g, 1usize), (chan_b, 0usize)] {
        let cur_val = cur[pos + dst_byte_offset] as u32;
        let a = cur[pos - 4 + dst_byte_offset] as u32;
        let predicted = if let Some(p) = prev {
            let b = p[pos + dst_byte_offset] as u32;
            (a + b) >> 1
        } else {
            a
        };
        let residual = family.xlat_u2l[((cur_val.wrapping_sub(predicted)) & mask) as usize];
        chan.correlate_row[i as usize] = residual;
        let bucket_val = chan.correlate_row[i as usize - 1];
        let bucket = chan.find_bucket(bucket_val);
        let l = bucket.bestcode as u32;
        let (codeword, len) = family.golomb_encode(residual, l);
        writer.write(codeword, len);
    }
}

/// Decompress a QUIC RGBA stream into BGRA (top-down). RGBA is encoded
/// as two streams concatenated: first an RGB32 stream filling B/G/R
/// (alpha=0 from the RGB32 pass), then an alpha overlay stream that
/// overwrites byte 3 of every pixel using a separate adaptive coder.
pub fn decompress_rgba(stream: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let mut reader = BitReader::new_after_header(stream)?;
    let family = Family::init(8);

    // RGB phase shares one CommonState across the 3 colour channels.
    let mut rgb_state = CommonState::default();
    rgb_state.reset_rle();
    let mut chan_r = ChannelDecoder::new(8, width as usize);
    let mut chan_g = ChannelDecoder::new(8, width as usize);
    let mut chan_b = ChannelDecoder::new(8, width as usize);
    let stride = (width as usize) * 4;
    let mut out = vec![0u8; stride * height as usize];

    decompress_rgb_phase(
        &mut reader,
        &family,
        &mut rgb_state,
        &mut chan_r,
        &mut chan_g,
        &mut chan_b,
        &mut out,
        width,
        height,
    )?;

    // Alpha phase has its OWN CommonState and one ChannelDecoder.
    let mut alpha_state = CommonState::default();
    alpha_state.reset_rle();
    let mut chan_a = ChannelDecoder::new(8, width as usize);
    decompress_alpha_phase(
        &mut reader,
        &family,
        &mut alpha_state,
        &mut chan_a,
        &mut out,
        width,
        height,
    )?;

    Ok(out)
}

/// Internal: drive the RGB-only decoding phase over a pre-allocated
/// BGRA buffer. Same logic as [`decompress_rgb32`] but factored so
/// [`decompress_rgba`] can reuse it before running the alpha overlay.
#[allow(clippy::too_many_arguments)]
fn decompress_rgb_phase(
    reader: &mut BitReader<'_>,
    family: &Family,
    state: &mut CommonState,
    chan_r: &mut ChannelDecoder,
    chan_g: &mut ChannelDecoder,
    chan_b: &mut ChannelDecoder,
    out: &mut [u8],
    width: u32,
    height: u32,
) -> Result<()> {
    let stride = (width as usize) * 4;
    {
        let first = &mut out[..stride];
        decompress_row_rgb32(
            reader, family, state, chan_r, chan_g, chan_b, None, first, width,
        )?;
    }
    chan_r.prev_minus1 = chan_r.correlate_row[0];
    chan_g.prev_minus1 = chan_g.correlate_row[0];
    chan_b.prev_minus1 = chan_b.correlate_row[0];
    for row in 1..height as usize {
        let split_at = row * stride;
        let (prev_part, current_part) = out.split_at_mut(split_at);
        let prev_row = &prev_part[(row - 1) * stride..(row - 1) * stride + stride];
        let current_row = &mut current_part[..stride];
        decompress_row_rgb32(
            reader,
            family,
            state,
            chan_r,
            chan_g,
            chan_b,
            Some(prev_row),
            current_row,
            width,
        )?;
        chan_r.prev_minus1 = chan_r.correlate_row[0];
        chan_g.prev_minus1 = chan_g.correlate_row[0];
        chan_b.prev_minus1 = chan_b.correlate_row[0];
    }
    Ok(())
}

/// Run the alpha overlay phase: decodes a single 8bpc channel into byte
/// 3 of each pixel slot. The B/G/R bytes were already filled by
/// [`decompress_rgb_phase`] and are NOT modified here.
#[allow(clippy::too_many_arguments)]
fn decompress_alpha_phase(
    reader: &mut BitReader<'_>,
    family: &Family,
    state: &mut CommonState,
    chan: &mut ChannelDecoder,
    out: &mut [u8],
    width: u32,
    height: u32,
) -> Result<()> {
    let stride = (width as usize) * 4;
    {
        let first = &mut out[..stride];
        decompress_alpha_row(reader, family, state, chan, None, first, width)?;
    }
    chan.prev_minus1 = chan.correlate_row[0];
    for row in 1..height as usize {
        let split_at = row * stride;
        let (prev_part, current_part) = out.split_at_mut(split_at);
        let prev_row = &prev_part[(row - 1) * stride..(row - 1) * stride + stride];
        let current_row = &mut current_part[..stride];
        decompress_alpha_row(reader, family, state, chan, Some(prev_row), current_row, width)?;
        chan.prev_minus1 = chan.correlate_row[0];
    }
    Ok(())
}

fn decompress_alpha_row(
    reader: &mut BitReader<'_>,
    family: &Family,
    state: &mut CommonState,
    chan: &mut ChannelDecoder,
    prev_row_bgra: Option<&[u8]>,
    cur_bgra: &mut [u8],
    width: u32,
) -> Result<()> {
    let mask = bppmask(family.bpc);
    let mut pos = 0u32;
    let mut remaining = width;
    while DEFWMIMAX > state.wmidx && state.wmileft <= remaining {
        if state.wmileft > 0 {
            let seg_end = pos + state.wmileft;
            decompress_alpha_seg(
                reader,
                family,
                state,
                chan,
                prev_row_bgra,
                cur_bgra,
                pos,
                seg_end,
                mask,
            )?;
            pos += state.wmileft;
            remaining -= state.wmileft;
        }
        state.wmidx += 1;
        state.set_wm_trigger();
        state.wmileft = DEFWMINEXT;
    }
    if remaining > 0 {
        let seg_end = pos + remaining;
        decompress_alpha_seg(
            reader,
            family,
            state,
            chan,
            prev_row_bgra,
            cur_bgra,
            pos,
            seg_end,
            mask,
        )?;
        if DEFWMIMAX > state.wmidx {
            state.wmileft = state.wmileft.wrapping_sub(remaining);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decompress_alpha_seg(
    reader: &mut BitReader<'_>,
    family: &Family,
    state: &mut CommonState,
    chan: &mut ChannelDecoder,
    prev_row_bgra: Option<&[u8]>,
    cur_bgra: &mut [u8],
    start: u32,
    end: u32,
    mask: u32,
) -> Result<()> {
    debug_assert!(end > start);
    let waitmask = bppmask(state.wmidx);
    let mut i = start;
    let mut stopidx;

    if i == 0 {
        decode_alpha_pixel_pos0(reader, family, chan, prev_row_bgra, cur_bgra)?;
        if state.waitcnt > 0 {
            state.waitcnt -= 1;
        } else {
            state.waitcnt = tabrand(&mut state.tabrand_seed) & waitmask;
            update_alpha_model_at(family, state, chan, 0);
        }
        i += 1;
        stopidx = i + state.waitcnt;
    } else {
        stopidx = i + state.waitcnt;
    }

    loop {
        while stopidx < end {
            for j in i..=stopidx {
                decode_alpha_pixel(reader, family, chan, prev_row_bgra, cur_bgra, j, mask)?;
            }
            update_alpha_model_at(family, state, chan, stopidx);
            i = stopidx + 1;
            stopidx = i + (tabrand(&mut state.tabrand_seed) & waitmask);
        }
        for j in i..end {
            decode_alpha_pixel(reader, family, chan, prev_row_bgra, cur_bgra, j, mask)?;
        }
        state.waitcnt = stopidx.wrapping_sub(end);
        return Ok(());
    }
}

fn decode_alpha_pixel_pos0(
    reader: &mut BitReader<'_>,
    family: &Family,
    chan: &mut ChannelDecoder,
    prev_row_bgra: Option<&[u8]>,
    cur_bgra: &mut [u8],
) -> Result<()> {
    let prev_minus1 = chan.prev_minus1;
    let bucket = chan.find_bucket(prev_minus1);
    let l = bucket.bestcode as u32;
    let (residual, cwlen) = family.golomb_decode(l, reader.peek32());
    chan.correlate_row[0] = residual as u8;
    let alpha = if let Some(prev) = prev_row_bgra {
        let prev_a = prev[3];
        ((family.xlat_l2u[residual as usize] + prev_a as u32) & 0xFF) as u8
    } else {
        family.xlat_l2u[residual as usize] as u8
    };
    cur_bgra[3] = alpha;
    reader.eat(cwlen)?;
    Ok(())
}

fn decode_alpha_pixel(
    reader: &mut BitReader<'_>,
    family: &Family,
    chan: &mut ChannelDecoder,
    prev_row_bgra: Option<&[u8]>,
    cur_bgra: &mut [u8],
    i: u32,
    mask: u32,
) -> Result<()> {
    let i_us = i as usize;
    let pos = i_us * 4 + 3; // alpha byte
    let bucket_val = chan.correlate_row[i_us - 1];
    let bucket = chan.find_bucket(bucket_val);
    let l = bucket.bestcode as u32;
    let (residual, cwlen) = family.golomb_decode(l, reader.peek32());
    chan.correlate_row[i_us] = residual as u8;
    let a = cur_bgra[(i_us - 1) * 4 + 3] as u32;
    let alpha = if let Some(prev) = prev_row_bgra {
        let b = prev[pos] as u32;
        ((family.xlat_l2u[residual as usize] + ((a + b) >> 1)) & mask) as u8
    } else {
        ((family.xlat_l2u[residual as usize] + a) & mask) as u8
    };
    cur_bgra[pos] = alpha;
    reader.eat(cwlen)?;
    Ok(())
}

fn update_alpha_model_at(
    family: &Family,
    state: &CommonState,
    chan: &mut ChannelDecoder,
    idx: u32,
) {
    let i = idx as usize;
    let bucket_val = if i == 0 {
        chan.prev_minus1
    } else {
        chan.correlate_row[i - 1]
    };
    let curval = chan.correlate_row[i];
    let bidx = chan.find_bucket_idx(bucket_val);
    update_model(family, state, &mut chan.buckets[bidx], curval);
}

/// Decompress a QUIC `Gray` stream into BGRA (R=G=B=gray, A=0xFF).
///
/// Single 8bpc channel. The C reference uses `quic_one_uncompress_row*`
/// + `lz_one_pixel_t`; we decode straight into a grayscale `Vec<u8>`
/// and expand to BGRA at the end.
pub fn decompress_gray(stream: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let mut reader = BitReader::new_after_header(stream)?;
    let family = Family::init(8);
    let mut state = CommonState::default();
    state.reset_rle();
    let mut chan = ChannelDecoder::new(8, width as usize);
    let n = (width as usize) * (height as usize);
    let mut gray = vec![0u8; n];
    decompress_gray_phase(&mut reader, &family, &mut state, &mut chan, &mut gray, width, height)?;
    let mut out = Vec::with_capacity(n * 4);
    for &g in &gray {
        out.push(g); // B
        out.push(g); // G
        out.push(g); // R
        out.push(0xFF); // A
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn decompress_gray_phase(
    reader: &mut BitReader<'_>,
    family: &Family,
    state: &mut CommonState,
    chan: &mut ChannelDecoder,
    gray: &mut [u8],
    width: u32,
    height: u32,
) -> Result<()> {
    let stride = width as usize;
    {
        let first = &mut gray[..stride];
        decompress_gray_row(reader, family, state, chan, None, first, width)?;
    }
    chan.prev_minus1 = chan.correlate_row[0];
    for row in 1..height as usize {
        let split_at = row * stride;
        let (prev_part, current_part) = gray.split_at_mut(split_at);
        let prev_row = &prev_part[(row - 1) * stride..row * stride];
        let current_row = &mut current_part[..stride];
        decompress_gray_row(reader, family, state, chan, Some(prev_row), current_row, width)?;
        chan.prev_minus1 = chan.correlate_row[0];
    }
    Ok(())
}

fn decompress_gray_row(
    reader: &mut BitReader<'_>,
    family: &Family,
    state: &mut CommonState,
    chan: &mut ChannelDecoder,
    prev: Option<&[u8]>,
    cur: &mut [u8],
    width: u32,
) -> Result<()> {
    let mask = bppmask(family.bpc);
    let mut pos = 0u32;
    let mut remaining = width;
    while DEFWMIMAX > state.wmidx && state.wmileft <= remaining {
        if state.wmileft > 0 {
            let end = pos + state.wmileft;
            decompress_gray_seg(reader, family, state, chan, prev, cur, pos, end, mask)?;
            pos += state.wmileft;
            remaining -= state.wmileft;
        }
        state.wmidx += 1;
        state.set_wm_trigger();
        state.wmileft = DEFWMINEXT;
    }
    if remaining > 0 {
        let end = pos + remaining;
        decompress_gray_seg(reader, family, state, chan, prev, cur, pos, end, mask)?;
        if DEFWMIMAX > state.wmidx {
            state.wmileft = state.wmileft.wrapping_sub(remaining);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decompress_gray_seg(
    reader: &mut BitReader<'_>,
    family: &Family,
    state: &mut CommonState,
    chan: &mut ChannelDecoder,
    prev: Option<&[u8]>,
    cur: &mut [u8],
    start: u32,
    end: u32,
    mask: u32,
) -> Result<()> {
    let waitmask = bppmask(state.wmidx);
    let mut i = start;
    let mut stopidx;
    if i == 0 {
        let bucket = chan.find_bucket(chan.prev_minus1);
        let l = bucket.bestcode as u32;
        let (residual, cwlen) = family.golomb_decode(l, reader.peek32());
        chan.correlate_row[0] = residual as u8;
        let val = if let Some(p) = prev {
            ((family.xlat_l2u[residual as usize] + p[0] as u32) & mask) as u8
        } else {
            family.xlat_l2u[residual as usize] as u8
        };
        cur[0] = val;
        reader.eat(cwlen)?;
        if state.waitcnt > 0 {
            state.waitcnt -= 1;
        } else {
            state.waitcnt = tabrand(&mut state.tabrand_seed) & waitmask;
            update_alpha_model_at_byte(family, state, chan, 0);
        }
        i += 1;
        stopidx = i + state.waitcnt;
    } else {
        stopidx = i + state.waitcnt;
    }
    loop {
        while stopidx < end {
            for j in i..=stopidx {
                decode_gray_pixel(reader, family, chan, prev, cur, j, mask)?;
            }
            update_alpha_model_at_byte(family, state, chan, stopidx);
            i = stopidx + 1;
            stopidx = i + (tabrand(&mut state.tabrand_seed) & waitmask);
        }
        for j in i..end {
            decode_gray_pixel(reader, family, chan, prev, cur, j, mask)?;
        }
        state.waitcnt = stopidx.wrapping_sub(end);
        return Ok(());
    }
}

fn decode_gray_pixel(
    reader: &mut BitReader<'_>,
    family: &Family,
    chan: &mut ChannelDecoder,
    prev: Option<&[u8]>,
    cur: &mut [u8],
    i: u32,
    mask: u32,
) -> Result<()> {
    let i_us = i as usize;
    let bucket_val = chan.correlate_row[i_us - 1];
    let bucket = chan.find_bucket(bucket_val);
    let l = bucket.bestcode as u32;
    let (residual, cwlen) = family.golomb_decode(l, reader.peek32());
    chan.correlate_row[i_us] = residual as u8;
    let a = cur[i_us - 1] as u32;
    let val = if let Some(p) = prev {
        let b = p[i_us] as u32;
        ((family.xlat_l2u[residual as usize] + ((a + b) >> 1)) & mask) as u8
    } else {
        ((family.xlat_l2u[residual as usize] + a) & mask) as u8
    };
    cur[i_us] = val;
    reader.eat(cwlen)?;
    Ok(())
}

/// Re-use the alpha-pass model update logic for any single-channel
/// adaptive coder (Gray and Alpha both use the same shape).
fn update_alpha_model_at_byte(
    family: &Family,
    state: &CommonState,
    chan: &mut ChannelDecoder,
    idx: u32,
) {
    update_alpha_model_at(family, state, chan, idx);
}

/// Decompress a QUIC RGB16 stream into BGRA (top-down). Internal pixel
/// storage is RGB555 in a `Vec<u16>`; we expand each 5-bit channel to
/// 8-bit at the end via the standard mirror-low-bits upsample.
pub fn decompress_rgb16(stream: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let mut reader = BitReader::new_after_header(stream)?;
    let family = Family::init(5);
    let mut state = CommonState::default();
    state.reset_rle();
    let mut chan_r = ChannelDecoder::new(5, width as usize);
    let mut chan_g = ChannelDecoder::new(5, width as usize);
    let mut chan_b = ChannelDecoder::new(5, width as usize);
    let n = (width as usize) * (height as usize);
    let mut packed = vec![0u16; n];
    decompress_rgb16_phase(
        &mut reader,
        &family,
        &mut state,
        &mut chan_r,
        &mut chan_g,
        &mut chan_b,
        &mut packed,
        width,
        height,
    )?;
    let mut out = Vec::with_capacity(n * 4);
    for &p in &packed {
        let r5 = ((p >> 10) & 0x1f) as u32;
        let g5 = ((p >> 5) & 0x1f) as u32;
        let b5 = (p & 0x1f) as u32;
        out.push(((b5 << 3) | (b5 >> 2)) as u8);
        out.push(((g5 << 3) | (g5 >> 2)) as u8);
        out.push(((r5 << 3) | (r5 >> 2)) as u8);
        out.push(0xFF);
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn decompress_rgb16_phase(
    reader: &mut BitReader<'_>,
    family: &Family,
    state: &mut CommonState,
    chan_r: &mut ChannelDecoder,
    chan_g: &mut ChannelDecoder,
    chan_b: &mut ChannelDecoder,
    packed: &mut [u16],
    width: u32,
    height: u32,
) -> Result<()> {
    let stride = width as usize;
    {
        let first = &mut packed[..stride];
        decompress_rgb16_row(reader, family, state, chan_r, chan_g, chan_b, None, first, width)?;
    }
    chan_r.prev_minus1 = chan_r.correlate_row[0];
    chan_g.prev_minus1 = chan_g.correlate_row[0];
    chan_b.prev_minus1 = chan_b.correlate_row[0];
    for row in 1..height as usize {
        let split = row * stride;
        let (prev_part, cur_part) = packed.split_at_mut(split);
        let prev_row = &prev_part[(row - 1) * stride..row * stride];
        let cur_row = &mut cur_part[..stride];
        decompress_rgb16_row(
            reader, family, state, chan_r, chan_g, chan_b, Some(prev_row), cur_row, width,
        )?;
        chan_r.prev_minus1 = chan_r.correlate_row[0];
        chan_g.prev_minus1 = chan_g.correlate_row[0];
        chan_b.prev_minus1 = chan_b.correlate_row[0];
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decompress_rgb16_row(
    reader: &mut BitReader<'_>,
    family: &Family,
    state: &mut CommonState,
    chan_r: &mut ChannelDecoder,
    chan_g: &mut ChannelDecoder,
    chan_b: &mut ChannelDecoder,
    prev: Option<&[u16]>,
    cur: &mut [u16],
    width: u32,
) -> Result<()> {
    let mask = bppmask(family.bpc); // 0x1f
    let mut pos = 0u32;
    let mut remaining = width;
    while DEFWMIMAX > state.wmidx && state.wmileft <= remaining {
        if state.wmileft > 0 {
            let end = pos + state.wmileft;
            decompress_rgb16_seg(
                reader, family, state, chan_r, chan_g, chan_b, prev, cur, pos, end, mask,
            )?;
            pos += state.wmileft;
            remaining -= state.wmileft;
        }
        state.wmidx += 1;
        state.set_wm_trigger();
        state.wmileft = DEFWMINEXT;
    }
    if remaining > 0 {
        let end = pos + remaining;
        decompress_rgb16_seg(
            reader, family, state, chan_r, chan_g, chan_b, prev, cur, pos, end, mask,
        )?;
        if DEFWMIMAX > state.wmidx {
            state.wmileft = state.wmileft.wrapping_sub(remaining);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decompress_rgb16_seg(
    reader: &mut BitReader<'_>,
    family: &Family,
    state: &mut CommonState,
    chan_r: &mut ChannelDecoder,
    chan_g: &mut ChannelDecoder,
    chan_b: &mut ChannelDecoder,
    prev: Option<&[u16]>,
    cur: &mut [u16],
    start: u32,
    end: u32,
    mask: u32,
) -> Result<()> {
    let waitmask = bppmask(state.wmidx);
    let mut i = start;
    let mut stopidx;
    if i == 0 {
        decode_rgb16_pixel_pos0(reader, family, chan_r, chan_g, chan_b, prev, cur)?;
        if state.waitcnt > 0 {
            state.waitcnt -= 1;
        } else {
            state.waitcnt = tabrand(&mut state.tabrand_seed) & waitmask;
            update_models_at(family, state, chan_r, chan_g, chan_b, 0);
        }
        i += 1;
        stopidx = i + state.waitcnt;
    } else {
        stopidx = i + state.waitcnt;
    }
    loop {
        while stopidx < end {
            for j in i..=stopidx {
                decode_rgb16_pixel(reader, family, chan_r, chan_g, chan_b, prev, cur, j, mask)?;
            }
            update_models_at(family, state, chan_r, chan_g, chan_b, stopidx);
            i = stopidx + 1;
            stopidx = i + (tabrand(&mut state.tabrand_seed) & waitmask);
        }
        for j in i..end {
            decode_rgb16_pixel(reader, family, chan_r, chan_g, chan_b, prev, cur, j, mask)?;
        }
        state.waitcnt = stopidx.wrapping_sub(end);
        return Ok(());
    }
}

#[inline]
fn rgb16_get(p: u16, channel: u16) -> u32 {
    // channel 0 = B (bits 0-4), 1 = G (bits 5-9), 2 = R (bits 10-14).
    ((p >> (channel * 5)) & 0x1f) as u32
}
#[inline]
fn rgb16_set(p: u16, channel: u16, val: u32) -> u16 {
    let shift = channel * 5;
    (p & !(0x1f << shift)) | ((val as u16 & 0x1f) << shift)
}

fn decode_rgb16_pixel_pos0(
    reader: &mut BitReader<'_>,
    family: &Family,
    chan_r: &mut ChannelDecoder,
    chan_g: &mut ChannelDecoder,
    chan_b: &mut ChannelDecoder,
    prev: Option<&[u16]>,
    cur: &mut [u16],
) -> Result<()> {
    cur[0] = 0;
    // Decode in B (channel 0), G (channel 1), R (channel 2) order to
    // mirror the C `APPLY_ALL_COMP(R, G, B)` macro evaluation.
    for (chan, ch_idx) in [(chan_r, 2u16), (chan_g, 1u16), (chan_b, 0u16)] {
        let prev_minus1 = chan.prev_minus1;
        let bucket = chan.find_bucket(prev_minus1);
        let l = bucket.bestcode as u32;
        let (residual, cwlen) = family.golomb_decode(l, reader.peek32());
        chan.correlate_row[0] = residual as u8;
        let val = if let Some(p) = prev {
            (family.xlat_l2u[residual as usize] + rgb16_get(p[0], ch_idx)) & 0x1f
        } else {
            family.xlat_l2u[residual as usize] & 0x1f
        };
        cur[0] = rgb16_set(cur[0], ch_idx, val);
        reader.eat(cwlen)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_rgb16_pixel(
    reader: &mut BitReader<'_>,
    family: &Family,
    chan_r: &mut ChannelDecoder,
    chan_g: &mut ChannelDecoder,
    chan_b: &mut ChannelDecoder,
    prev: Option<&[u16]>,
    cur: &mut [u16],
    i: u32,
    mask: u32,
) -> Result<()> {
    let i_us = i as usize;
    cur[i_us] = 0;
    for (chan, ch_idx) in [(chan_r, 2u16), (chan_g, 1u16), (chan_b, 0u16)] {
        let bucket_val = chan.correlate_row[i_us - 1];
        let bucket = chan.find_bucket(bucket_val);
        let l = bucket.bestcode as u32;
        let (residual, cwlen) = family.golomb_decode(l, reader.peek32());
        chan.correlate_row[i_us] = residual as u8;
        let a = rgb16_get(cur[i_us - 1], ch_idx);
        let val = if let Some(p) = prev {
            let b = rgb16_get(p[i_us], ch_idx);
            (family.xlat_l2u[residual as usize] + ((a + b) >> 1)) & mask
        } else {
            (family.xlat_l2u[residual as usize] + a) & mask
        };
        cur[i_us] = rgb16_set(cur[i_us], ch_idx, val);
        reader.eat(cwlen)?;
    }
    Ok(())
}

/// Top-level entry: decode an RGB32 QUIC stream into a freshly
/// allocated BGRA buffer (top-down, `width * 4` bytes per row, alpha
/// always 0).
///
/// **Status:** the per-channel adaptive coder is in place — Family init,
/// Golomb-Rice, MELCODE state runs, the waitcnt/wmidx evolution. The
/// `RLE_PRED_IMP` branch (run-length escape via `decode_state_run` mid-
/// row) is NOT yet wired into the segment decoder, so streams that
/// actually trigger runs (large constant-colour regions like solid
/// backgrounds) will desynchronise. Verified end-to-end on a 1×1 black
/// pixel; richer round-trip coverage and the RLE branch are the next
/// iteration's work.
pub fn decompress_rgb32(stream: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let mut reader = BitReader::new_after_header(stream)?;
    let family = Family::init(8);
    let mut state = CommonState::default();
    state.reset_rle();
    let mut chan_r = ChannelDecoder::new(8, width as usize);
    let mut chan_g = ChannelDecoder::new(8, width as usize);
    let mut chan_b = ChannelDecoder::new(8, width as usize);
    let stride = (width as usize) * 4;
    let mut out = vec![0u8; stride * height as usize];
    decompress_rgb_phase(
        &mut reader,
        &family,
        &mut state,
        &mut chan_r,
        &mut chan_g,
        &mut chan_b,
        &mut out,
        width,
        height,
    )?;
    Ok(out)
}

#[cfg(test)]
mod body_tests {
    use super::*;

    #[test]
    fn family_8bpc_init_matches_reference_constants() {
        let f = Family::init(8);
        // n_gr_codewords sanity from `family_init`:
        // limit = 26, bpc = 8, l in 0..8.
        // For l=0: altprefixlen = min(26-8=18, bppmask(8)=255) = 18 → n=18*1=18.
        assert_eq!(f.n_gr_codewords[0], 18);
        // For l=1: altprefixlen = min(18, bppmask(7)=127) = 18 → n=18*2=36.
        assert_eq!(f.n_gr_codewords[1], 36);
        // For l=7: altprefixlen = min(18, bppmask(1)=1) = 1 → n=1*128=128.
        assert_eq!(f.n_gr_codewords[7], 128);
    }

    #[test]
    fn xlat_tables_are_inverses() {
        let f = Family::init(8);
        for s in 0..=255u32 {
            let mapped = f.xlat_u2l[s as usize] as u32;
            let back = f.xlat_l2u[mapped as usize];
            assert_eq!(back, s, "xlat round-trip broke at {s}");
        }
    }

    #[test]
    fn golomb_decode_round_trip_for_small_values() {
        // Encode `n` using the same formula as `golomb_coding_slow` and
        // verify our decoder recovers it. Use l=2, bpc=8 family.
        let f = Family::init(8);
        let l = 2u32;
        for n in 0u32..=20 {
            let (codeword, cwlen) = if n < f.n_gr_codewords[l as usize] {
                let cw = (1u32 << l) | (n & bppmask(l));
                let len = (n >> l) + l + 1;
                (cw, len)
            } else {
                let cw = n - f.n_gr_codewords[l as usize];
                (cw, f.not_gr_cwlen[l as usize])
            };
            // Pack `codeword` into the top `cwlen` bits of a u32.
            let bits = codeword << (32 - cwlen);
            let (value, decoded_len) = f.golomb_decode(l, bits);
            assert_eq!(value, n, "value mismatch at n={n}");
            assert_eq!(decoded_len, cwlen, "len mismatch at n={n}");
        }
    }

    #[test]
    fn common_state_initialises_wm_trigger_from_table() {
        let s = CommonState::default();
        // wmidx = 0 → row[0] of besttrigtab[1] (DEFEVOL=3 → /2=1) = 110.
        assert_eq!(s.wm_trigger, 110);
    }

    #[test]
    fn tabrand_advances_seed() {
        let mut seed = 0u32;
        let a = tabrand(&mut seed);
        let b = tabrand(&mut seed);
        assert_ne!(a, b);
        assert_eq!(seed, 2);
    }

    /// 1×1 Gray with single zero pixel — Golomb-Rice symbol 0 at l=7
    /// (8 bits = `10000000`). One codeword fills the upper byte of the
    /// first body word; output should be BGRA (0, 0, 0, 0xFF).
    #[test]
    fn decompress_gray_single_zero_pixel() {
        let h = QuicHeader {
            image_type: QuicImageType::Gray,
            width: 1,
            height: 1,
        };
        let mut buf = Vec::new();
        h.encode(&mut buf);
        buf.extend_from_slice(&0x80000000u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        let pixels = decompress_gray(&buf, 1, 1).unwrap();
        assert_eq!(pixels, vec![0, 0, 0, 0xFF]);
    }

    /// 1×1 RGB16 zero pixel: 5bpc family. Initial bestcode = bpc - 1 = 4,
    /// so each channel encodes symbol 0 at l=4. For l=4 in the 5bpc
    /// family: `n_gr_codewords[4]` = ?, but for symbol 0 the GR codeword
    /// is `(1 << 4) | 0` of length `(0 >> 4) + 4 + 1 = 5`. Three channels
    /// = 15 bits total. Bit pattern: 100001000010000... padded.
    #[test]
    fn decompress_rgb16_single_zero_pixel() {
        let h = QuicHeader {
            image_type: QuicImageType::Rgb16,
            width: 1,
            height: 1,
        };
        let mut buf = Vec::new();
        h.encode(&mut buf);
        // 3 codewords of 5 bits each, MSB-first: 10000 10000 10000 then
        // 17 zero bits → `10000_10000_10000_00000000000000000` =
        // 0x84_20_00_00.
        buf.extend_from_slice(&0x84200000u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        let pixels = decompress_rgb16(&buf, 1, 1).unwrap();
        // All RGB channels = 0, alpha = 0xFF.
        assert_eq!(pixels, vec![0, 0, 0, 0xFF]);
    }

    /// 1×1 RGBA with all-zero pixel: RGB phase encodes 3 codewords for
    /// symbol 0 at l=7 (8 bits each = 24 bits), then alpha phase encodes
    /// one codeword for symbol 0 at l=7 (8 bits). Total body = 32 bits =
    /// one full word.
    #[test]
    fn decompress_rgba_single_zero_pixel() {
        let h = QuicHeader {
            image_type: QuicImageType::Rgba,
            width: 1,
            height: 1,
        };
        let mut buf = Vec::new();
        h.encode(&mut buf);
        // Body word: 0b 10000000 10000000 10000000 10000000 = 0x80808080
        // (3 BGR codewords + 1 alpha codeword). Stored LE.
        buf.extend_from_slice(&0x80808080u32.to_le_bytes());
        // Padding so peek across the boundary returns zeros.
        buf.extend_from_slice(&0u32.to_le_bytes());
        let pixels = decompress_rgba(&buf, 1, 1).unwrap();
        // All BGR=0, A=0.
        assert_eq!(pixels, vec![0, 0, 0, 0]);
    }

    /// Hand-craft a QUIC stream encoding a single black 1×1 RGB32 pixel.
    /// For each of R, G, B we send a Golomb-Rice codeword for symbol 0
    /// at code l = 7 (initial bestcode = bpc - 1 = 7): codeword =
    /// `1 0000000` (8 bits). Three channels = 24 bits, then padding.
    #[test]
    fn decompress_rgb32_single_black_pixel() {
        let h = QuicHeader {
            image_type: QuicImageType::Rgb32,
            width: 1,
            height: 1,
        };
        let mut buf = Vec::new();
        h.encode(&mut buf);
        // Body: 0b 10000000 10000000 10000000 00000000 = 0x80808000.
        // Stored LE on disk → bytes [0x00, 0x80, 0x80, 0x80].
        buf.extend_from_slice(&0x80808000u32.to_le_bytes());
        // A second word so peek across the boundary returns zeros.
        buf.extend_from_slice(&0u32.to_le_bytes());

        let pixels = decompress_rgb32(&buf, 1, 1).unwrap();
        assert_eq!(pixels, vec![0, 0, 0, 0]);
    }

    /// Round-trip a deterministic synthetic image through the encoder
    /// and decoder. Validates the per-channel adaptive coder, the
    /// model evolution, the waitcnt logic, and the bit-stream framing
    /// end-to-end on a non-trivial pixel pattern.
    #[test]
    fn rgb32_round_trip_deterministic_pattern() {
        let width = 16u32;
        let height = 8u32;
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                pixels.push(((x * 13 + y * 7) & 0xff) as u8); // B
                pixels.push(((x * 11 + y * 23) & 0xff) as u8); // G
                pixels.push(((x * 17 + y * 31) & 0xff) as u8); // R
                pixels.push(0);
            }
        }
        let stream = compress_rgb32(&pixels, width, height);
        let decoded = decompress_rgb32(&stream, width, height).unwrap();
        assert_eq!(
            decoded, pixels,
            "encoder/decoder round-trip mismatch for pattern image"
        );
    }

    /// Solid-color image triggers the RLE branch (every pixel matches
    /// the previous one). This is the case the QUIC body decoder's
    /// `RLE_PRED_IMP` was structurally implemented for but never
    /// exercised end-to-end before — encoder/decoder paired test
    /// closes that gap.
    #[test]
    fn rgb32_round_trip_solid_color_exercises_rle() {
        let width = 32u32;
        let height = 16u32;
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            pixels.push(0x42); // B
            pixels.push(0x99); // G
            pixels.push(0x10); // R
            pixels.push(0);
        }
        let stream = compress_rgb32(&pixels, width, height);
        let decoded = decompress_rgb32(&stream, width, height).unwrap();
        assert_eq!(
            decoded, pixels,
            "RLE branch round-trip mismatch on solid-color image"
        );
    }

    /// Multi-row gradient — exercises the cross-row (a + b) / 2
    /// predictor.
    #[test]
    fn rgb32_round_trip_horizontal_and_vertical_gradient() {
        let width = 24u32;
        let height = 12u32;
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                pixels.push((x * 8) as u8); // B (horizontal gradient)
                pixels.push((y * 16) as u8); // G (vertical gradient)
                pixels.push(((x + y) * 5) as u8); // R (diagonal)
                pixels.push(0);
            }
        }
        let stream = compress_rgb32(&pixels, width, height);
        let decoded = decompress_rgb32(&stream, width, height).unwrap();
        assert_eq!(decoded, pixels, "gradient image round-trip mismatch");
    }

    #[test]
    fn decode_state_run_zero_run_returns_zero() {
        // First bit is 0 (no leading ones), melclen = 0 (initial state),
        // so result is 0.
        let h = QuicHeader {
            image_type: QuicImageType::Rgb32,
            width: 1,
            height: 1,
        };
        let mut buf = Vec::new();
        h.encode(&mut buf);
        buf.extend_from_slice(&0x0000_0000u32.to_be_bytes());
        buf.extend_from_slice(&0x0000_0000u32.to_be_bytes());
        let mut r = BitReader::new_after_header(&buf).unwrap();
        let mut s = CommonState::default();
        let runlen = decode_state_run(&mut r, &mut s).unwrap();
        assert_eq!(runlen, 0);
        // melcstate stays at 0 (was 0 → branch with `> 0` is skipped).
        assert_eq!(s.melcstate, 0);
    }
}


fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b.try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = QuicHeader {
            image_type: QuicImageType::Rgb32,
            width: 1280,
            height: 800,
        };
        let mut buf = Vec::new();
        h.encode(&mut buf);
        assert_eq!(buf.len(), QUIC_HEADER_SIZE);
        assert_eq!(QuicHeader::decode(&buf).unwrap(), h);
    }

    #[test]
    fn header_rejects_bad_magic() {
        let mut buf = vec![0u8; QUIC_HEADER_SIZE];
        buf[0..4].copy_from_slice(&0xdead_beefu32.to_be_bytes());
        assert!(matches!(
            QuicHeader::decode(&buf),
            Err(QuicError::BadMagic { .. })
        ));
    }

    #[test]
    fn bit_reader_eats_individual_bits() {
        // Build: header + one body word with known bit pattern.
        let h = QuicHeader {
            image_type: QuicImageType::Rgb32,
            width: 10,
            height: 10,
        };
        let mut buf = Vec::new();
        h.encode(&mut buf);
        // Body word: 0b10110000_00000000_00000000_00000000.
        buf.extend_from_slice(&0xB000_0000u32.to_le_bytes());
        // And a second word so refill succeeds at any boundary.
        buf.extend_from_slice(&0xCAFE_BABEu32.to_le_bytes());

        let mut r = BitReader::new_after_header(&buf).unwrap();
        // Top bit is 1.
        assert_eq!(r.peek32() >> 31, 1);
        r.eat(1).unwrap();
        // Next bit (0).
        assert_eq!(r.peek32() >> 31, 0);
        r.eat(1).unwrap();
        // Next is 1, then 1.
        assert_eq!(r.peek32() >> 31, 1);
    }

    #[test]
    fn bit_reader_eat32_advances_one_word() {
        let h = QuicHeader {
            image_type: QuicImageType::Rgb32,
            width: 1,
            height: 1,
        };
        let mut buf = Vec::new();
        h.encode(&mut buf);
        buf.extend_from_slice(&0xAAAA_AAAAu32.to_be_bytes());
        buf.extend_from_slice(&0x5555_5555u32.to_be_bytes());

        let mut r = BitReader::new_after_header(&buf).unwrap();
        assert_eq!(r.peek32(), 0xAAAA_AAAA);
        r.eat32().unwrap();
        assert_eq!(r.peek32(), 0x5555_5555);
    }
}

//! MJPEG frame decoder. Each `STREAM_DATA` payload from a stream whose
//! codec is `MJPEG` is a complete baseline JPEG; we hand it to
//! [`jpeg_decoder`] and reorder the resulting RGB(A) bytes into BGRA so
//! the rest of the client speaks one pixel format.

use jpeg_decoder::{Decoder, PixelFormat};

#[derive(Debug)]
pub(crate) struct DecodedFrame {
    pub width: u16,
    #[allow(dead_code)]
    pub height: u16,
    /// 32-bit BGRA, top-down, `width * 4` bytes per row.
    pub bgra: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum MjpegError {
    #[error("jpeg: {0}")]
    Jpeg(String),
    #[error("unsupported jpeg pixel format: {0:?}")]
    UnsupportedPixelFormat(PixelFormat),
    #[error("jpeg dimensions missing")]
    NoDimensions,
}

pub(crate) fn decode(jpeg: &[u8]) -> Result<DecodedFrame, MjpegError> {
    let mut decoder = Decoder::new(jpeg);
    let pixels = decoder.decode().map_err(|e| MjpegError::Jpeg(e.to_string()))?;
    let info = decoder.info().ok_or(MjpegError::NoDimensions)?;
    let (w, h) = (info.width, info.height);
    let n = (w as usize) * (h as usize);
    let bgra = match info.pixel_format {
        PixelFormat::RGB24 => {
            let mut out = vec![0u8; n * 4];
            for i in 0..n {
                out[i * 4] = pixels[i * 3 + 2]; // B
                out[i * 4 + 1] = pixels[i * 3 + 1]; // G
                out[i * 4 + 2] = pixels[i * 3]; // R
                out[i * 4 + 3] = 0xFF; // A (opaque)
            }
            out
        }
        PixelFormat::L8 => {
            let mut out = vec![0u8; n * 4];
            for i in 0..n {
                let g = pixels[i];
                out[i * 4] = g;
                out[i * 4 + 1] = g;
                out[i * 4 + 2] = g;
                out[i * 4 + 3] = 0xFF;
            }
            out
        }
        other => return Err(MjpegError::UnsupportedPixelFormat(other)),
    };
    Ok(DecodedFrame {
        width: w,
        height: h,
        bgra,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jpeg_encoder::{ColorType, Encoder as JEnc};

    fn encode_solid_rgb(width: u16, height: u16, r: u8, g: u8, b: u8) -> Vec<u8> {
        let mut rgb = Vec::with_capacity((width as usize) * (height as usize) * 3);
        for _ in 0..(width as usize * height as usize) {
            rgb.push(r);
            rgb.push(g);
            rgb.push(b);
        }
        let mut buf = Vec::new();
        let enc = JEnc::new(&mut buf, 95);
        enc.encode(&rgb, width, height, ColorType::Rgb).unwrap();
        buf
    }

    #[test]
    fn solid_rgb_jpeg_decodes_to_bgra() {
        let jpeg = encode_solid_rgb(8, 4, 200, 100, 50);
        let frame = decode(&jpeg).unwrap();
        assert_eq!(frame.width, 8);
        assert_eq!(frame.height, 4);
        assert_eq!(frame.bgra.len(), 8 * 4 * 4);
        // JPEG is lossy — colour will be close but not exact. Check that
        // each pixel is approximately the source colour and channel order
        // is BGRA, opaque.
        for px in frame.bgra.chunks_exact(4) {
            let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
            assert!((b as i32 - 50).abs() < 12, "B off: {b} vs 50");
            assert!((g as i32 - 100).abs() < 12, "G off: {g} vs 100");
            assert!((r as i32 - 200).abs() < 12, "R off: {r} vs 200");
            assert_eq!(a, 0xFF);
        }
    }
}

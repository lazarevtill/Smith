use crate::{error, Content, ErrorKind, Format, Limits, Result};
use image::{AnimationDecoder, ImageDecoder, ImageFormat, ImageReader};
use std::io::Cursor;

pub(crate) fn inspect(format: Format, bytes: &[u8], limits: &Limits) -> Result<Content> {
    let expected = match format {
        Format::Png => ImageFormat::Png,
        Format::Jpeg => ImageFormat::Jpeg,
        Format::Gif => ImageFormat::Gif,
        Format::Bmp => ImageFormat::Bmp,
        Format::WebP => ImageFormat::WebP,
        _ => unreachable!(),
    };
    if image::guess_format(bytes).ok() != Some(expected) {
        return Err(error(
            ErrorKind::MimeMismatch,
            "image signature does not match declared format",
        ));
    }
    check_container(format, bytes)?;
    let mut decode_limits = image::Limits::default();
    decode_limits.max_image_width = Some(limits.max_image_dimension);
    decode_limits.max_image_height = Some(limits.max_image_dimension);
    decode_limits.max_alloc = Some(limits.max_decoded_bytes);

    if format == Format::Gif {
        let expected_frames = inspect_gif_structure(bytes, limits)?;
        let mut decoder =
            image::codecs::gif::GifDecoder::new(Cursor::new(bytes)).map_err(decode_error)?;
        decoder.set_limits(decode_limits).map_err(decode_error)?;
        let (width, height) = decoder.dimensions();
        dimensions(width, height, limits)?;
        let pixels = u64::from(width) * u64::from(height);
        // GIF compositing needs multiple canvas buffers. Account for those explicitly.
        if pixels * 16 > limits.max_decoded_bytes {
            return Err(error(
                ErrorKind::LimitExceeded,
                "GIF compositing exceeds decoded byte limit",
            ));
        }
        let mut frames = 0;
        for frame in decoder.into_frames() {
            // The previous frame is dropped each iteration; all frames are validated.
            frame.map_err(decode_error)?;
            frames += 1;
            if frames > limits.max_frames || pixels * frames as u64 > limits.max_total_frame_pixels
            {
                return Err(error(
                    ErrorKind::LimitExceeded,
                    "animation exceeds frame budget",
                ));
            }
        }
        if frames == 0 {
            return Err(error(
                ErrorKind::InvalidContent,
                "GIF contains no image frames",
            ));
        }
        if frames != expected_frames {
            return Err(error(
                ErrorKind::InvalidContent,
                "GIF frame count disagrees with container",
            ));
        }
        return Ok(Content::Image {
            width,
            height,
            frames,
            requires_vision: true,
        });
    }

    if format == Format::WebP {
        inspect_webp_structure(bytes, limits)?;
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), expected);
    reader.limits(decode_limits);
    let decoder = reader.into_decoder().map_err(decode_error)?;
    let (width, height) = decoder.dimensions();
    dimensions(width, height, limits)?;
    if decoder.total_bytes() > limits.max_decoded_bytes {
        return Err(error(
            ErrorKind::LimitExceeded,
            "image exceeds decoded byte limit",
        ));
    }
    // Decoding the full raster catches damaged compressed data; a header probe is insufficient.
    image::DynamicImage::from_decoder(decoder).map_err(decode_error)?;
    Ok(Content::Image {
        width,
        height,
        frames: 1,
        requires_vision: true,
    })
}

// Check embedded bitstream dimensions before decoding. A small VP8X canvas must
// not conceal a huge VP8/VP8L frame that would allocate before a codec notices.
fn inspect_webp_structure(bytes: &[u8], limits: &Limits) -> Result<()> {
    let invalid = || {
        error(
            ErrorKind::InvalidContent,
            "malformed WebP chunk structure or dimensions",
        )
    };
    let animated = || {
        error(
            ErrorKind::UnsupportedFormat,
            "animated WebP is not supported",
        )
    };
    let mut pos = 12;
    let mut canvas = None;
    let mut raster = None;
    while pos < bytes.len() {
        let header = bytes.get(pos..pos + 8).ok_or_else(invalid)?;
        let size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let end = pos
            .checked_add(8)
            .and_then(|v| v.checked_add(size))
            .ok_or_else(invalid)?;
        let data = bytes.get(pos + 8..end).ok_or_else(invalid)?;
        pos = end.checked_add(size % 2).ok_or_else(invalid)?;
        if pos > bytes.len() {
            return Err(invalid());
        }
        let dims = match &header[..4] {
            b"ANIM" | b"ANMF" => return Err(animated()),
            b"VP8X" => {
                if data.len() != 10 || canvas.is_some() {
                    return Err(invalid());
                }
                if data[0] & 2 != 0 {
                    return Err(animated());
                }
                let w = 1 + u32::from_le_bytes([data[4], data[5], data[6], 0]);
                let h = 1 + u32::from_le_bytes([data[7], data[8], data[9], 0]);
                dimensions(w, h, limits)?;
                canvas = Some((w, h));
                continue;
            }
            b"VP8 " => {
                if data.len() < 10 || data[0] & 1 != 0 || data[3..6] != [0x9d, 1, 0x2a] {
                    return Err(invalid());
                }
                (
                    u32::from(u16::from_le_bytes([data[6], data[7]]) & 0x3fff),
                    u32::from(u16::from_le_bytes([data[8], data[9]]) & 0x3fff),
                )
            }
            b"VP8L" => {
                if data.len() < 5 || data[0] != 0x2f {
                    return Err(invalid());
                }
                let header = u32::from_le_bytes(data[1..5].try_into().unwrap());
                if header >> 29 != 0 {
                    return Err(invalid());
                }
                ((header & 0x3fff) + 1, ((header >> 14) & 0x3fff) + 1)
            }
            _ => continue,
        };
        dimensions(dims.0, dims.1, limits)?;
        if raster.replace(dims).is_some() {
            return Err(invalid());
        }
    }
    let raster = raster.ok_or_else(invalid)?;
    if canvas.is_some_and(|canvas| canvas != raster) {
        return Err(invalid());
    }
    Ok(())
}

// Inspect block boundaries and frame count before any frame decompression. This
// prevents even one excess frame from consuming a caller's animation budget.
fn inspect_gif_structure(bytes: &[u8], limits: &Limits) -> Result<usize> {
    let invalid = || error(ErrorKind::InvalidContent, "malformed GIF block structure");
    let header = bytes.get(..13).ok_or_else(invalid)?;
    let width = u16::from_le_bytes([header[6], header[7]]) as u32;
    let height = u16::from_le_bytes([header[8], header[9]]) as u32;
    dimensions(width, height, limits)?;
    let mut pos = 13;
    if header[10] & 0x80 != 0 {
        pos += 3 * (2usize << (header[10] & 7));
    }
    let mut frames = 0;
    loop {
        match bytes.get(pos).copied() {
            Some(0x3b) if pos + 1 == bytes.len() => return Ok(frames),
            Some(0x21) => {
                // Extension introducer + label, then length-prefixed sub-blocks.
                pos += 2;
                skip_gif_blocks(bytes, &mut pos)?;
            }
            Some(0x2c) => {
                let desc = bytes.get(pos..pos + 10).ok_or_else(invalid)?;
                let left = u16::from_le_bytes([desc[1], desc[2]]) as u32;
                let top = u16::from_le_bytes([desc[3], desc[4]]) as u32;
                let w = u16::from_le_bytes([desc[5], desc[6]]) as u32;
                let h = u16::from_le_bytes([desc[7], desc[8]]) as u32;
                if w == 0 || h == 0 || left + w > width || top + h > height {
                    return Err(invalid());
                }
                frames += 1;
                if frames > limits.max_frames
                    || u64::from(width) * u64::from(height) * frames as u64
                        > limits.max_total_frame_pixels
                {
                    return Err(error(
                        ErrorKind::LimitExceeded,
                        "animation exceeds frame budget",
                    ));
                }
                pos += 10;
                if desc[9] & 0x80 != 0 {
                    pos += 3 * (2usize << (desc[9] & 7));
                }
                pos += 1; // LZW minimum code size; actual code validation belongs to decoder.
                skip_gif_blocks(bytes, &mut pos)?;
            }
            _ => return Err(invalid()),
        }
    }
}
fn skip_gif_blocks(bytes: &[u8], pos: &mut usize) -> Result<()> {
    loop {
        let size = *bytes
            .get(*pos)
            .ok_or_else(|| error(ErrorKind::InvalidContent, "truncated GIF data block"))?
            as usize;
        *pos += size + 1;
        if *pos > bytes.len() {
            return Err(error(ErrorKind::InvalidContent, "truncated GIF data block"));
        }
        if size == 0 {
            return Ok(());
        }
    }
}

fn dimensions(width: u32, height: u32, limits: &Limits) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(error(
            ErrorKind::InvalidContent,
            "image has zero dimensions",
        ));
    }
    if width > limits.max_image_dimension
        || height > limits.max_image_dimension
        || u64::from(width) * u64::from(height) > limits.max_image_pixels
    {
        return Err(error(
            ErrorKind::LimitExceeded,
            "image exceeds dimension or pixel limit",
        ));
    }
    Ok(())
}

fn decode_error(e: image::ImageError) -> crate::Error {
    let kind = if matches!(e, image::ImageError::Limits(_)) {
        ErrorKind::LimitExceeded
    } else {
        ErrorKind::InvalidContent
    };
    error(kind, "image decoder rejected content or resource budget")
}

fn check_container(format: Format, bytes: &[u8]) -> Result<()> {
    let invalid = || {
        error(
            ErrorKind::InvalidContent,
            "truncated or malformed image container",
        )
    };
    match format {
        Format::Png => {
            let mut position = 8usize;
            let mut ended = false;
            while position < bytes.len() {
                let head = bytes.get(position..position + 8).ok_or_else(invalid)?;
                let len = u32::from_be_bytes(head[..4].try_into().unwrap()) as usize;
                let kind = &head[4..8];
                position = position
                    .checked_add(12)
                    .and_then(|n| n.checked_add(len))
                    .ok_or_else(invalid)?;
                if position > bytes.len() {
                    return Err(invalid());
                }
                if kind == b"acTL" {
                    return Err(error(
                        ErrorKind::UnsupportedFormat,
                        "animated PNG is not supported",
                    ));
                }
                if kind == b"IEND" {
                    if len != 0 || position != bytes.len() {
                        return Err(invalid());
                    }
                    ended = true;
                }
            }
            if !ended {
                return Err(invalid());
            }
        }
        Format::Jpeg if !bytes.ends_with(b"\xff\xd9") => return Err(invalid()),
        Format::Gif if !bytes.ends_with(b"\x3b") => return Err(invalid()),
        Format::WebP => {
            let size = bytes.get(4..8).ok_or_else(invalid)?;
            if u64::from(u32::from_le_bytes(size.try_into().unwrap())) + 8 != bytes.len() as u64 {
                return Err(invalid());
            }
        }
        Format::Bmp => {
            let size = bytes.get(2..6).ok_or_else(invalid)?;
            if u32::from_le_bytes(size.try_into().unwrap()) as usize != bytes.len() {
                return Err(invalid());
            }
        }
        _ => (),
    }
    Ok(())
}

//! Reproducible synthetic test images; never reads user input or accesses the network.
use image::{codecs::gif::GifEncoder, Frame, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
use std::{fs::File, path::PathBuf};

fn main() {
    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let rgb = RgbImage::from_fn(3, 2, |x, y| Rgb([(x * 90) as u8, (y * 160) as u8, 60]));
    for (name, format) in [
        ("pixels.png", ImageFormat::Png),
        ("pixels.jpg", ImageFormat::Jpeg),
        ("pixels.bmp", ImageFormat::Bmp),
        ("pixels.webp", ImageFormat::WebP),
    ] {
        rgb.save_with_format(out.join(name), format).unwrap();
    }
    let file = File::create(out.join("two-frames.gif")).unwrap();
    let mut encoder = GifEncoder::new(file);
    for color in [Rgba([255, 0, 0, 255]), Rgba([0, 0, 255, 255])] {
        encoder
            .encode_frame(Frame::new(RgbaImage::from_pixel(3, 2, color)))
            .unwrap();
    }
}

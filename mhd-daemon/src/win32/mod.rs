pub mod clipboard;
pub mod screen_capture;
pub mod text_host;

use crate::win32::screen_capture::CapturedImage;

/// Encode a captured image as in-memory PNG bytes.
///
/// Uses the `png` crate to encode RGBA pixel data.
pub fn encode_png(image: &CapturedImage) -> Result<Vec<u8>, png::EncodingError> {
    let mut buf = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut buf, image.width, image.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&image.rgba)?;
    }
    Ok(buf)
}

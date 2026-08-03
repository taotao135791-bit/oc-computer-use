//! Image helpers used by the driver: decode an encoded frame into a small
//! grayscale thumbnail. Thumbnails are the input to stale-frame detection and
//! the stabilizer, so this must be cheap and deterministic.

use cu_core::CuError;

/// Decode a PNG/JPEG and produce a grayscale thumbnail of `width`x`height`.
/// The image is letterboxed (aspect preserved, filled black) so all
/// thumbnails from one display share the same dimensions — a requirement of
/// [`cu_core::ScreenSnapshot::change_score`].
pub fn to_grayscale_thumbnail(bytes: &[u8], width: u32, height: u32) -> Result<Vec<u8>, CuError> {
    let img = image::load_from_memory(bytes)
        .map_err(|e| CuError::Driver(format!("cannot decode captured image: {e}")))?
        .to_rgb8();
    let resized = image::imageops::resize(
        &img,
        width,
        height,
        image::imageops::FilterType::Triangle,
    );
    // Convert RGB to grayscale (Rec. 601 luma).
    let mut out = Vec::with_capacity((width * height) as usize);
    for px in resized.pixels() {
        let luma = (0.299 * px[0] as f64 + 0.587 * px[1] as f64 + 0.114 * px[2] as f64)
            .round()
            .clamp(0.0, 255.0) as u8;
        out.push(luma);
    }
    Ok(out)
}

/// Decode an image to its dimensions without keeping the full buffer.
pub fn image_dimensions(bytes: &[u8]) -> Result<(u32, u32), CuError> {
    let img = image::load_from_memory(bytes)
        .map_err(|e| CuError::Driver(format!("cannot decode captured image: {e}")))?;
    Ok((img.width(), img.height()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_png(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(width, height, image::Rgb([128u8, 128, 128]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    #[test]
    fn thumbnail_has_expected_dims_and_range() {
        let png = make_test_png(100, 50);
        let thumb = to_grayscale_thumbnail(&png, 64, 64).unwrap();
        assert_eq!(thumb.len(), 64 * 64);
        assert!(thumb.iter().all(|&b| b <= 255));
        // Uniform gray image → uniform thumbnail.
        assert_eq!(thumb[0], 128);
    }

    #[test]
    fn corrupt_image_errors() {
        assert!(to_grayscale_thumbnail(b"not an image", 64, 64).is_err());
    }

    #[test]
    fn dimensions_round_trip() {
        let png = make_test_png(200, 100);
        let (w, h) = image_dimensions(&png).unwrap();
        assert_eq!((w, h), (200, 100));
    }
}

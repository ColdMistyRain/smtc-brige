use image::codecs::jpeg::JpegEncoder;
use std::io::Cursor;

// ── Cover Resize (cross-platform) ───────────────────────────────────────────

#[allow(dead_code)]
pub fn resize_cover_jpeg(buffer: &[u8], size: u32) -> Result<Vec<u8>, String> {
    log::debug!("resizing cover to {size}x{size} ({} bytes)", buffer.len());
    let img = image::load_from_memory(buffer).map_err(|e| format!("image decode: {e}"))?;
    let (w, h) = (img.width(), img.height());

    let scale = f64::max(size as f64 / w as f64, size as f64 / h as f64);
    let new_w = (w as f64 * scale).ceil() as u32;
    let new_h = (h as f64 * scale).ceil() as u32;

    let scaled = img.resize_exact(new_w, new_h, image::imageops::FilterType::Lanczos3);

    let x = ((size as i64 - new_w as i64) / 2).max(0) as u32;
    let y = ((size as i64 - new_h as i64) / 2).max(0) as u32;

    let mut canvas = image::DynamicImage::new_rgb8(size, size);
    image::imageops::overlay(&mut canvas, &scaled, x as i64, y as i64);

    let mut buf = Vec::new();
    let mut cursor = Cursor::new(&mut buf);
    let mut encoder = JpegEncoder::new_with_quality(&mut cursor, 90);
    encoder
        .encode(
            canvas.as_bytes(),
            canvas.width(),
            canvas.height(),
            canvas.color().into(),
        )
        .map_err(|e| format!("jpeg encode: {e}"))?;

    Ok(buf)
}

// ── SMTC API (platform-specific) ────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod win;
#[cfg(target_os = "windows")]
pub use win::{smtc_control, smtc_status_raw, smtc_thumbnail};

#[cfg(target_os = "linux")]
mod mpris;
#[cfg(target_os = "linux")]
pub use mpris::{smtc_control, smtc_status_raw, smtc_thumbnail};

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
mod noop;
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub use noop::{smtc_control, smtc_status_raw, smtc_thumbnail};

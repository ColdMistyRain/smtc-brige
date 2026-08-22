use image::codecs::jpeg::JpegEncoder;
use std::io::Cursor;

// ── 封面缩放（跨平台） ───────────────────────────────────────────

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

// ── SMTC API（平台相关） ────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod win;
#[cfg(target_os = "windows")]
pub use win::{smtc_control, smtc_status_raw, smtc_thumbnail};

// 本项目仅支持 Windows：SMTC 是 Windows 的系统媒体传输控件能力，
// 其他平台无法提供等价实现，直接编译报错（而不是运行期返回空桩）。
#[cfg(not(target_os = "windows"))]
compile_error!("smtc-brige 仅支持 Windows（SMTC 是 Windows 系统媒体传输控件）");

//! Screenshot region capture (Feature 3) — GDI `BitBlt` from the live
//! desktop DC. Kitty is Windows-only, so this stays a direct Win32 call
//! (matching this codebase's existing preference — see `hotkey.rs`'s
//! clipboard-image handling) rather than pulling in a dedicated
//! cross-platform screen-capture crate, whose abstraction would buy nothing
//! here.
//!
//! Two capture entry points, deliberately kept separate to avoid ever
//! shipping a multi-MB full-desktop image over the Rust->JS IPC boundary:
//! `capture_full_desktop_preview` produces a small, downsampled PNG purely
//! for the region-selection window's visual background, and `capture_region`
//! does a second, fresh, full-resolution, *targeted* `BitBlt` for the actual
//! final crop once the user has chosen a rectangle — the only thing that
//! ever crosses the boundary at full size is the already-cropped result,
//! bounded by whatever region the user selected.

use base64::engine::general_purpose;
use base64::Engine as _;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

/// The full virtual-desktop bounding rect (spans every monitor), in physical
/// pixels: `(x, y, width, height)`. `x`/`y` can be negative when a monitor
/// sits left of or above the primary — callers must pass these through
/// unchanged to `capture_pixels`/`capture_region`, never clamp to zero.
pub fn virtual_screen_rect() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

/// Capture a rectangle of the live desktop (physical pixels, virtual-screen
/// coordinates) as raw top-down BGRA bytes. The only function that actually
/// touches the Win32 capture APIs — kept as a thin, isolated seam so the
/// pixel/coordinate math in the public wrappers below stays unit-testable
/// without a real display.
fn capture_pixels(x: i32, y: i32, width: i32, height: i32) -> Result<Vec<u8>, String> {
    if width <= 0 || height <= 0 {
        return Err("capture region must have positive width and height".to_string());
    }
    unsafe {
        let screen_dc = GetDC(HWND(std::ptr::null_mut()));
        if screen_dc.is_invalid() {
            return Err("GetDC failed".to_string());
        }
        let mem_dc = CreateCompatibleDC(screen_dc);
        let bitmap = CreateCompatibleBitmap(screen_dc, width, height);
        if bitmap.is_invalid() {
            ReleaseDC(HWND(std::ptr::null_mut()), screen_dc);
            let _ = DeleteDC(mem_dc);
            return Err("CreateCompatibleBitmap failed".to_string());
        }
        let old_obj = SelectObject(mem_dc, bitmap);

        let blit_ok = BitBlt(mem_dc, 0, 0, width, height, screen_dc, x, y, SRCCOPY).is_ok();

        let mut pixels = Vec::new();
        if blit_ok {
            let mut bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height, // negative = top-down DIB (matches image::RgbaImage's row order)
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: 0, // BI_RGB
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut buf = vec![0u8; (width as usize) * (height as usize) * 4];
            let lines = GetDIBits(
                mem_dc,
                bitmap,
                0,
                height as u32,
                Some(buf.as_mut_ptr() as *mut _),
                &mut bmi,
                DIB_RGB_COLORS,
            );
            if lines != 0 {
                pixels = buf;
            }
        }

        SelectObject(mem_dc, old_obj);
        let _ = DeleteObject(bitmap);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(HWND(std::ptr::null_mut()), screen_dc);

        if pixels.is_empty() {
            return Err("screen capture failed".to_string());
        }
        Ok(pixels)
    }
}

/// BGRA (GDI's native order) -> RGBA data URL, reusing the exact PNG-encode
/// pattern `hotkey.rs`'s `encode_clipboard_image` already uses for the
/// clipboard-attach path.
fn bgra_to_png_data_url(mut pixels: Vec<u8>, width: u32, height: u32) -> Result<String, String> {
    for px in pixels.chunks_exact_mut(4) {
        px.swap(0, 2); // BGRA -> RGBA
    }
    let img = image::RgbaImage::from_raw(width, height, pixels)
        .ok_or_else(|| "captured pixel buffer size mismatch".to_string())?;
    let mut buf = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "data:image/png;base64,{}",
        general_purpose::STANDARD.encode(&buf)
    ))
}

/// Full-desktop capture, downsampled to at most `max_dimension` on its
/// longer side, for use as the selection window's background preview only
/// — never the final output. Returns the preview data URL alongside the
/// full virtual-screen rect (physical pixels) the selection window needs to
/// translate its own fractional click coordinates back into real screen
/// coordinates for the final `capture_region` call.
pub fn capture_full_desktop_preview(
    max_dimension: u32,
) -> Result<(String, i32, i32, i32, i32), String> {
    let (x, y, w, h) = virtual_screen_rect();
    let pixels = capture_pixels(x, y, w, h)?;
    let full = image::RgbaImage::from_raw(w as u32, h as u32, {
        let mut p = pixels;
        for px in p.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        p
    })
    .ok_or_else(|| "captured pixel buffer size mismatch".to_string())?;

    let (pw, ph) = if w as u32 > h as u32 {
        let scale = f64::from(max_dimension) / f64::from(w as u32);
        (max_dimension, ((h as f64) * scale).round() as u32)
    } else {
        let scale = f64::from(max_dimension) / f64::from(h as u32);
        (((w as f64) * scale).round() as u32, max_dimension)
    };
    let preview = image::imageops::resize(
        &full,
        pw.max(1),
        ph.max(1),
        image::imageops::FilterType::Triangle,
    );
    let mut buf = Vec::new();
    image::DynamicImage::ImageRgba8(preview)
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    let data_url = format!(
        "data:image/png;base64,{}",
        general_purpose::STANDARD.encode(&buf)
    );
    Ok((data_url, x, y, w, h))
}

/// Fresh, targeted, full-resolution capture of exactly the selected region
/// — the actual final output attached to the chat message.
pub fn capture_region(x: i32, y: i32, width: i32, height: i32) -> Result<String, String> {
    let pixels = capture_pixels(x, y, width, height)?;
    bgra_to_png_data_url(pixels, width as u32, height as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_to_png_data_url_rejects_mismatched_buffer_size() {
        let err = bgra_to_png_data_url(vec![0u8; 4], 10, 10).unwrap_err();
        assert!(err.contains("size mismatch"));
    }

    #[test]
    fn bgra_to_png_data_url_swaps_blue_and_red_channels() {
        // One BGRA pixel: blue=10, green=20, red=30, alpha=255 -> expect RGBA
        // in the encoded PNG to read red=30, green=20, blue=10.
        let bgra = vec![10u8, 20, 30, 255];
        let data_url = bgra_to_png_data_url(bgra, 1, 1).unwrap();
        assert!(data_url.starts_with("data:image/png;base64,"));
        let b64 = data_url.strip_prefix("data:image/png;base64,").unwrap();
        let png_bytes = general_purpose::STANDARD.decode(b64).unwrap();
        let img = image::load_from_memory(&png_bytes).unwrap().to_rgba8();
        let px = img.get_pixel(0, 0);
        assert_eq!(px.0, [30, 20, 10, 255]);
    }
}

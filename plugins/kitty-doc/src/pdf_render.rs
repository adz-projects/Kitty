//! PDF page rasterization for vision reading (phase 8): render pages to PNG
//! so a page with no usable text layer can be read by a vision-capable
//! provider. Pure Rust (hayro), so it cross-compiles to Android arm64 like
//! the rest of this crate — no native library to bundle.
//!
//! Each page is rendered inside `catch_unwind`: a malformed font or content
//! stream must fail that page, never abort the process (the same isolation
//! the text extractor uses). A failed page is reported, not silently dropped.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::{render_pdf, RenderSettings};

/// One rendered page, PNG-encoded, with its 1-based page number.
#[derive(Debug, Clone)]
pub struct RenderedPage {
    pub page: u32,
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// A page that could not be rendered (malformed content), kept so the caller
/// can report it rather than pretend the page is blank.
#[derive(Debug, Clone)]
pub struct RenderFailure {
    pub page: u32,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct RenderOutcome {
    pub pages: Vec<RenderedPage>,
    pub failures: Vec<RenderFailure>,
}

/// Render up to `max_pages` pages of a PDF file at `scale` (1.0 = 72 dpi).
pub fn render_pdf_pages(
    path: &Path,
    scale: f32,
    max_pages: usize,
) -> Result<RenderOutcome, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(render_pdf_bytes(&bytes, scale, max_pages))
}

/// Render from bytes. A PDF that will not parse at all yields an empty
/// outcome with a single failure rather than an error, so the caller treats
/// "unreadable" uniformly with "a page failed".
pub fn render_pdf_bytes(bytes: &[u8], scale: f32, max_pages: usize) -> RenderOutcome {
    let pdf = match Pdf::new(bytes.to_vec()) {
        Ok(pdf) => pdf,
        Err(e) => {
            return RenderOutcome {
                pages: Vec::new(),
                failures: vec![RenderFailure {
                    page: 0,
                    reason: format!("could not parse the PDF: {e:?}"),
                }],
            };
        }
    };
    let total = pdf.pages().len();
    let count = total.min(max_pages);
    let mut outcome = RenderOutcome::default();
    for idx in 0..count {
        let page_no = idx as u32 + 1;
        let rendered = catch_unwind(AssertUnwindSafe(|| {
            render_pdf(
                &pdf,
                scale,
                InterpreterSettings::default(),
                &RenderSettings::default(),
                Some(idx..=idx),
            )
        }));
        match rendered {
            Ok(Some(pixmaps)) if !pixmaps.is_empty() => {
                let pixmap = &pixmaps[0];
                match encode_png(pixmap) {
                    Ok((png, w, h)) => outcome.pages.push(RenderedPage {
                        page: page_no,
                        png,
                        width: w,
                        height: h,
                    }),
                    Err(e) => outcome.failures.push(RenderFailure {
                        page: page_no,
                        reason: e,
                    }),
                }
            }
            Ok(_) => outcome.failures.push(RenderFailure {
                page: page_no,
                reason: "the renderer produced no image".to_string(),
            }),
            Err(_) => outcome.failures.push(RenderFailure {
                page: page_no,
                reason: "the renderer panicked on this page".to_string(),
            }),
        }
    }
    outcome
}

/// Composite the premultiplied pixmap over white and encode as PNG. Vision
/// providers expect an opaque image; a transparent background would read as
/// black in many decoders.
fn encode_png(pixmap: &hayro::vello_cpu::Pixmap) -> Result<(Vec<u8>, u32, u32), String> {
    let w = pixmap.width() as u32;
    let h = pixmap.height() as u32;
    if w == 0 || h == 0 {
        return Err("the page rendered to zero size".to_string());
    }
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for px in pixmap.data() {
        // Premultiplied over white: out = premul + (255 - a).
        let inv = 255u8.saturating_sub(px.a);
        rgba.push(px.r.saturating_add(inv));
        rgba.push(px.g.saturating_add(inv));
        rgba.push(px.b.saturating_add(inv));
        rgba.push(255);
    }
    let img = image::RgbaImage::from_raw(w, h, rgba)
        .ok_or_else(|| "pixel buffer size mismatch".to_string())?;
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| format!("PNG encode failed: {e}"))?;
    Ok((png, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal hand-built one-page PDF with a text object. Enough for the
    /// parser and renderer to produce a non-empty image without any external
    /// fixture generator.
    fn tiny_pdf() -> Vec<u8> {
        let mut objects: Vec<String> = Vec::new();
        objects.push("<< /Type /Catalog /Pages 2 0 R >>".to_string());
        objects.push("<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string());
        objects.push(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_string(),
        );
        let stream = "BT /F1 24 Tf 20 100 Td (Hello) Tj ET";
        objects.push(format!(
            "<< /Length {} >>\nstream\n{}\nendstream",
            stream.len(),
            stream
        ));
        objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string());

        let mut out = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (i, obj) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.push_str(&format!("{} 0 obj\n{}\nendobj\n", i + 1, obj));
        }
        let xref_pos = out.len();
        out.push_str(&format!("xref\n0 {}\n", objects.len() + 1));
        out.push_str("0000000000 65535 f \n");
        for off in &offsets {
            out.push_str(&format!("{off:010} 00000 n \n"));
        }
        out.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF",
            objects.len() + 1,
            xref_pos
        ));
        out.into_bytes()
    }

    #[test]
    fn a_valid_pdf_renders_a_nonempty_png() {
        let outcome = render_pdf_bytes(&tiny_pdf(), 1.0, 10);
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert_eq!(outcome.pages.len(), 1);
        let page = &outcome.pages[0];
        assert_eq!(page.page, 1);
        assert!(page.width > 0 && page.height > 0);
        // PNG magic.
        assert_eq!(&page.png[..4], &[0x89, b'P', b'N', b'G']);
    }

    #[test]
    fn garbage_bytes_fail_gracefully_without_panicking() {
        let outcome = render_pdf_bytes(b"this is not a pdf at all", 1.0, 10);
        assert!(outcome.pages.is_empty());
        assert!(!outcome.failures.is_empty());
    }

    #[test]
    fn max_pages_caps_the_output() {
        let outcome = render_pdf_bytes(&tiny_pdf(), 1.0, 0);
        assert!(outcome.pages.is_empty());
    }
}

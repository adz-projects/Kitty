//! The PDF shapes that used to come back blank — or get refused outright —
//! from `lean_pdf_read_text`, and led models to conclude that a text PDF was
//! "just an image".
//!
//! `pdf.rs`'s own fixture is PyMuPDF's built-in Helvetica with a WinAnsi
//! encoding, the one shape lopdf 0.34's `extract_text` always handled. Real
//! documents are not built like that. Each fixture here is one of the ways a
//! real one differs, and each used to lose its text:
//!
//! - an embedded TrueType font (Type0 / Identity-H + ToUnicode), which is how
//!   Word, Chrome and PyMuPDF all embed a real font
//! - a page whose content is a Form XObject, which lopdf never entered
//! - text shown with the `'` operator, which lopdf ignored
//! - an owner-password-only PDF, which opens in every viewer without a prompt
//!   and was refused as "password protected"
//!
//! plus a genuinely image-only page, which must be *reported* as having no
//! text layer rather than passed back as a silent blank page.

use std::path::{Path, PathBuf};
use std::process::Command;

use kitty_tools::tools::pdf::pdf_read_text;
use serde_json::Value;

fn run_python(script: &str) {
    let output = Command::new("python")
        .arg("-c")
        .arg(script)
        .output()
        .expect("failed to run python");
    assert!(
        output.status.success(),
        "python script failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kitty-tools-pdf-rw-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn read(path: &Path) -> Value {
    serde_json::from_str(&pdf_read_text(&path.to_string_lossy(), None, None, None, 0))
        .expect("tool output was not valid JSON")
}

fn all_text(v: &Value) -> String {
    v["data"]
        .as_array()
        .unwrap_or_else(|| panic!("no data array: {v}"))
        .iter()
        .map(|p| p.as_str().unwrap().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A font file every Windows install has. The fixtures that need an embedded
/// TrueType font skip, loudly, where it is absent rather than failing on a
/// machine that simply lacks it.
fn arial() -> Option<&'static str> {
    let p = r"C:\Windows\Fonts\arial.ttf";
    Path::new(p).exists().then_some(p)
}

#[test]
fn embedded_truetype_font_is_read() {
    let Some(font) = arial() else {
        eprintln!("skipping: no arial.ttf");
        return;
    };
    let path = tmp_dir("ttf").join("ttf.pdf");
    run_python(&format!(
        r#"
import fitz
doc = fitz.open()
page = doc.new_page()
page.insert_font(fontname="F0", fontfile=r"{font}")
page.insert_text((72, 72), "Quarterly revenue grew by eleven percent", fontname="F0")
doc.save(r"{}")
"#,
        path.display()
    ));
    let v = read(&path);
    assert_eq!(v["status"], "success", "{v}");
    let text = all_text(&v);
    assert!(text.contains("Quarterly revenue"), "got: {text}");
}

#[test]
fn text_inside_a_form_xobject_is_read() {
    let path = tmp_dir("form").join("form.pdf");
    run_python(&format!(
        r#"
import fitz
src = fitz.open()
src.new_page().insert_text((72, 72), "Wrapped inside a form xobject")
doc = fitz.open()
page = doc.new_page()
# show_pdf_page places the source page as a Form XObject drawn with `Do`:
# the page's own content stream then holds no text operators at all.
page.show_pdf_page(page.rect, src, 0)
doc.save(r"{}")
"#,
        path.display()
    ));
    let v = read(&path);
    assert_eq!(v["status"], "success", "{v}");
    let text = all_text(&v);
    assert!(text.contains("form xobject"), "got: {text}");
}

#[test]
fn quote_operator_lines_are_read() {
    let path = tmp_dir("quote").join("quote.pdf");
    run_python(&format!(
        r#"
import fitz
doc = fitz.open()
page = doc.new_page()
# Registers the helv font resource on the page, then the content stream is
# replaced with one that shows its second line through the `'` operator.
page.insert_text((72, 72), "x", fontname="helv")
xref = page.get_contents()[0]
doc.update_stream(xref, b"BT /helv 12 Tf 14 TL 72 700 Td (First line shown) Tj (Second line quoted) ' ET")
doc.save(r"{}")
"#,
        path.display()
    ));
    let v = read(&path);
    assert_eq!(v["status"], "success", "{v}");
    let text = all_text(&v);
    assert!(text.contains("First line"), "got: {text}");
    assert!(text.contains("Second line quoted"), "got: {text}");
}

/// Python that rewrites a MuPDF-encrypted file so its `/Encrypt` dictionary
/// is an indirect object. MuPDF writes it inline in the trailer; Acrobat,
/// iText, Word and qpdf write it indirect, which is the shape real restricted
/// PDFs arrive in (lopdf cannot open the inline form at all — see
/// `extract_pages`). Offsets of existing objects are untouched: the new object
/// is appended after them and the xref table regenerated.
const INDIRECT_ENCRYPT_PY: &str = r#"
import re
def indirect_encrypt(path):
    data = open(path, 'rb').read()
    xref_at = data.rindex(b'\nxref\n') + 1
    body, tail = data[:xref_at], data[xref_at:]
    t = tail.index(b'trailer')
    k = tail.index(b'/Encrypt', t) + len(b'/Encrypt')
    depth, i = 0, k
    while True:
        if tail[i:i+2] == b'<<': depth += 1; i += 2; continue
        if tail[i:i+1] == b'<':
            # A hex string: skip it whole, or its closing `>` pairs with the
            # dictionary's and reads as a `>>`.
            i = tail.index(b'>', i) + 1; continue
        if tail[i:i+2] == b'>>':
            depth -= 1; i += 2
            if depth == 0: break
            continue
        i += 1
    enc = tail[k:i]
    size = int(re.search(rb'/Size (\d+)', tail).group(1))
    entries = re.findall(rb'\d{10} \d{5} [fn] ?\r?\n', tail[:t])
    obj = b'%d 0 obj\n' % size + enc + b'\nendobj\n'
    new_off = len(body)
    body += obj
    xref = b'xref\n0 %d\n' % (size + 1) + b''.join(entries) + b'%010d 00000 n \n' % new_off
    trailer = tail[t:tail.index(b'startxref')]
    trailer = trailer.replace(b'/Encrypt' + enc, b'/Encrypt %d 0 R' % size)
    trailer = re.sub(rb'/Size \d+', b'/Size %d' % (size + 1), trailer)
    out = body + xref + trailer + b'startxref\n%d\n%%%%EOF\n' % len(body)
    open(path, 'wb').write(out)
"#;

#[test]
fn owner_password_only_pdf_is_readable() {
    let path = tmp_dir("owner").join("owner.pdf");
    run_python(&format!(
        r#"{INDIRECT_ENCRYPT_PY}
import fitz
doc = fitz.open()
doc.new_page().insert_text((72, 72), "Restricted but readable statement")
doc.save(r"{p}", encryption=fitz.PDF_ENCRYPT_AES_128, owner_pw="owner", user_pw="",
         permissions=fitz.PDF_PERM_PRINT)
indirect_encrypt(r"{p}")
"#,
        p = path.display()
    ));
    let v = read(&path);
    assert_eq!(v["status"], "success", "{v}");
    let text = all_text(&v);
    assert!(text.contains("Restricted but readable"), "got: {text}");
}

#[test]
fn a_user_password_pdf_is_still_refused() {
    let path = tmp_dir("user").join("user.pdf");
    run_python(&format!(
        r#"{INDIRECT_ENCRYPT_PY}
import fitz
doc = fitz.open()
doc.new_page().insert_text((72, 72), "Secret")
doc.save(r"{p}", encryption=fitz.PDF_ENCRYPT_AES_128, owner_pw="owner", user_pw="user")
indirect_encrypt(r"{p}")
"#,
        p = path.display()
    ));
    let v = read(&path);
    assert_eq!(v["error_code"], "PDF_ENCRYPTED", "{v}");
}

/// MuPDF's own layout — `/Encrypt` inline in the trailer — which lopdf loads
/// as a document with no objects. It must be reported as encrypted, never
/// returned as a successful, zero-page, empty read.
#[test]
fn an_inline_encrypt_dictionary_is_reported_not_read_as_empty() {
    let path = tmp_dir("inline").join("inline.pdf");
    run_python(&format!(
        r#"
import fitz
doc = fitz.open()
doc.new_page().insert_text((72, 72), "Secret")
doc.save(r"{}", encryption=fitz.PDF_ENCRYPT_AES_128, owner_pw="owner", user_pw="user")
"#,
        path.display()
    ));
    let v = read(&path);
    assert_eq!(v["error_code"], "PDF_ENCRYPTED", "{v}");
}

#[test]
fn an_image_only_page_is_reported_not_left_blank() {
    let path = tmp_dir("img").join("img.pdf");
    run_python(&format!(
        r#"
import fitz
doc = fitz.open()
doc.new_page().insert_text((72, 72), "A real text page")
pix = fitz.Pixmap(fitz.csRGB, fitz.IRect(0, 0, 64, 64), False)
pix.clear_with(200)
doc.new_page().insert_image(fitz.Rect(72, 72, 400, 400), pixmap=pix)
doc.save(r"{}")
"#,
        path.display()
    ));
    let v = read(&path);
    assert_eq!(v["status"], "success", "{v}");
    assert_eq!(
        v["metadata"]["pages_image_only"],
        serde_json::json!([2]),
        "{v}"
    );
    assert!(
        v["metadata"].get("pages_failed").is_none(),
        "an image page is not a failure: {v}"
    );
    let msg = v["message"].as_str().unwrap_or_default();
    assert!(msg.contains("no text layer"), "the model must be told: {v}");
    assert!(all_text(&v).contains("A real text page"));
}

/// The shape of a PDF a user actually has: printed from a browser, with a
/// system font subset-embedded and ligatures shaped by the layout engine.
/// Skipped where Edge is not installed.
#[test]
fn browser_printed_pdf_is_read() {
    let edge = r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe";
    if !Path::new(edge).exists() {
        eprintln!("skipping: no Edge");
        return;
    }
    let dir = tmp_dir("edge");
    let html = dir.join("in.html");
    let pdf = dir.join("out.pdf");
    std::fs::write(
        &html,
        "<html><body style=\"font-family:Calibri,'Segoe UI',sans-serif;font-size:14pt\">\
         <h1>Office efficiency findings</h1>\
         <p>The finance office filed its final figures on the fifth.</p>\
         </body></html>",
    )
    .unwrap();
    let status = Command::new(edge)
        .args([
            "--headless",
            "--disable-gpu",
            "--no-pdf-header-footer",
            &format!("--user-data-dir={}", dir.join("profile").display()),
            &format!("--print-to-pdf={}", pdf.display()),
        ])
        .arg(&html)
        .status()
        .expect("failed to run Edge");
    assert!(
        status.success() && pdf.exists(),
        "Edge did not produce a PDF"
    );

    let v = read(&pdf);
    assert_eq!(v["status"], "success", "{v}");
    let text = all_text(&v);
    assert!(text.contains("efficiency"), "got: {text}");
    assert!(text.contains("final figures"), "got: {text}");
}

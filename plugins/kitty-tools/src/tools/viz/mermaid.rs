//! `generate_accessible_mermaid` payload builder.
//!
//! Mermaid has no viable server-side Rust renderer, so the Mermaid.js runtime
//! is vendored (see `assets/mermaid.LICENSE`) and inlined into a standalone
//! HTML document; the sandboxed iframe parses and renders the DSL at display
//! time. That means we inherit Mermaid's own (uncontrolled) layout quality —
//! "foolproof" here therefore means *guaranteed degradation, never a blank
//! frame*: the server rejects empty/oversized sources up front, and any
//! parse/render error at display time swaps in a visible error card alongside
//! the raw source, so a diagram is never silently lost.

use crate::envelope::error_response;

use super::{success_payload, wrap_in_standalone_html};

const MERMAID_JS: &str = include_str!("assets/mermaid.min.js");
const MAX_SOURCE_CHARS: usize = 12_000;

/// Embeds `s` as a JSON string literal that cannot break the surrounding inline
/// `<script>` element: JSON-encode it, then escape `/` after `<` so the HTML
/// parser never encounters a literal `</script>` sequence inside the string.
fn js_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into()).replace("</", "<\\/")
}

pub fn generate_accessible_mermaid(mermaid: &str, title: &str, description: &str) -> String {
    if mermaid.trim().is_empty() {
        return error_response(
            "VIZ_EMPTY_MERMAID",
            "No Mermaid source was provided.",
            None,
            Some("Provide a non-empty Mermaid `mermaid` string, e.g. \"flowchart TD\\nA-->B\"."),
        );
    }
    if mermaid.chars().count() > MAX_SOURCE_CHARS {
        return error_response(
            "VIZ_MERMAID_TOO_LARGE",
            &format!("The Mermaid source is {} characters; at most {MAX_SOURCE_CHARS} are allowed.", mermaid.chars().count()),
            None,
            Some("Split the diagram into smaller pieces, or simplify it."),
        );
    }

    let body = build_body(mermaid, title, description);
    let standalone = wrap_in_standalone_html(title, &body);
    success_payload(title, &standalone, &[])
}

fn build_body(source: &str, title: &str, description: &str) -> String {
    let source_lit = js_string(source);
    let title_lit = js_string(title);
    let desc_lit = js_string(description);
    format!(
        r#"<style>
.mermaid-error {{ background:#fafafa; border:1px solid #e4e4e7; border-radius:8px; padding:12px 14px; font-family:system-ui,sans-serif; }}
.mermaid-error-msg {{ color:#b91c1c; font-size:12px; margin:6px 0; font-family:ui-monospace,monospace; white-space:pre-wrap; }}
.mermaid-raw {{ background:#fff; border:1px solid #e4e4e7; border-radius:6px; padding:10px; font-size:11px; font-family:ui-monospace,monospace; white-space:pre-wrap; max-height:260px; overflow:auto; }}
</style>
<div id="mermaid-host" style="min-height:120px; padding:8px 0;"></div>
<script>
{merr}
</script>
<script>
(function () {{
  var SOURCE = {source};
  var TITLE = {title};
  var DESC = {desc};
  var host = document.getElementById('mermaid-host');
  function bump() {{ if (typeof sendHeight === 'function') sendHeight(); }}
  if (typeof mermaid === 'undefined') {{
    host.innerHTML = '<div class="mermaid-error"><strong>Mermaid runtime missing.</strong></div>';
    bump();
    return;
  }}
  mermaid.initialize({{
    startOnLoad: false,
    securityLevel: 'strict',
    flowchart: {{ useMaxWidth: true }},
    accessibility: {{ title: TITLE, description: DESC }}
  }});
  function showError(msg) {{
    host.innerHTML = '';
    var card = document.createElement('div');
    card.className = 'mermaid-error';
    var strong = document.createElement('strong');
    strong.textContent = 'Mermaid could not render this diagram.';
    var err = document.createElement('div');
    err.className = 'mermaid-error-msg';
    err.textContent = msg;
    var pre = document.createElement('pre');
    pre.className = 'mermaid-raw';
    pre.textContent = SOURCE;
    card.appendChild(strong); card.appendChild(err); card.appendChild(pre);
    host.appendChild(card);
    bump();
  }}
  mermaid.render('mermaid-graph-' + Date.now(), SOURCE).then(function (res) {{
    host.innerHTML = res.svg;
    bump();
  }}).catch(function (e) {{
    showError(String((e && e.message) || e));
  }});
}})();
</script>"#,
        merr = MERMAID_JS,
        source = source_lit,
        title = title_lit,
        desc = desc_lit,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn empty_source_is_rejected() {
        let v: Value = serde_json::from_str(&generate_accessible_mermaid("   ", "T", "D")).unwrap();
        assert_eq!(v["status"], "error");
        assert_eq!(v["error_code"], "VIZ_EMPTY_MERMAID");
        assert!(v["hint"].is_string());
    }

    #[test]
    fn oversized_source_is_rejected() {
        let big = "flowchart TD\n".to_string() + &"x".repeat(MAX_SOURCE_CHARS + 10);
        let v: Value = serde_json::from_str(&generate_accessible_mermaid(&big, "T", "D")).unwrap();
        assert_eq!(v["error_code"], "VIZ_MERMAID_TOO_LARGE");
    }

    #[test]
    fn success_payload_is_an_iframe_render() {
        let out = generate_accessible_mermaid("flowchart TD\nA-->B", "Flow", "A goes to B");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "success");
        assert_eq!(v["render_config"]["target"], "iframe");
        let html = v["html_payload"].as_str().unwrap();
        assert!(html.contains("mermaid.initialize"));
        assert!(html.contains(r#"securityLevel: 'strict'"#));
        assert!(html.contains("useMaxWidth"));
        assert!(html.contains("mermaid-host"));
        assert!(html.contains("mermaid-graph-"), "must call mermaid.render");
    }

    #[test]
    fn hostile_source_cannot_break_out_of_the_inline_script() {
        // A source containing "</script>" must be escaped (`<\/script>`) so the
        // html_payload's inline <script> element cannot be terminated early and
        // re-injected as markup.
        let src = "flowchart TD\nA[\"</script><img src=x onerror=alert(1)>\"]";
        let out = generate_accessible_mermaid(src, "T", "D");
        let v: Value = serde_json::from_str(&out).unwrap();
        let html = v["html_payload"].as_str().unwrap();
        // The literal sequence must not appear in a script context; it must be
        // escaped as `<\/script>` inside the JS string.
        assert!(html.contains("<\\/script>"), "source's `</script>` must be escaped");
        assert!(!html.contains("</script><img"), "raw breakout sequence must not survive");
    }

    #[test]
    fn title_and_description_are_passed_to_mermaid_accessibility() {
        let out = generate_accessible_mermaid("pie\nshowData\n\"A\":1", "My pie", "A single-slice pie.");
        let html: Value = serde_json::from_str(&out).unwrap();
        let h = html["html_payload"].as_str().unwrap();
        assert!(h.contains("accessibility"));
        assert!(h.contains(r#""My pie""#));
        assert!(h.contains(r#""A single-slice pie.""#));
    }
}
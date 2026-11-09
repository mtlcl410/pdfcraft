//! `page_render` reports the renderer's warnings next to the PNG, so a headless caller can tell
//! that a picture is incomplete or approximate, as `pdfcraft-cli render` can (#4340).

use std::path::PathBuf;

use pdfcraft_automation::{Automation, Content};
use serde_json::{Value, json};

/// The warning a colour that falls back gives, worded as `pdfcraft-cli render` prints it.
const FALLBACK: &str = "a colour that could not be computed was painted in a fallback colour";

/// A one-page 200 × 200 pt PDF that draws `content`, with the colour space `CS1` defined as
/// `[/DeviceN [/Spot1 /Spot2] /DeviceRGB 5 0 R]` and object 5 a calculator function `program`.
fn devicen_pdf(content: &str, program: &str) -> Vec<u8> {
    format!(
        "%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n\
         2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n\
         3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /ColorSpace << /CS1 [/DeviceN [/Spot1 /Spot2] /DeviceRGB 5 0 R] >> >> /Contents 4 0 R >> endobj\n\
         4 0 obj << /Length {} >> stream\n{content}\nendstream endobj\n\
         5 0 obj << /FunctionType 4 /Domain [0 1 0 1] /Range [0 1 0 1 0 1] /Length {} >> stream\n{program}\nendstream endobj\n\
         trailer << /Root 1 0 R >>\n%%EOF",
        content.len(),
        program.len()
    )
    .into_bytes()
}

/// A page whose colour falls back: the tint transform pops more than it is given, so it can't be
/// evaluated (the renderer's own test of the warning uses the same function).
fn fallback_pdf() -> Vec<u8> {
    devicen_pdf("/CS1 cs 0.5 0.5 scn 0 0 100 100 re f", "{ pop pop pop }")
}

/// The same page with a tint transform that works: nothing to warn about.
fn clean_pdf() -> Vec<u8> {
    devicen_pdf("/CS1 cs 0.5 0.5 scn 0 0 100 100 re f", "{ pop pop 0 0 0 }")
}

/// A fresh folder holding `fallback.pdf` and `clean.pdf`, and an automation rooted there.
fn setup(test: &str) -> (PathBuf, Automation) {
    let dir = std::env::temp_dir().join(format!("pdfcraft-render-warnings-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("fallback.pdf"), fallback_pdf()).unwrap();
    std::fs::write(dir.join("clean.pdf"), clean_pdf()).unwrap();
    let a = Automation::new().with_root(&dir).unwrap();
    (dir, a)
}

fn open(a: &mut Automation, path: &str) -> u64 {
    match a.call("doc_open", &json!({ "path": path })).unwrap().remove(0) {
        Content::Json(v) => v["doc"].as_u64().unwrap(),
        other => panic!("expected JSON: {other:?}"),
    }
}

#[test]
fn page_render_lists_the_warnings_after_the_png() {
    let (dir, mut a) = setup("fallback");
    let doc = open(&mut a, "fallback.pdf");
    let out = a.call("page_render", &json!({ "doc": doc, "page": 1, "dpi": 72 })).unwrap();
    assert_eq!(out.len(), 2, "the PNG and the warnings: {out:?}");
    assert!(matches!(out[0], Content::Png { width: 200, height: 200, .. }), "the image comes first, as before");
    let Content::Json(report) = &out[1] else { panic!("expected the warnings as JSON: {:?}", out[1]) };
    assert_eq!(report["page"], 1, "the page is 1-based, like the argument");
    assert_eq!(report["warnings"], json!([FALLBACK]));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn render_preview_lists_the_warnings_after_the_png_too() {
    let (dir, mut a) = setup("preview");
    let doc = open(&mut a, "fallback.pdf");
    let out = a.call("render_preview", &json!({ "doc": doc, "max_side": 64 })).unwrap();
    assert!(matches!(out.first(), Some(Content::Png { width: 64, height: 64, .. })), "{out:?}");
    let Some(Content::Json(report)) = out.get(1) else { panic!("expected the warnings as JSON: {out:?}") };
    assert_eq!(report["page"], 1, "the page defaults to the first");
    assert_eq!(report["warnings"], json!([FALLBACK]));
    let clean = open(&mut a, "clean.pdf");
    assert_eq!(a.call("render_preview", &json!({ "doc": clean })).unwrap().len(), 1, "a clean page gives the PNG alone");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_clean_page_renders_to_the_png_alone() {
    let (dir, mut a) = setup("clean");
    let fallback = open(&mut a, "fallback.pdf");
    let clean = open(&mut a, "clean.pdf");
    // The warning of one render doesn't carry over to the next, nor to another document.
    a.call("page_render", &json!({ "doc": fallback, "page": 1, "dpi": 72 })).unwrap();
    let out = a.call("page_render", &json!({ "doc": clean, "page": 1, "dpi": 72 })).unwrap();
    assert_eq!(out.len(), 1, "no warnings, no extra part: {out:?}");
    assert!(matches!(out[0], Content::Png { .. }));
    let again = a.call("page_render", &json!({ "doc": fallback, "page": 1, "dpi": 72 })).unwrap();
    assert_eq!(again.len(), 2, "a cached renderer still reports the page's warning: {again:?}");
    let _ = std::fs::remove_dir_all(dir);
}

/// Over MCP the warnings are a text block after the image and the call's `structuredContent`.
#[cfg(feature = "mcp")]
#[test]
fn mcp_page_render_carries_the_warnings_as_structured_content() {
    use pdfcraft_automation::mcp::McpServer;
    let (dir, a) = setup("mcp");
    let mut s = McpServer::new(a);
    let mut rpc = |id: u64, method: &str, params: Value| -> Value {
        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
        serde_json::from_str(&s.handle_line(&line).expect("a reply")).unwrap()
    };
    rpc(1, "initialize", json!({}));
    rpc(2, "tools/call", json!({ "name": "doc_open", "arguments": { "path": "fallback.pdf" } }));
    let reply = rpc(3, "tools/call", json!({ "name": "page_render", "arguments": { "doc": 1, "page": 1 } }));
    let result = &reply["result"];
    assert_eq!(result["isError"], false, "{reply}");
    assert_eq!(result["content"][0]["type"], "image");
    assert_eq!(result["content"][1]["type"], "text");
    assert_eq!(result["structuredContent"]["warnings"], json!([FALLBACK]));
    let _ = std::fs::remove_dir_all(dir);
}

/// The warning a Type 3 font without `/Widths` gives, worded as `pdfcraft-cli render` prints it.
const TYPE3_WIDTHS: &str = "a Type 3 font has no /Widths; its glyphs were spaced by the widths their glyph procedures give, \
                            where they give one, and otherwise by /MissingWidth (0 by default)";

/// A page showing `ab` in a Type 3 font without `/Widths`: `a`'s procedure sets its width with
/// `d1`, and `b`'s `d0` gives a name where its width should be, so `b` falls back on
/// `/MissingWidth`.
fn type3_pdf() -> Vec<u8> {
    let content = "BT /F1 100 Tf 20 100 Td (ab) Tj ET";
    let glyph_a = "500 0 0 0 400 400 d1 0 0 400 400 re f";
    let glyph_b = "/x 0 d0 0 0 600 600 re f";
    format!(
        "%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n\
         2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n\
         3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >> endobj\n\
         4 0 obj << /Length {} >> stream\n{content}\nendstream endobj\n\
         5 0 obj << /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] /FontMatrix [0.001 0 0 0.001 0 0] \
         /CharProcs << /a 6 0 R /b 7 0 R >> /Encoding << /Type /Encoding /Differences [97 /a /b] >> >> endobj\n\
         6 0 obj << /Length {} >> stream\n{glyph_a}\nendstream endobj\n\
         7 0 obj << /Length {} >> stream\n{glyph_b}\nendstream endobj\n\
         trailer << /Root 1 0 R >>\n%%EOF",
        content.len(),
        glyph_a.len(),
        glyph_b.len()
    )
    .into_bytes()
}

/// The sentence says what happened to every glyph: one whose procedure gives no width wasn't
/// spaced by its procedure, so the warning doesn't claim it was.
#[test]
fn a_type3_font_without_widths_is_reported_with_its_fallback() {
    let (dir, mut a) = setup("type3");
    std::fs::write(dir.join("type3.pdf"), type3_pdf()).unwrap();
    let doc = open(&mut a, "type3.pdf");
    let out = a.call("page_render", &json!({ "doc": doc, "page": 1, "dpi": 72 })).unwrap();
    let Some(Content::Json(report)) = out.get(1) else { panic!("expected the warnings as JSON: {out:?}") };
    assert_eq!(report["warnings"], json!([TYPE3_WIDTHS]));
    let _ = std::fs::remove_dir_all(dir);
}

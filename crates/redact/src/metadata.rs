//! Metadata that repeats redacted text (#2448).
//!
//! [`crate::apply`] removes the glyphs under the marks, but the same words are often in the
//! document information dictionary (`/Title`, `/Subject`, `/Keywords`…) and the XMP metadata
//! that mirrors it. Once the page no longer shows them, the metadata must not keep them either.
//!
//! An `/Info` entry whose text contains a redacted term is removed. An XMP stream whose text
//! (element content and attribute values, entities decoded) contains one is removed whole:
//! editing the XML in place could leave it malformed or keep the term in another spelling.
//! Unreadable XMP goes too, and so does text too large to search against every term
//! (fail-closed). Terms are matched without regard to case or spacing.

use std::collections::HashSet;

use pdfcraft_cos::{Document, ObjRef, Object};

/// Terms longer than this are cut (matching still finds the prefix).
const MAX_TERM_CHARS: usize = 4096;
const MAX_TERMS: usize = 20_000;
/// XMP packets larger than this, decoded, are removed without reading them.
const MAX_XMP_BYTES: usize = 16 * 1024 * 1024;
/// Text whose length times the number of terms exceeds this is treated as matching rather than
/// searched (fail-closed), so a page of removed text against a large packet stays quick.
const MAX_WORK: usize = 1 << 28;

/// Lower-case `s` with every run of whitespace made one space, trimmed.
fn normalize(s: &str) -> String {
    s.split_whitespace().flat_map(|w| w.chars().flat_map(char::to_lowercase).chain(std::iter::once(' '))).collect::<String>().trim_end().to_string()
}

fn significant(s: &str, min: usize) -> bool {
    s.chars().filter(|c| c.is_alphanumeric()).count() >= min
}

/// The terms to look for: each removed run (at least two letters or digits), and each of its
/// words with at least three, so "Smith, John" in `/Author` is caught when "John Smith" was
/// redacted. Shorter pieces would match nearly any text.
pub(crate) fn terms(removed: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for run in removed {
        let run: String = normalize(run).chars().take(MAX_TERM_CHARS).collect();
        let words = run.split(|c: char| !c.is_alphanumeric()).filter(|w| significant(w, 3)).map(str::to_string).collect::<Vec<_>>();
        for t in std::iter::once(run.clone()).filter(|r| significant(r, 2)).chain(words) {
            if out.len() >= MAX_TERMS {
                return out;
            }
            if seen.insert(t.clone()) {
                out.push(t);
            }
        }
    }
    out
}

fn mentions(text: &str, terms: &[String]) -> bool {
    let text = normalize(text);
    text.len().saturating_mul(terms.len()) > MAX_WORK || terms.iter().any(|t| text.contains(t.as_str()))
}

/// Decode the five predefined XML entities and character references; anything else stays.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(rest.get(..at).unwrap_or(""));
        let tail = rest.get(at..).unwrap_or("");
        let end = tail.find(';').filter(|&e| e <= 12);
        let entity = end.and_then(|e| tail.get(1..e));
        let decoded = entity.and_then(|e| match e {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => {
                let n = match e.strip_prefix("#x").or_else(|| e.strip_prefix("#X")) {
                    Some(h) => u32::from_str_radix(h, 16).ok(),
                    None => e.strip_prefix('#').and_then(|d| d.parse().ok()),
                };
                n.and_then(char::from_u32)
            }
        });
        match (decoded, end) {
            (Some(c), Some(e)) => {
                out.push(c);
                rest = tail.get(e + 1..).unwrap_or("");
            }
            _ => {
                out.push('&');
                rest = tail.get(1..).unwrap_or("");
            }
        }
    }
    out.push_str(rest);
    out
}

/// The text an XML document carries: element content and attribute values (XMP stores simple
/// properties as attributes), each followed by a space, entities decoded. Names, comments and
/// markup are left out, so a term can't match a tag such as `dc:title`.
pub(crate) fn xml_text(xml: &str) -> String {
    let mut out = String::new();
    let mut rest = xml;
    while !rest.is_empty() {
        let Some(open) = rest.find('<') else {
            out.push_str(&unescape(rest));
            break;
        };
        out.push_str(&unescape(rest.get(..open).unwrap_or("")));
        out.push(' ');
        let tag = rest.get(open..).unwrap_or("");
        if let Some(body) = tag.strip_prefix("<![CDATA[") {
            let end = body.find("]]>").unwrap_or(body.len());
            out.push_str(body.get(..end).unwrap_or(""));
            out.push(' ');
            rest = body.get(end + 3..).unwrap_or("");
            continue;
        }
        if tag.starts_with("<!--") {
            rest = tag.find("-->").and_then(|e| tag.get(e + 3..)).unwrap_or("");
            continue;
        }
        // Attribute values inside the tag, up to its closing `>` (which can't be inside quotes).
        let mut chars = tag.char_indices().skip(1);
        let mut end = tag.len();
        while let Some((i, c)) = chars.next() {
            match c {
                '>' => {
                    end = i + 1;
                    break;
                }
                '"' | '\'' => {
                    let mut value = String::new();
                    for (_, d) in chars.by_ref() {
                        if d == c {
                            break;
                        }
                        value.push(d);
                    }
                    out.push_str(&unescape(&value));
                    out.push(' ');
                }
                _ => {}
            }
        }
        rest = tag.get(end..).unwrap_or("");
    }
    out
}

/// The decoded text of an XMP stream (UTF-8, or UTF-16 with a byte-order mark), or `None` when it
/// can't be read.
fn xmp_text(doc: &Document, o: &Object) -> Option<String> {
    let Object::Stream(s) = &*doc.resolve(o) else { return None };
    let bytes = s.decoded().ok()?;
    if bytes.len() > MAX_XMP_BYTES {
        return None;
    }
    let units = |be: bool| {
        bytes.get(2..).unwrap_or(&[]).as_chunks::<2>().0.iter().map(move |&p| if be { u16::from_be_bytes(p) } else { u16::from_le_bytes(p) })
    };
    let text = match bytes.get(..2) {
        Some([0xFE, 0xFF]) => char::decode_utf16(units(true)).map(|c| c.unwrap_or('\u{FFFD}')).collect(),
        Some([0xFF, 0xFE]) => char::decode_utf16(units(false)).map(|c| c.unwrap_or('\u{FFFD}')).collect(),
        _ => String::from_utf8_lossy(&bytes).into_owned(),
    };
    Some(xml_text(&text))
}

/// Remove the metadata that still holds `removed` text: matching `/Info` entries, and the
/// catalog's and pages' XMP streams that mention it. Returns how many entries and streams went.
pub(crate) fn scrub(doc: &mut Document, removed: &[String]) -> Result<usize, pdfcraft_cos::CosError> {
    let terms = terms(removed);
    if terms.is_empty() {
        return Ok(0);
    }
    let mut n = 0;

    // The information dictionary, direct in the trailer or an object of its own.
    let info_obj = doc.trailer().get(b"Info").cloned();
    if let Some(info) = info_obj.as_ref().and_then(|i| doc.resolve(i).as_dict().cloned()) {
        let doomed: Vec<Vec<u8>> = info
            .iter()
            .filter(|(_, v)| doc.resolve(v).as_string().is_some_and(|s| mentions(&s.to_text(), &terms)))
            .map(|(k, _)| k.to_vec())
            .collect();
        if !doomed.is_empty() {
            n += doomed.len();
            let mut kept = info.clone();
            for k in &doomed {
                kept.remove(k);
            }
            match info_obj.as_ref().and_then(Object::as_ref) {
                Some(r) => doc.update_dict(r, |d| *d = kept)?,
                None => doc.trailer_mut().set(b"Info".to_vec(), Object::Dict(kept)),
            }
        }
    }

    // XMP on the catalog and on every page.
    let mut holders: Vec<ObjRef> = doc.root().into_iter().collect();
    holders.extend(pdfcraft_model::pages(doc).iter().map(|p| p.obj));
    for holder in holders {
        let Some(xmp) = doc.get(holder).as_dict().and_then(|d| d.get(b"Metadata").cloned()) else { continue };
        if xmp_text(doc, &xmp).is_none_or(|t| mentions(&t, &terms)) {
            doc.update_dict(holder, |d| {
                d.remove(b"Metadata");
            })?;
            n += 1;
        }
    }
    Ok(n)
}

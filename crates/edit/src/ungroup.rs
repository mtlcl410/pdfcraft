//! Edit a PDF ▸ Ungroup: draw a Form XObject's content in place of its `Do` (#2964, #3005).
//!
//! Text inside grouped Form artwork can't be edited, because editing rewrites only the page's own
//! content streams. Ungrouping copies the form's content into the page at the same place,
//! wrapped as `q <Matrix> cm <BBox> re W n … Q`, with the form's resources merged into a copy of
//! the page's under names that don't clash. Its text then edits like any other. The form object
//! itself is left alone, so another page or placement that draws it is unchanged.
//!
//! Drawing a form inline paints the same pixels unless the form is a transparency group whose
//! compositing differs from painting its parts one by one: a knockout group, a group that uses
//! a blend mode other than Normal inside, or a group drawn translucent, blended or under a soft
//! mask. Those are refused rather than changed, as is tagged form content (its structure link
//! would be lost). A form in a layer (`/OC`) keeps its membership as `/OC … BDC … EMC`.

use pdfcraft_content::{Matrix, Op, parse};
use pdfcraft_cos::{Dict, Document, Object};

use crate::EditError;

/// The largest form content ungrouped (decoded bytes): the page's own content cap is far above
/// any real form, and a bomb must not be copied into the page.
const MAX_FORM_BYTES: usize = 64 << 20;

/// Resource categories a content stream refers to by name (ISO 32000-2 §7.8.3, Table 34).
const CATEGORIES: [&[u8]; 7] = [b"Font", b"XObject", b"ExtGState", b"ColorSpace", b"Pattern", b"Shading", b"Properties"];

/// A name map for one resource category: form name → name in the page's resources.
type Renames = Vec<(Vec<u8>, Vec<u8>)>;

/// The ops that draw form `name` (from the page's `resources`) inline, and the page resources
/// with the form's merged in. `translucent` says whether the graphics state at the `Do` is
/// translucent, blended or soft-masked, which a transparency group composites as a whole.
pub(crate) fn ungroup_ops(doc: &Document, resources: &Dict, name: &[u8], translucent: bool) -> Result<(Vec<Op>, Dict), EditError> {
    let refuse = |why: &str| EditError::Invalid(format!("this artwork can't be ungrouped: {why}"));
    let xobjects = resources.get(b"XObject").map(|x| doc.resolve(x)).and_then(|x| x.as_dict().cloned()).ok_or_else(|| refuse("it is missing"))?;
    let form = xobjects.get(name).ok_or_else(|| refuse("it is missing"))?;
    let Object::Stream(s) = &*doc.resolve(form) else { return Err(refuse("it is not a Form XObject")) };
    if s.dict.name(b"Subtype") != Some(b"Form") {
        return Err(refuse("it is not a Form XObject"));
    }
    let numbers = |key: &[u8], n: usize| -> Option<Vec<f64>> {
        let a = s.dict.get(key).map(|o| doc.resolve(o))?;
        let v: Vec<f64> = a.as_array()?.iter().filter_map(|x| doc.resolve(x).as_f64()).collect();
        (v.len() == n && v.iter().all(|x| x.is_finite())).then_some(v)
    };
    let bbox = numbers(b"BBox", 4).ok_or_else(|| refuse("its bounding box is missing or unreadable"))?;
    let matrix = match s.dict.get(b"Matrix") {
        None => None,
        Some(_) => Some(Matrix(
            <[f64; 6]>::try_from(numbers(b"Matrix", 6).ok_or_else(|| refuse("its matrix is unreadable"))?)
                .map_err(|_| refuse("its matrix is unreadable"))?,
        )),
    };
    let own = s.dict.get(b"Resources").map(|r| doc.resolve(r)).and_then(|r| r.as_dict().cloned());
    // A form without its own resources uses the ones it is drawn with (PDF 1.1).
    let form_res = own.clone().unwrap_or_else(|| resources.clone());

    // A tagged form's content is a structure element's: drawn inline it would lose that link.
    if s.dict.get(b"StructParent").is_some() || s.dict.get(b"StructParents").is_some() {
        return Err(refuse("it is tagged content (Tags panel); edit its text in a copy without tags"));
    }
    let group = s.dict.get(b"Group").map(|g| doc.resolve(g)).and_then(|g| g.as_dict().cloned()).filter(|g| g.name(b"S") == Some(b"Transparency"));
    if let Some(group) = &group {
        if matches!(group.get(b"K"), Some(Object::Bool(true))) {
            return Err(refuse("it is a knockout transparency group"));
        }
        if translucent {
            return Err(refuse("it is a transparency group drawn translucent, blended or under a soft mask"));
        }
    }

    // Strictly: damaged data must not be copied in as a shorter form.
    let data = s.decoded_strict_within(MAX_FORM_BYTES).map_err(|e| refuse(&format!("its content can't be read ({e})")))?;
    let parsed = parse(&data);
    if parsed.skipped > 0 {
        return Err(refuse("its content has parts that can't be read"));
    }
    let mut ops = parsed.ops;
    if group.is_some() {
        // The form itself is being read: drawing itself again counts as blending.
        let mut seen: Vec<pdfcraft_cos::ObjRef> = form.as_ref().into_iter().collect();
        if blends_inside(doc, &form_res, &ops, 0, &mut seen) {
            return Err(refuse("it is a transparency group with blend modes inside"));
        }
    }

    // Merge the form's resources into a copy of the page's, renaming what clashes.
    let mut merged = resources.clone();
    let mut renames: Vec<(&[u8], Renames)> = Vec::new();
    if own.is_some() {
        for cat in CATEGORIES {
            let Some(theirs) = form_res.get(cat).map(|x| doc.resolve(x)).and_then(|x| x.as_dict().cloned()) else { continue };
            let mut mine = merged.get(cat).map(|x| doc.resolve(x)).and_then(|x| x.as_dict().cloned()).unwrap_or_default();
            let mut map = Renames::new();
            let mut n = 0u32;
            for (key, value) in theirs.iter() {
                if mine.get(key) == Some(value) {
                    continue;
                }
                let fresh = loop {
                    n = n.saturating_add(1);
                    let candidate = format!("PCUg{n}").into_bytes();
                    if mine.get(&candidate).is_none() && theirs.get(&candidate).is_none() {
                        break candidate;
                    }
                    if n == u32::MAX {
                        return Err(refuse("too many resources"));
                    }
                };
                mine.set(fresh.clone(), value.clone());
                map.push((key.to_vec(), fresh));
            }
            merged.set(cat.to_vec(), Object::Dict(mine));
            if !map.is_empty() {
                renames.push((cat, map));
            }
        }
    }
    let renamed = |cat: &[u8], name: &[u8]| -> Option<Vec<u8>> {
        renames.iter().find(|(c, _)| *c == cat).and_then(|(_, m)| m.iter().find(|(from, _)| from == name)).map(|(_, to)| to.clone())
    };
    let mut depth = 0usize;
    for op in &mut ops {
        let slot: Option<(&[u8], usize)> = match op.op.as_slice() {
            b"Tf" | b"Do" | b"gs" | b"sh" | b"cs" | b"CS" => {
                let cat: &[u8] = match op.op.as_slice() {
                    b"Tf" => b"Font",
                    b"Do" => b"XObject",
                    b"gs" => b"ExtGState",
                    b"sh" => b"Shading",
                    _ => b"ColorSpace",
                };
                Some((cat, 0))
            }
            b"scn" | b"SCN" => op.operands.len().checked_sub(1).map(|i| (&b"Pattern"[..], i)),
            b"BDC" | b"DP" => Some((&b"Properties"[..], 1)),
            b"q" => {
                depth = depth.saturating_add(1);
                None
            }
            b"Q" => {
                depth = depth.checked_sub(1).ok_or_else(|| refuse("its content restores more graphics states than it saves"))?;
                None
            }
            _ => None,
        };
        if let Some((cat, i)) = slot
            && let Some(operand) = op.operands.get_mut(i)
            && let Object::Name(n) = operand
            && let Some(to) = renamed(cat, n)
        {
            *operand = Object::Name(to);
        }
        if let Some((dict, _)) = &mut op.inline {
            for key in [&b"CS"[..], b"ColorSpace"] {
                if let Some(Object::Name(n)) = dict.get(key)
                    && let Some(to) = renamed(b"ColorSpace", n)
                {
                    dict.set(key.to_vec(), Object::Name(to));
                }
            }
        }
    }

    // A form in a layer (/OC) stays in it: its content is marked with the same membership.
    let layer = match s.dict.get(b"OC") {
        None => None,
        Some(oc) => {
            let mut props = merged.get(b"Properties").map(|x| doc.resolve(x)).and_then(|x| x.as_dict().cloned()).unwrap_or_default();
            let existing = props.iter().find(|(_, v)| *v == oc).map(|(k, _)| k.clone());
            let name = match existing {
                Some(k) => k,
                None => {
                    let fresh = (1..=u32::MAX)
                        .map(|n| format!("PCUgOC{n}").into_bytes())
                        .find(|c| props.get(c).is_none())
                        .ok_or_else(|| refuse("too many resources"))?;
                    props.set(fresh.clone(), oc.clone());
                    merged.set(b"Properties".to_vec(), Object::Dict(props));
                    fresh
                }
            };
            Some(name)
        }
    };

    let num = |v: f64| Object::Real(v);
    let mut out = Vec::new();
    if let Some(name) = &layer {
        out.push(Op::new("BDC", vec![Object::name("OC"), Object::Name(name.clone())]));
    }
    out.push(Op::new("q", Vec::new()));
    if let Some(m) = matrix {
        out.push(Op::new("cm", m.0.iter().copied().map(num).collect()));
    }
    let (x0, y0, x1, y1) = (bbox[0].min(bbox[2]), bbox[1].min(bbox[3]), bbox[0].max(bbox[2]), bbox[1].max(bbox[3]));
    out.push(Op::new("re", vec![num(x0), num(y0), num(x1 - x0), num(y1 - y0)]));
    out.push(Op::new("W", Vec::new()));
    out.push(Op::new("n", Vec::new()));
    out.extend(ops);
    // A form ends with the state it was drawn with (the Do saves and restores it).
    out.extend(std::iter::repeat_with(|| Op::new("Q", Vec::new())).take(depth));
    out.push(Op::new("Q", Vec::new()));
    if layer.is_some() {
        out.push(Op::new("EMC", Vec::new()));
    }
    Ok((out, merged))
}

/// How deep [`blends_inside`] follows nested forms; deeper ones count as blending.
const MAX_NESTING: usize = 8;

/// Whether `ops` (drawn with `resources`) blend with something other than Normal: a `gs` they
/// use, or one used by a form they draw, at any depth. A nested form that can't be read, is
/// nested too deep or draws itself counts as blending, so the caller refuses rather than guesses.
fn blends_inside(doc: &Document, resources: &Dict, ops: &[Op], depth: usize, seen: &mut Vec<pdfcraft_cos::ObjRef>) -> bool {
    let category = |cat: &[u8]| resources.get(cat).map(|x| doc.resolve(x)).and_then(|x| x.as_dict().cloned()).unwrap_or_default();
    let (states, xobjects) = (category(b"ExtGState"), category(b"XObject"));
    for op in ops {
        if op.is("gs")
            && op.name(0).and_then(|n| states.get(n)).map(|g| doc.resolve(g)).and_then(|g| g.as_dict().cloned()).is_some_and(|gs| blends(doc, &gs))
        {
            return true;
        }
        if !op.is("Do") {
            continue;
        }
        let Some(o) = op.name(0).and_then(|n| xobjects.get(n)) else { continue };
        let Object::Stream(nested) = &*doc.resolve(o) else { continue };
        if nested.dict.name(b"Subtype") != Some(b"Form") {
            continue;
        }
        if depth >= MAX_NESTING || o.as_ref().is_some_and(|r| seen.contains(&r)) {
            return true;
        }
        let Ok(data) = nested.decoded_strict_within(MAX_FORM_BYTES) else { return true };
        let own = nested.dict.get(b"Resources").map(|r| doc.resolve(r)).and_then(|r| r.as_dict().cloned()).unwrap_or_else(|| resources.clone());
        if let Some(r) = o.as_ref() {
            seen.push(r);
        }
        let inner = parse(&data);
        if inner.skipped > 0 {
            return true;
        }
        let blends = blends_inside(doc, &own, &inner.ops, depth + 1, seen);
        if let Some(r) = o.as_ref() {
            seen.retain(|x| *x != r);
        }
        if blends {
            return true;
        }
    }
    false
}

/// Whether an ExtGState blends with something other than Normal.
fn blends(doc: &Document, gs: &Dict) -> bool {
    let Some(bm) = gs.get(b"BM").map(|b| doc.resolve(b)) else { return false };
    let first = match &*bm {
        Object::Array(a) => a.first().map(|x| doc.resolve(x)),
        _ => Some(bm.clone()),
    };
    !first.is_some_and(|b| matches!(b.as_name(), Some(b"Normal" | b"Compatible")))
}

/// Whether the graphics state is translucent, blended or soft-masked at op `at` of stream
/// `stream`, following `q`/`Q` and `gs` through the page's streams in order. Each `gs` changes
/// only the parameters it sets.
pub(crate) fn translucent_at(doc: &Document, resources: &Dict, streams: &[Vec<Op>], stream: usize, at: usize) -> bool {
    let states = resources.get(b"ExtGState").map(|x| doc.resolve(x)).and_then(|x| x.as_dict().cloned()).unwrap_or_default();
    // Fill alpha below one, stroke alpha below one, a soft mask, a blend mode other than Normal.
    let mut stack: Vec<[bool; 4]> = Vec::new();
    let mut now = [false; 4];
    for (si, ops) in streams.iter().enumerate().take(stream.saturating_add(1)) {
        let end = if si == stream { at.min(ops.len()) } else { ops.len() };
        for op in ops.iter().take(end) {
            match op.op.as_slice() {
                b"q" => stack.push(now),
                b"Q" => now = stack.pop().unwrap_or([false; 4]),
                b"gs" => {
                    let Some(gs) = op.name(0).and_then(|n| states.get(n)).map(|g| doc.resolve(g)).and_then(|g| g.as_dict().cloned()) else {
                        continue;
                    };
                    let alpha = |key: &[u8]| gs.get(key).map(|v| doc.resolve(v)).and_then(|v| v.as_f64()).map(|a| a < 1.0);
                    if let Some(v) = alpha(b"ca") {
                        now[0] = v;
                    }
                    if let Some(v) = alpha(b"CA") {
                        now[1] = v;
                    }
                    if let Some(m) = gs.get(b"SMask").map(|v| doc.resolve(v)) {
                        now[2] = m.as_name() != Some(b"None");
                    }
                    if gs.get(b"BM").is_some() {
                        now[3] = blends(doc, &gs);
                    }
                }
                _ => {}
            }
        }
    }
    now.iter().any(|v| *v)
}

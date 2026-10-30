//! Pictures a template links instead of embedding (`<image href="…">`). Since XFA 2.6 a form
//! keeps them in its PDF: each image file is a stream holding the file's bytes, named in the
//! `XFAImages` name tree of the catalog's `Names` dictionary by the URI the template writes (XFA
//! 3.3 pp. 257–258, "Storing Images Inside PDF"). Only that tree is looked in: as the spec warns,
//! a form that isn't trusted must not have its links followed outside the file.

use std::collections::{HashMap, HashSet};

use pdfcraft_cos::{Document, ObjRef, Object};

use crate::model::{Field, Node, Subform, Template, Value};

/// Most nodes of the name tree visited, and most names kept from it.
const MAX_TREE_NODES: usize = 4096;
const MAX_NAMES: usize = 4096;
/// Deepest the name tree is followed.
const MAX_DEPTH: usize = 32;
/// Largest picture taken from the file, as for a picture written into the template.
const MAX_IMAGE: usize = 32 << 20;
/// Most bytes of pictures taken from the file in all, each place a picture is used counted (a
/// picture is held once per place). Small in tests, so a test can reach it.
#[cfg(not(test))]
const MAX_TOTAL: usize = 128 << 20;
#[cfg(test)]
const MAX_TOTAL: usize = 1 << 10;
/// Deepest the template is walked.
const MAX_TEMPLATE_DEPTH: usize = 256;

/// The `XFAImages` name tree: each name, as text, and the object it names.
fn names(doc: &Document) -> HashMap<String, Object> {
    let mut out = HashMap::new();
    let Some(root) = doc.root() else { return out };
    let catalog = doc.get(root);
    let tree =
        catalog.as_dict().and_then(|c| c.get(b"Names")).map(|n| doc.resolve(n)).and_then(|n| n.as_dict().and_then(|d| d.get(b"XFAImages")).cloned());
    let Some(tree) = tree else { return out };
    let mut seen: HashSet<ObjRef> = HashSet::new();
    let mut visited = 0usize;
    let mut stack = vec![(tree, 0usize)];
    while let Some((node, depth)) = stack.pop() {
        if visited >= MAX_TREE_NODES || out.len() >= MAX_NAMES {
            break;
        }
        // Too deep: this branch is left, the others are still walked.
        if depth > MAX_DEPTH {
            continue;
        }
        visited += 1;
        if let Some(r) = node.as_ref()
            && !seen.insert(r)
        {
            continue;
        }
        let node = doc.resolve(&node);
        let Some(d) = node.as_dict() else { continue };
        if let Some(pairs) = d.get(b"Names").map(|n| doc.resolve(n)).and_then(|n| n.as_array().cloned()) {
            for pair in pairs.chunks(2) {
                if let [key, value] = pair
                    && let Some(key) = doc.resolve(key).as_string()
                {
                    out.entry(key.to_text()).or_insert_with(|| value.clone());
                    if out.len() >= MAX_NAMES {
                        break;
                    }
                }
            }
        }
        if let Some(kids) = d.get(b"Kids").map(|k| doc.resolve(k)).and_then(|k| k.as_array().cloned()) {
            stack.extend(kids.into_iter().map(|k| (k, depth + 1)));
        }
    }
    out
}

/// Pictures found so far by their link, read from the file once each.
struct Found<'a> {
    doc: &'a Document,
    names: HashMap<String, Object>,
    read: HashMap<String, Option<Vec<u8>>>,
    total: usize,
    warnings: Vec<String>,
}

impl Found<'_> {
    /// A copy of the picture linked as `href` for one more place, when the file holds it and the
    /// copies so far leave room for it under [`MAX_TOTAL`].
    fn get(&mut self, href: &str) -> Option<Vec<u8>> {
        if !self.read.contains_key(href) {
            let data = self.load(href);
            self.read.insert(href.to_string(), data);
        }
        let len = self.read.get(href).and_then(Option::as_ref).map(Vec::len)?;
        if self.total.saturating_add(len) > MAX_TOTAL {
            self.warnings.push("the pictures the form keeps in the file take too much memory in all; the rest are left out".into());
            return None;
        }
        self.total = self.total.saturating_add(len);
        self.read.get(href).cloned().flatten()
    }

    fn load(&mut self, href: &str) -> Option<Vec<u8>> {
        let object = self.names.get(href)?;
        let object = self.doc.resolve(object);
        let Object::Stream(s) = &*object else { return None };
        let data = match s.decoded_within(MAX_IMAGE) {
            Ok(d) => d,
            Err(_) => {
                self.warnings.push(format!("a picture the form keeps in the file ({href}) could not be read; it is left out"));
                return None;
            }
        };
        Some(data)
    }

    fn value(&mut self, v: &mut Value) {
        if let Value::Image { data, href: Some(href), .. } = v
            && data.is_empty()
            && let Some(found) = self.get(href)
        {
            *data = found;
        }
    }

    fn field(&mut self, f: &mut Field) {
        self.value(&mut f.value);
        if let Some(c) = f.caption.as_mut() {
            self.value(&mut c.value);
        }
    }

    fn nodes(&mut self, nodes: &mut [Node], depth: usize) {
        if depth > MAX_TEMPLATE_DEPTH {
            return;
        }
        for n in nodes {
            match n {
                Node::Subform(s) => self.subform(s, depth + 1),
                Node::Area(a) => self.nodes(&mut a.children, depth + 1),
                Node::Field(f) => self.field(f),
                Node::Draw(d) => self.value(&mut d.value),
                Node::ExclGroup(g) => g.fields.iter_mut().for_each(|f| self.field(f)),
            }
        }
    }

    fn subform(&mut self, s: &mut Subform, depth: usize) {
        if let Some(set) = s.page_set.as_mut() {
            for area in &mut set.areas {
                self.nodes(&mut area.items, depth + 1);
            }
        }
        self.nodes(&mut s.children, depth);
    }
}

/// Give the pictures `tpl` links to (and doesn't embed) the data `doc` keeps for them. A link
/// the file holds nothing for stays empty. What couldn't be read is returned as warnings.
pub fn resolve(tpl: &mut Template, doc: &Document) -> Vec<String> {
    let names = names(doc);
    if names.is_empty() {
        return Vec::new();
    }
    let mut found = Found { doc, names, read: HashMap::new(), total: 0, warnings: Vec::new() };
    found.subform(&mut tpl.root, 0);
    found.warnings.dedup();
    found.warnings
}

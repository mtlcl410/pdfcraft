//! Unnamed exclusion groups (radio groups without a name). Such a group is transparent to data
//! binding (XFA 3.3, "Transparent Nodes", p. 193): it has no data node of its own, and each of
//! its buttons is bound to its own node under the group's container, the long exclusion format,
//! which needs no group name (p. 197). Acrobat saves such a group so, each button under its own
//! name (`<TopA>1</TopA><TopB/>`). A named group keeps its value in its own node (the short
//! format, p. 196).

use std::sync::Arc;

use pdfcraft_cos::{Document, SaveOptions, write_incremental};

use crate::data::{FieldData, FieldDatum, parse_datasets, read_values, som_to_path, write_datasets};
use crate::fixtures::shell;
use crate::script::{FormNode, NodeKind, Overrides, data_of, form_tree, rerender, template_of};
use crate::{read_packets, render_into};

/// A form with an unnamed radio group at the top, a repeating row holding another, one at the
/// bottom and a named one (`pick`), each of two buttons whose items are `1` and `2`; `data` is
/// the children of the `form` data node.
fn form(data: &str) -> String {
    let radio = |name: &str, on: u8| {
        format!(r#"<field name="{name}" w="1in" h="18pt"><ui><checkButton shape="round"/></ui><items><integer>{on}</integer></items></field>"#)
    };
    let group = |name: Option<&str>, a: &str, b: &str| {
        let name = name.map(|n| format!(r#" name="{n}""#)).unwrap_or_default();
        format!(r#"<exclGroup{name} layout="lr-tb">{}{}</exclGroup>"#, radio(a, 1), radio(b, 2))
    };
    format!(
        r#"<xdp:xdp xmlns:xdp="http://ns.adobe.com/xdp/">
<template xmlns="http://www.xfa.org/schema/xfa-template/3.3/">
<subform name="form" layout="tb">
<pageSet><pageArea name="page"><contentArea x="36pt" y="36pt" w="540pt" h="720pt"/><medium short="612pt" long="792pt"/></pageArea></pageSet>
{}
<subform name="rows" layout="tb"><occur min="1" max="-1" initial="1"/>{}</subform>
{}
{}
</subform>
</template>
<xfa:datasets xmlns:xfa="http://www.xfa.org/schema/xfa-data/1.0/"><xfa:data><form>{data}</form></xfa:data></xfa:datasets>
</xdp:xdp>"#,
        group(None, "TopA", "TopB"),
        group(None, "RowA", "RowB"),
        group(None, "BottomA", "BottomB"),
        group(Some("pick"), "PickA", "PickB"),
    )
}

/// The radio groups of the laid-out form: each one's field, by the SOM path of its first
/// button (`form[0].TopA[0]`, `form[0].rows[1].RowA[0]`, `form[0].pick[0].PickA[0]`).
fn groups(doc: &Document) -> Vec<(String, FieldDatum)> {
    let catalog = doc.get(doc.root().unwrap());
    let acro = doc.resolve(catalog.as_dict().unwrap().get(b"AcroForm").unwrap());
    let fields = doc.resolve(acro.as_dict().unwrap().get(b"Fields").unwrap()).as_array().unwrap().clone();
    let text = |d: &pdfcraft_cos::Dict, key: &[u8]| d.get(key).map(|o| doc.resolve(o)).and_then(|o| o.as_string().map(|s| s.to_text()));
    let mut out = Vec::new();
    for f in fields {
        let r = f.as_ref().unwrap();
        let d = doc.get(r);
        let d = d.as_dict().unwrap();
        if d.int(b"Ff").unwrap_or(0) & (1 << 15) == 0 {
            continue;
        }
        let kids = doc.resolve(d.get(b"Kids").unwrap()).as_array().unwrap().clone();
        let first = doc.resolve(kids.first().unwrap());
        let som = text(first.as_dict().unwrap(), crate::pdf::SOM_KEY).unwrap();
        let value = d.get(b"V").map(|v| doc.resolve(v)).and_then(|v| v.as_name().map(|n| String::from_utf8_lossy(n).into_owned()));
        let name = text(d, b"T").unwrap();
        out.push((som, FieldDatum { obj: r, name, data: FieldData::Radio(value.filter(|v| v != "Off")) }));
    }
    out
}

/// Each group's selection, by its first button's SOM path, in layout order.
fn selections(doc: &Document) -> Vec<(String, Option<String>)> {
    groups(doc)
        .into_iter()
        .filter_map(|(som, f)| match f.data {
            FieldData::Radio(v) => Some((som, v)),
            _ => None,
        })
        .collect()
}

/// [`selections`] of a form with `rows.len()` rows choosing those buttons (on states).
fn expect(top: Option<&str>, rows: &[Option<&str>], bottom: Option<&str>, pick: Option<&str>) -> Vec<(String, Option<String>)> {
    let mut out = vec![("form[0].TopA[0]".to_string(), top.map(str::to_string))];
    for (i, r) in rows.iter().enumerate() {
        out.push((format!("form[0].rows[{i}].RowA[0]"), r.map(str::to_string)));
    }
    out.push(("form[0].BottomA[0]".into(), bottom.map(str::to_string)));
    out.push(("form[0].pick[0].PickA[0]".into(), pick.map(str::to_string)));
    out
}

/// Lay the form out again from its template and data, as opening a saved form does.
fn relayout(doc: &mut Document) {
    let tpl = template_of(doc).unwrap();
    rerender(doc, &tpl).unwrap();
}

/// The text of the data node at `som`.
fn data_at(doc: &Document, som: &str) -> Option<String> {
    let packets = read_packets(doc).unwrap().unwrap();
    parse_datasets(&packets.xdp).unwrap().text_at(&som_to_path(som)).map(str::to_string)
}

/// Each group's value in the live form scripts see, in document order.
fn live_group_values(doc: &Document) -> Vec<String> {
    fn walk(n: &FormNode, out: &mut Vec<String>) {
        if n.kind == Some(NodeKind::ExclGroup) {
            out.push(n.value.clone());
        }
        for c in &n.children {
            walk(c, out);
        }
    }
    let tpl = template_of(doc).unwrap();
    let data = data_of(doc).unwrap();
    let mut out = Vec::new();
    walk(&form_tree(&tpl, Some(&data), &Overrides::default()).root, &mut out);
    out
}

/// Filled in, saved and opened again: each unnamed group's choice is saved under its button's
/// own name (a row's in that row), the named group's in its own node, and every group shows its
/// choice again when the form is laid out from the saved data. On `main` the top and bottom
/// choices were written to the `form` node, which holds the rows ("data node holds structured
/// content"), so they were lost, and a row's went into the row node itself.
#[test]
fn an_unnamed_radio_group_saves_each_button_under_its_own_name() {
    let mut doc = Document::open(Arc::new(shell(&form("<rows/><rows/>")))).unwrap();
    render_into(&mut doc).unwrap();
    assert_eq!(selections(&doc), expect(None, &[None, None], None, None));
    let picks = [Some("1"), Some("2"), Some("1"), Some("2"), Some("2")];
    let fields: Vec<FieldDatum> =
        groups(&doc).into_iter().zip(picks).map(|((_, f), pick)| FieldDatum { data: FieldData::Radio(pick.map(str::to_string)), ..f }).collect();
    let write = write_datasets(&mut doc, &fields).unwrap();
    assert!(write.written && write.warnings.is_empty(), "{write:?}");
    for (som, want) in [
        ("form[0].TopA[0]", Some("1")),
        ("form[0].TopB[0]", None),
        ("form[0].rows[0].RowA[0]", None),
        ("form[0].rows[0].RowB[0]", Some("2")),
        ("form[0].rows[1].RowA[0]", Some("1")),
        ("form[0].rows[1].RowB[0]", None),
        ("form[0].BottomA[0]", None),
        ("form[0].BottomB[0]", Some("2")),
        ("form[0].pick[0]", Some("2")),
    ] {
        assert_eq!(data_at(&doc, som).as_deref(), want, "{som}");
    }
    // What was written reads back as it was filled in, and writing it again changes nothing.
    let read: Vec<(String, FieldData)> = read_values(&doc, &fields);
    let filled: Vec<(String, FieldData)> = fields.iter().map(|f| (f.name.clone(), f.data.clone())).collect();
    assert_eq!(read, filled);
    let again = write_datasets(&mut doc, &fields).unwrap();
    assert!(!again.written && again.warnings.is_empty(), "{again:?}");

    // Opened again, the form is laid out again from the data it was saved with.
    let mut reopened = Document::open(Arc::new(write_incremental(&doc, &SaveOptions::default()).unwrap())).unwrap();
    relayout(&mut reopened);
    assert_eq!(selections(&reopened), expect(Some("1"), &[Some("2"), Some("1")], Some("2"), Some("2")));
    assert_eq!(live_group_values(&reopened), ["1", "2", "1", "2", "2"]);

    // Another button chosen: the one chosen before is emptied.
    let fields: Vec<FieldDatum> = groups(&reopened)
        .into_iter()
        .map(|(som, f)| if som == "form[0].TopA[0]" { FieldDatum { data: FieldData::Radio(Some("2".into())), ..f } } else { f })
        .collect();
    let write = write_datasets(&mut reopened, &fields).unwrap();
    assert!(write.written && write.warnings.is_empty(), "{write:?}");
    assert_eq!(data_at(&reopened, "form[0].TopA[0]").as_deref(), Some(""));
    assert_eq!(data_at(&reopened, "form[0].TopB[0]").as_deref(), Some("2"));
    relayout(&mut reopened);
    assert_eq!(selections(&reopened), expect(Some("2"), &[Some("2"), Some("1")], Some("2"), Some("2")));
}

/// Data in the shape Acrobat saves an unnamed group in (each button under its own name, the
/// ones not chosen empty) selects each group's button when the form is laid out, in the live
/// form scripts read and in what the data is read as, and the fields hold nothing to write back.
#[test]
fn an_unnamed_radio_group_reads_its_buttons_data() {
    let data = "<TopA>1</TopA><TopB/><rows><RowA/><RowB>2</RowB></rows><BottomA/><BottomB>2</BottomB>\
                <rows><RowA>1</RowA><RowB/></rows><rows><RowA/><RowB/></rows><pick>1</pick>";
    let mut doc = Document::open(Arc::new(shell(&form(data)))).unwrap();
    let report = render_into(&mut doc).unwrap();
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(selections(&doc), expect(Some("1"), &[Some("2"), Some("1"), None], Some("2"), Some("1")));
    assert_eq!(live_group_values(&doc), ["1", "2", "1", "", "2", "1"]);
    let fields: Vec<FieldDatum> = groups(&doc).into_iter().map(|(_, f)| f).collect();
    let read: Vec<FieldData> = read_values(&doc, &fields).into_iter().map(|(_, v)| v).collect();
    let item = |v: &str| FieldData::Radio(Some(v.to_string()));
    assert_eq!(read, [item("1"), item("2"), item("1"), FieldData::Radio(None), item("2"), item("1")]);
    let write = write_datasets(&mut doc, &fields).unwrap();
    assert!(!write.written && write.warnings.is_empty(), "{write:?}");
}

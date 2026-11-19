//! Bounded read-only OOXML package reader with DOCTYPE and archive-bomb limits.
pub mod compat;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::Path,
};
use xmltree::{Element, XMLNode};

pub fn local(n: &str) -> &str {
    n.rsplit(':').next().unwrap_or(n)
}

pub fn child<'a>(e: &'a Element, n: &str) -> Option<&'a Element> {
    e.children
        .iter()
        .filter_map(XMLNode::as_element)
        .find(|x| local(&x.name) == n)
}

pub fn children<'a>(e: &'a Element, n: &str) -> Vec<&'a Element> {
    e.children
        .iter()
        .filter_map(XMLNode::as_element)
        .filter(|x| local(&x.name) == n)
        .collect()
}

pub fn child_mut<'a>(e: &'a mut Element, n: &str) -> Option<&'a mut Element> {
    e.children
        .iter_mut()
        .filter_map(XMLNode::as_mut_element)
        .find(|x| local(&x.name) == n)
}

pub fn set_text(e: &mut Element, s: String) {
    e.children.clear();
    e.children.push(XMLNode::Text(s));
}

pub fn content(e: &Element) -> String {
    let mut s = String::new();
    for n in &e.children {
        match n {
            XMLNode::Text(t) | XMLNode::CData(t) => s.push_str(t),
            XMLNode::Element(x) => s.push_str(&content(x)),
            _ => (),
        }
    }
    s
}

pub fn attr(e: &Element, n: &str) -> String {
    if let Some(value) = e.attributes.get(n) {
        return value.clone();
    }
    e.attributes
        .iter()
        .find(|(k, _)| k.as_str() != "xmlns" && !k.starts_with("xmlns:") && local(k) == n)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

fn xml(raw: &[u8]) -> Result<Element> {
    use quick_xml::{Reader, events::Event};
    let mut r = Reader::from_reader(raw);
    r.config_mut().trim_text(false);
    let mut stack = Vec::<Element>::new();
    let mut root = None;
    loop {
        match r.read_event()? {
            Event::Start(x) | Event::Empty(x) => {
                let empty = raw.get(r.buffer_position().saturating_sub(2) as usize) == Some(&b'/');
                let mut e = Element::new(std::str::from_utf8(x.name().as_ref())?);
                for a in x.attributes() {
                    let a = a?;
                    e.attributes.insert(
                        std::str::from_utf8(a.key.as_ref())?.to_owned(),
                        a.decode_and_unescape_value(r.decoder())?.into_owned(),
                    );
                }
                if empty {
                    if let Some(p) = stack.last_mut() {
                        p.children.push(XMLNode::Element(e));
                    } else {
                        root = Some(e);
                    }
                } else {
                    stack.push(e);
                }
            }
            Event::End(_) => {
                let e = stack.pop().context("unbalanced workbook XML")?;
                if let Some(p) = stack.last_mut() {
                    p.children.push(XMLNode::Element(e));
                } else {
                    if root.is_some() {
                        bail!("multiple XML roots");
                    }
                    root = Some(e);
                }
            }
            Event::Text(t) => {
                if let Some(p) = stack.last_mut() {
                    p.children.push(XMLNode::Text(
                        quick_xml::escape::unescape(&t.decode()?)?.into_owned(),
                    ));
                }
            }
            Event::GeneralRef(t) => {
                let entity = format!("&{};", std::str::from_utf8(&t)?);
                let value = quick_xml::escape::unescape(&entity)?.into_owned();
                if let Some(p) = stack.last_mut() {
                    p.children.push(XMLNode::Text(value));
                }
            }
            Event::CData(t) => {
                if let Some(p) = stack.last_mut() {
                    p.children.push(XMLNode::CData(t.decode()?.into_owned()));
                }
            }
            Event::Comment(t) => {
                if let Some(p) = stack.last_mut() {
                    p.children.push(XMLNode::Comment(t.decode()?.into_owned()));
                }
            }
            Event::PI(t) => {
                if let Some(p) = stack.last_mut() {
                    p.children.push(XMLNode::ProcessingInstruction(
                        String::from_utf8(t.target().to_vec())?,
                        Some(String::from_utf8(t.content().to_vec())?),
                    ));
                }
            }
            Event::DocType(_) => bail!("DOCTYPE not admitted"),
            Event::Eof => break,
            _ => (),
        }
    }
    if !stack.is_empty() {
        bail!("unbalanced workbook XML");
    }
    root.context("empty workbook XML")
}

fn path_join(base: &str, target: &str) -> Result<String> {
    let mut stack = vec![];
    let combined = if target.starts_with('/') {
        target.trim_start_matches('/').to_string()
    } else {
        format!(
            "{}/{}",
            base.rsplit_once('/').map(|x| x.0).unwrap_or(""),
            target
        )
    };
    for p in combined.split('/') {
        match p {
            "" | "." => (),
            ".." => {
                if stack.pop().is_none() {
                    bail!("OOXML relationship escapes package");
                }
            }
            _ => stack.push(p),
        }
    }
    Ok(stack.join("/"))
}

pub fn letters(mut c: usize) -> String {
    let mut b = vec![];
    while c > 0 {
        c -= 1;
        b.push((b'A' + (c % 26) as u8) as char);
        c /= 26;
    }
    b.into_iter().rev().collect()
}

pub fn coord(s: &str) -> Result<(usize, usize)> {
    let split = s
        .find(|c: char| c.is_ascii_digit())
        .context("invalid cell coordinate")?;
    let mut col = 0;
    for c in s[..split].bytes() {
        if !c.is_ascii_uppercase() {
            bail!("invalid cell column");
        }
        col = col * 26 + (c - b'A' + 1) as usize;
    }
    let row = s[split..].parse()?;
    if col == 0 || col > 16384 || row == 0 || row > 1048576 {
        bail!("cell coordinate out of range");
    }
    Ok((col, row))
}

fn cell_value(c: &Element, strings: &[Element]) -> Value {
    if child(c, "f").is_some() {
        return Value::String(format!(
            "={}",
            child(c, "f").map(content).unwrap_or_default()
        ));
    }
    cached_cell_value(c, strings)
}

fn cached_cell_value(c: &Element, strings: &[Element]) -> Value {
    match attr(c, "t").as_str() {
        "s" => child(c, "v")
            .and_then(|x| content(x).parse::<usize>().ok())
            .and_then(|i| strings.get(i))
            .map(|x| Value::String(content(x)))
            .unwrap_or(Value::Null),
        "inlineStr" => child(c, "is")
            .map(|x| Value::String(content(x)))
            .unwrap_or(Value::Null),
        "b" => Value::Bool(child(c, "v").is_some_and(|x| content(x) == "1")),
        "str" | "e" => child(c, "v")
            .map(|x| Value::String(content(x)))
            .unwrap_or(Value::Null),
        _ => {
            let s = child(c, "v").map(content).unwrap_or_default();
            if s.is_empty() {
                Value::Null
            } else {
                serde_json::from_str(&s).unwrap_or(Value::String(s))
            }
        }
    }
}

#[derive(Clone)]
pub struct Package {
    parts: BTreeMap<String, Vec<u8>>,
    pub sheets: BTreeMap<String, String>,
    strings: Vec<Element>,
}

impl Package {
    pub fn read(path: &Path) -> Result<Self> {
        Self::from_reader(fs::File::open(path)?)
    }

    pub fn from_reader<R: Read + std::io::Seek>(reader: R) -> Result<Self> {
        let mut zip = zip::ZipArchive::new(reader)?;
        let mut parts = BTreeMap::new();
        let mut total = 0u64;
        for i in 0..zip.len() {
            let mut f = zip.by_index(i)?;
            if f.is_dir() {
                continue;
            }
            let name = f.name().to_string();
            if name.starts_with('/')
                || name.split('/').any(|p| matches!(p, ".." | "." | ""))
                || parts.contains_key(&name)
                || f.size() > 64 * 1024 * 1024
            {
                bail!("invalid or oversized workbook package part");
            }
            total += f.size();
            if total > 512 * 1024 * 1024 {
                bail!("workbook package bound");
            }
            let mut b = vec![];
            f.by_ref().take(64 * 1024 * 1024 + 1).read_to_end(&mut b)?;
            if b.len() > 64 * 1024 * 1024 {
                bail!("workbook part bound");
            }
            parts.insert(name, b);
        }
        let wb = xml(parts.get("xl/workbook.xml").context("missing workbook")?)?;
        let rels = xml(parts
            .get("xl/_rels/workbook.xml.rels")
            .context("missing workbook relationships")?)?;
        let mut sheets = BTreeMap::new();
        for s in children(child(&wb, "sheets").context("missing sheets")?, "sheet") {
            let id = attr(s, "id");
            let r = children(&rels, "Relationship")
                .into_iter()
                .find(|r| attr(r, "Id") == id)
                .context("missing sheet relationship")?;
            if attr(r, "TargetMode") == "External" {
                bail!("external worksheet relationship");
            }
            let name = attr(s, "name");
            if sheets
                .insert(name, path_join("xl/workbook.xml", &attr(r, "Target"))?)
                .is_some()
            {
                bail!("duplicate worksheet name");
            }
        }
        let strings = parts
            .get("xl/sharedStrings.xml")
            .map(|b| xml(b).map(|x| children(&x, "si").into_iter().cloned().collect()))
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            parts,
            sheets,
            strings,
        })
    }

    pub fn cached_value(&self, cell: &Element) -> Value {
        cached_cell_value(cell, &self.strings)
    }

    pub fn preview_styles(&self) -> Result<Option<Element>> {
        let types = self.part("[Content_Types].xml")?;
        for entry in types
            .children
            .iter()
            .filter_map(XMLNode::as_element)
            .filter(|entry| local(&entry.name) == "Override")
        {
            if entry.attributes.get("ContentType").is_some_and(|value| {
                value == "application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"
            }) {
                let name = entry
                    .attributes
                    .get("PartName")
                    .context("styles part lacks name")?;
                let name = name.strip_prefix('/').unwrap_or(name);
                return self.part(name).map(Some);
            }
        }
        Ok(None)
    }

    pub fn part(&self, p: &str) -> Result<Element> {
        let mut element = xml(self.parts.get(p).context("missing workbook XML part")?)?;
        if self.sheets.values().any(|path| path == p) {
            compat::worksheet(&mut element)?;
        }
        Ok(element)
    }

    pub fn sheet_rows_bounded(
        &self,
        name: &str,
        maximum_slots: usize,
    ) -> Result<(Element, Vec<Vec<Value>>)> {
        let e = self.part(self.sheets.get(name).context("missing worksheet")?)?;
        let mut allocated = 0usize;
        let mut out = vec![];
        if let Some(data) = child(&e, "sheetData") {
            for row in children(data, "row") {
                let n: usize = attr(row, "r").parse()?;
                if n > 100000 {
                    bail!("worksheet row bound");
                }
                let rows_added = n.saturating_sub(out.len());
                allocated = allocated
                    .checked_add(rows_added)
                    .context("worksheet dense slot overflow")?;
                if allocated > maximum_slots {
                    bail!("worksheet dense slot bound");
                }
                while out.len() < n {
                    out.push(vec![]);
                }
                for c in children(row, "c") {
                    let (col, _) = coord(&attr(c, "r"))?;
                    let cells_added = col.saturating_sub(out[n - 1].len());
                    allocated = allocated
                        .checked_add(cells_added)
                        .context("worksheet dense slot overflow")?;
                    if allocated > maximum_slots {
                        bail!("worksheet dense slot bound");
                    }
                    while out[n - 1].len() < col {
                        out[n - 1].push(Value::Null);
                    }
                    out[n - 1][col - 1] = cell_value(c, &self.strings);
                }
            }
        }
        Ok((e, out))
    }
}

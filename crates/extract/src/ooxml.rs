//! Office Open XML: DOCX, XLSX, PPTX.

use crate::container::{Zip, rel_target, rels};
use crate::sink::Sink;
use crate::xml::{self, Flow, XmlEvent};
use crate::{ExtractError, Limits, Metadata};

const OFFICE_DOCUMENT: &str = "/officeDocument";

/// Main and core-properties part names of a package.
struct Package {
    main: String,
    core: String,
}

/// Finds the main part (named by the package relationships, or `fallback`)
/// and the core-properties part.
fn package(zip: &mut Zip<'_>, fallback: &str) -> Result<Package, ExtractError> {
    let root = rels(zip, "")?;
    let main = rel_target(&root, OFFICE_DOCUMENT)
        .filter(|t| zip.contains(t))
        .unwrap_or_else(|| fallback.to_owned());
    let core =
        rel_target(&root, "/core-properties").unwrap_or_else(|| "docProps/core.xml".to_owned());
    Ok(Package { main, core })
}

/// Reads the core properties (title, creator, created).
fn core_properties(zip: &mut Zip<'_>, name: &str, meta: &mut Metadata) -> Result<(), ExtractError> {
    let Some(src) = zip.read_xml(name)? else {
        return Ok(());
    };
    let mut current: Option<&'static str> = None;
    let mut value = String::new();
    xml::walk(&src, name, |ev| {
        match ev {
            XmlEvent::Start(t) => {
                current = match t.name() {
                    b"title" => Some("title"),
                    b"creator" => Some("creator"),
                    b"created" => Some("created"),
                    _ => None,
                };
                value.clear();
            }
            XmlEvent::Text(s) if current.is_some() => value.push_str(s),
            XmlEvent::End(_) => {
                let v = value.trim();
                let slot = match current {
                    Some("title") => &mut meta.title,
                    Some("creator") => &mut meta.author,
                    Some("created") => &mut meta.created,
                    _ => return Flow::Continue,
                };
                if slot.is_none() && !v.is_empty() {
                    *slot = Some(v.to_owned());
                }
                current = None;
            }
            _ => {}
        }
        Flow::Continue
    })
}

/// Word: body text, then footnotes and endnotes.
pub(crate) fn docx(
    bytes: &[u8],
    limits: &Limits,
    sink: &mut Sink,
    meta: &mut Metadata,
) -> Result<(), ExtractError> {
    let mut zip = Zip::open(bytes, limits)?;
    let pkg = package(&mut zip, "word/document.xml")?;
    let main = pkg.main;
    let src = zip
        .read_xml(&main)?
        .ok_or_else(|| ExtractError::Corrupt("docx: no main document part".into()))?;
    docx_part(&src, &main, sink)?;

    let part_rels = rels(&mut zip, &main)?;
    for suffix in ["/footnotes", "/endnotes"] {
        if sink.is_full() {
            break;
        }
        if let Some(part) = rel_target(&part_rels, suffix) {
            if let Some(src) = zip.read_xml(&part)? {
                sink.newline();
                docx_part(&src, &part, sink)?;
            }
        }
    }
    core_properties(&mut zip, &pkg.core, meta)
}

/// Text of one WordprocessingML part. Deleted revisions (`w:delText`) and
/// field codes (`w:instrText`) are skipped: only `w:t` carries text.
fn docx_part(src: &str, name: &str, sink: &mut Sink) -> Result<(), ExtractError> {
    let mut in_text = false;
    // Inside table cells paragraphs are separated by spaces, so that each
    // table row stays on one line.
    let mut cell_depth = 0usize;
    xml::walk(src, name, |ev| {
        match ev {
            XmlEvent::Start(t) => match t.name() {
                b"t" => in_text = true,
                b"tc" => cell_depth += 1,
                b"tab" | b"ptab" => sink.space(),
                b"br" | b"cr" => sink.newline(),
                _ => {}
            },
            XmlEvent::Empty(t) => match t.name() {
                b"tab" | b"ptab" => sink.space(),
                b"br" | b"cr" | b"p" => sink.newline(),
                b"noBreakHyphen" => sink.text("-"),
                _ => {}
            },
            XmlEvent::End(name) => match name {
                b"t" => in_text = false,
                b"p" if cell_depth > 0 => sink.space(),
                b"p" | b"tr" => sink.newline(),
                b"tc" => {
                    cell_depth = cell_depth.saturating_sub(1);
                    sink.space();
                }
                _ => {}
            },
            XmlEvent::Text(s) if in_text => sink.text(s),
            XmlEvent::Text(_) => {}
        }
        if sink.is_full() {
            Flow::Stop
        } else {
            Flow::Continue
        }
    })
}

/// Excel: for each sheet in workbook order, its name on a line, then one
/// line per row with cell values separated by spaces. Shared strings,
/// inline strings, formula string results and numbers are included;
/// booleans and errors are not.
pub(crate) fn xlsx(
    bytes: &[u8],
    limits: &Limits,
    sink: &mut Sink,
    meta: &mut Metadata,
) -> Result<(), ExtractError> {
    let mut zip = Zip::open(bytes, limits)?;
    let pkg = package(&mut zip, "xl/workbook.xml")?;
    let workbook = pkg.main;
    let src = zip
        .read_xml(&workbook)?
        .ok_or_else(|| ExtractError::Corrupt("xlsx: no workbook part".into()))?;

    let mut sheets: Vec<(String, String)> = Vec::new();
    xml::walk(&src, &workbook, |ev| {
        if let XmlEvent::Start(t) | XmlEvent::Empty(t) = ev {
            if t.name() == b"sheet" {
                let name = t.attr(b"name").unwrap_or_default();
                let rid = t
                    .attr_qualified(b"r:id")
                    .or_else(|| t.attr(b"id"))
                    .unwrap_or_default();
                sheets.push((name, rid));
            }
        }
        Flow::Continue
    })?;

    let wb_rels = rels(&mut zip, &workbook)?;
    let strings_part =
        rel_target(&wb_rels, "/sharedStrings").unwrap_or_else(|| "xl/sharedStrings.xml".to_owned());
    let shared = match zip.read_xml(&strings_part)? {
        Some(src) => shared_strings(&src, &strings_part, limits)?,
        None => Vec::new(),
    };

    for (name, rid) in &sheets {
        if sink.is_full() {
            break;
        }
        let Some(part) = wb_rels
            .iter()
            .find(|r| &r.id == rid && !r.external)
            .map(|r| r.target.clone())
        else {
            continue;
        };
        let Some(src) = zip.read_xml(&part)? else {
            continue;
        };
        sink.newline();
        sink.text(name);
        sink.newline();
        sheet(&src, &part, &shared, sink)?;
    }
    core_properties(&mut zip, &pkg.core, meta)
}

/// The shared-string table. Phonetic runs (`rPh`) are skipped. The table
/// is bounded by the size of its zip entry; strings beyond the output
/// budget are not kept.
fn shared_strings(src: &str, name: &str, limits: &Limits) -> Result<Vec<String>, ExtractError> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut kept_chars = 0usize;
    let (mut in_t, mut in_phonetic) = (false, false);
    xml::walk(src, name, |ev| {
        match ev {
            XmlEvent::Start(t) => match t.name() {
                b"t" => in_t = true,
                b"rPh" => in_phonetic = true,
                _ => {}
            },
            XmlEvent::Empty(t) if t.name() == b"si" => out.push(String::new()),
            XmlEvent::End(n) => match n {
                b"t" => in_t = false,
                b"rPh" => in_phonetic = false,
                b"si" => {
                    kept_chars = kept_chars.saturating_add(current.len());
                    out.push(std::mem::take(&mut current));
                }
                _ => {}
            },
            XmlEvent::Text(s)
                if in_t
                    && !in_phonetic
                    && kept_chars.saturating_add(current.len())
                        < limits.max_output_chars.saturating_mul(4) =>
            {
                current.push_str(s);
            }
            _ => {}
        }
        Flow::Continue
    })?;
    Ok(out)
}

/// One worksheet.
fn sheet(src: &str, name: &str, shared: &[String], sink: &mut Sink) -> Result<(), ExtractError> {
    let mut cell_type = String::new();
    let mut value = String::new();
    let (mut in_v, mut in_inline_t) = (false, false);
    xml::walk(src, name, |ev| {
        match ev {
            XmlEvent::Start(t) => match t.name() {
                b"c" => {
                    cell_type = t.attr(b"t").unwrap_or_default();
                    value.clear();
                }
                b"v" => in_v = true,
                b"t" => in_inline_t = true,
                _ => {}
            },
            XmlEvent::Text(s) if in_v || in_inline_t => value.push_str(s),
            XmlEvent::End(n) => match n {
                b"v" => in_v = false,
                b"t" => in_inline_t = false,
                b"c" => {
                    let text = match cell_type.as_str() {
                        "s" => value
                            .trim()
                            .parse::<usize>()
                            .ok()
                            .and_then(|i| shared.get(i))
                            .map(String::as_str),
                        "b" | "e" => None,
                        _ => Some(value.as_str()),
                    };
                    if let Some(text) = text.filter(|t| !t.trim().is_empty()) {
                        sink.space();
                        sink.text(text);
                    }
                    value.clear();
                }
                b"row" => sink.newline(),
                _ => {}
            },
            _ => {}
        }
        if sink.is_full() {
            Flow::Stop
        } else {
            Flow::Continue
        }
    })
}

/// PowerPoint: slides in presentation order, one line per paragraph.
pub(crate) fn pptx(
    bytes: &[u8],
    limits: &Limits,
    sink: &mut Sink,
    meta: &mut Metadata,
) -> Result<(), ExtractError> {
    let mut zip = Zip::open(bytes, limits)?;
    let pkg = package(&mut zip, "ppt/presentation.xml")?;
    let presentation = pkg.main;
    let mut slide_ids: Vec<String> = Vec::new();
    if let Some(src) = zip.read_xml(&presentation)? {
        xml::walk(&src, &presentation, |ev| {
            if let XmlEvent::Start(t) | XmlEvent::Empty(t) = ev {
                if t.name() == b"sldId" {
                    if let Some(rid) = t.attr_qualified(b"r:id").or_else(|| t.attr(b"id")) {
                        slide_ids.push(rid);
                    }
                }
            }
            Flow::Continue
        })?;
    }
    let pres_rels = rels(&mut zip, &presentation)?;
    let mut slides: Vec<String> = slide_ids
        .iter()
        .filter_map(|rid| pres_rels.iter().find(|r| &r.id == rid && !r.external))
        .map(|r| r.target.clone())
        .collect();
    if slides.is_empty() {
        slides = numbered_slides(&zip);
    }
    if slides.is_empty() && !zip.contains(&presentation) {
        return Err(ExtractError::Corrupt("pptx: no presentation part".into()));
    }

    for part in &slides {
        if sink.is_full() {
            break;
        }
        let Some(src) = zip.read_xml(part)? else {
            continue;
        };
        sink.newline();
        slide(&src, part, sink)?;
    }
    core_properties(&mut zip, &pkg.core, meta)
}

/// `ppt/slides/slideN.xml` parts sorted by N, for packages whose
/// presentation part lists no slides.
fn numbered_slides(zip: &Zip<'_>) -> Vec<String> {
    let mut numbered: Vec<(u32, String)> = zip
        .names()
        .filter_map(|n| {
            let number = n.strip_prefix("ppt/slides/slide")?.strip_suffix(".xml")?;
            Some((number.parse().ok()?, n.to_owned()))
        })
        .collect();
    numbered.sort();
    numbered.into_iter().map(|(_, n)| n).collect()
}

fn slide(src: &str, name: &str, sink: &mut Sink) -> Result<(), ExtractError> {
    let mut in_t = false;
    xml::walk(src, name, |ev| {
        match ev {
            XmlEvent::Start(t) if t.name() == b"t" => in_t = true,
            XmlEvent::Empty(t) if t.name() == b"br" => sink.newline(),
            XmlEvent::End(n) => match n {
                b"t" => in_t = false,
                b"p" => sink.newline(),
                b"tc" => sink.space(),
                _ => {}
            },
            XmlEvent::Text(s) if in_t => sink.text(s),
            _ => {}
        }
        if sink.is_full() {
            Flow::Stop
        } else {
            Flow::Continue
        }
    })
}

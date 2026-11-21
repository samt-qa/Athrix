//! OpenDocument: ODT, ODS, ODP.

use crate::container::Zip;
use crate::sink::Sink;
use crate::xml::{self, Flow, XmlEvent};
use crate::{ExtractError, Kind, Limits, Metadata};

/// Extracts `content.xml`: paragraphs and headings end with a line break,
/// table cells are separated by spaces and rows by line breaks; for
/// spreadsheets each sheet name is put on its own line, for presentations
/// each slide starts a new line. Tracked deletions are skipped.
pub(crate) fn extract(
    kind: Kind,
    bytes: &[u8],
    limits: &Limits,
    sink: &mut Sink,
    meta: &mut Metadata,
) -> Result<(), ExtractError> {
    let mut zip = Zip::open(bytes, limits)?;
    if let Some(manifest) = zip.read("META-INF/manifest.xml")? {
        if crate::find(&manifest, b"encryption-data").is_some() {
            return Err(ExtractError::Encrypted);
        }
    }
    let src = zip
        .read_xml("content.xml")?
        .ok_or_else(|| ExtractError::Corrupt(format!("{}: no content.xml", kind.name())))?;

    let mut skip_depth = 0usize;
    // Inside table cells paragraphs are separated by spaces, so that each
    // table row stays on one line.
    let mut cell_depth = 0usize;
    xml::walk(&src, "content.xml", |ev| {
        if skip_depth > 0 {
            match ev {
                XmlEvent::Start(_) => skip_depth += 1,
                XmlEvent::End(_) => skip_depth -= 1,
                _ => {}
            }
            return Flow::Continue;
        }
        match ev {
            XmlEvent::Start(t) => match t.name() {
                b"tracked-changes" | b"sequence-decls" | b"variable-decls" | b"forms" => {
                    skip_depth = 1;
                }
                b"table" if kind == Kind::Ods => {
                    sink.newline();
                    if let Some(name) = t.attr(b"name") {
                        sink.text(&name);
                        sink.newline();
                    }
                }
                b"table-cell" | b"covered-table-cell" => cell_depth += 1,
                b"page" => sink.newline(),
                b"line-break" => sink.newline(),
                b"s" | b"tab" => sink.space(),
                _ => {}
            },
            XmlEvent::Empty(t) => match t.name() {
                b"p" | b"h" if cell_depth > 0 => sink.space(),
                b"line-break" | b"p" | b"h" => sink.newline(),
                b"s" | b"tab" | b"table-cell" | b"covered-table-cell" => sink.space(),
                _ => {}
            },
            XmlEvent::End(name) => match name {
                b"p" | b"h" | b"list-item" if cell_depth > 0 => sink.space(),
                b"p" | b"h" | b"table-row" | b"page" | b"list-item" => sink.newline(),
                b"table-cell" | b"covered-table-cell" => {
                    cell_depth = cell_depth.saturating_sub(1);
                    sink.space();
                }
                _ => {}
            },
            XmlEvent::Text(s) => sink.text(s),
        }
        if sink.is_full() {
            Flow::Stop
        } else {
            Flow::Continue
        }
    })?;

    if let Some(src) = zip.read_xml("meta.xml")? {
        metadata(&src, meta)?;
    }
    Ok(())
}

/// `meta.xml`: `dc:title`, `meta:initial-creator` (else `dc:creator`),
/// `meta:creation-date`.
fn metadata(src: &str, meta: &mut Metadata) -> Result<(), ExtractError> {
    let mut current: Option<&'static str> = None;
    let mut value = String::new();
    let mut creator = None;
    xml::walk(src, "meta.xml", |ev| {
        match ev {
            XmlEvent::Start(t) => {
                current = match t.name() {
                    b"title" => Some("title"),
                    b"initial-creator" => Some("initial-creator"),
                    b"creator" => Some("creator"),
                    b"creation-date" => Some("created"),
                    _ => None,
                };
                value.clear();
            }
            XmlEvent::Text(s) if current.is_some() => value.push_str(s),
            XmlEvent::End(_) => {
                let v = value.trim();
                let slot = match current {
                    Some("title") => &mut meta.title,
                    Some("initial-creator") => &mut meta.author,
                    Some("creator") => &mut creator,
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
    })?;
    if meta.author.is_none() {
        meta.author = creator;
    }
    Ok(())
}

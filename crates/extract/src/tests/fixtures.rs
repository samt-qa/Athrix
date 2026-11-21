//! Test documents built in code (no binary fixtures in the repository).

use std::io::{Cursor, Write};

use zip::CompressionMethod;
use zip::write::{SimpleFileOptions, ZipWriter};

pub const AR: &str = "السَّلامُ عَلَيْكُم ورحمة الله";
pub const AR2: &str = "مرحبا بالعالم";
pub const EN: &str = "Hello world";

/// Escapes text for XML content.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A zip with the given entries, deflated (or stored when `stored`).
pub fn zip_with(entries: &[(&str, Vec<u8>)], stored: bool) -> Vec<u8> {
    let mut w = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, data) in entries {
        let method = if stored || *name == "mimetype" {
            CompressionMethod::Stored
        } else {
            CompressionMethod::Deflated
        };
        let options = SimpleFileOptions::default().compression_method(method);
        w.start_file(*name, options).unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap().into_inner()
}

fn s(x: &str) -> Vec<u8> {
    x.as_bytes().to_vec()
}

const RELS_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const REL_DOC: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";
const REL_CORE: &str =
    "http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties";

fn root_rels(main: &str) -> Vec<u8> {
    s(&format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="{RELS_NS}"><Relationship Id="rId1" Type="{REL_DOC}" Target="{main}"/><Relationship Id="rId2" Type="{REL_CORE}" Target="docProps/core.xml"/></Relationships>"#
    ))
}

fn core(title: &str, creator: &str) -> Vec<u8> {
    s(&format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"><dc:title>{}</dc:title><dc:creator>{}</dc:creator><dcterms:created xsi:type="dcterms:W3CDTF">2025-03-01T10:00:00Z</dcterms:created></cp:coreProperties>"#,
        esc(title),
        esc(creator)
    ))
}

/// A DOCX whose body has one paragraph per item; each paragraph's text is
/// split into runs at `|`.
pub fn docx(paragraphs: &[&str], stored: bool) -> Vec<u8> {
    let mut body = String::new();
    for p in paragraphs {
        body.push_str("<w:p><w:pPr><w:bidi/></w:pPr>");
        for run in p.split('|') {
            body.push_str(&format!(
                r#"<w:r><w:t xml:space="preserve">{}</w:t></w:r>"#,
                esc(run)
            ));
        }
        body.push_str("</w:p>");
    }
    docx_raw(&body, stored)
}

/// A DOCX with a raw `w:body` content.
pub fn docx_raw(body: &str, stored: bool) -> Vec<u8> {
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}<w:sectPr/></w:body></w:document>"#
    );
    zip_with(
        &[
            (
                "[Content_Types].xml",
                s(
                    r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#,
                ),
            ),
            ("_rels/.rels", root_rels("word/document.xml")),
            ("word/document.xml", s(&document)),
            ("docProps/core.xml", core("تقرير", "سامي")),
        ],
        stored,
    )
}

/// A cell for [`xlsx`].
pub enum Cell<'a> {
    Shared(&'a str),
    Inline(&'a str),
    Number(&'a str),
}

/// An XLSX with the given sheets (name, rows). Shared strings are stored
/// in reverse order of first use to check index resolution.
pub fn xlsx(sheets: &[(&str, Vec<Vec<Cell<'_>>>)]) -> Vec<u8> {
    let mut shared: Vec<String> = Vec::new();
    for (_, rows) in sheets {
        for row in rows {
            for c in row {
                if let Cell::Shared(t) = c {
                    if !shared.iter().any(|x| x == t) {
                        shared.push((*t).to_owned());
                    }
                }
            }
        }
    }
    shared.reverse();
    let mut entries = vec![
        ("[Content_Types].xml".to_owned(), s("<Types/>")),
        ("_rels/.rels".to_owned(), root_rels("xl/workbook.xml")),
        ("docProps/core.xml".to_owned(), core("جدول", "Ann")),
    ];
    let mut wb_sheets = String::new();
    let mut wb_rels = String::new();
    for (i, (name, rows)) in sheets.iter().enumerate() {
        let n = i + 1;
        wb_sheets.push_str(&format!(
            r#"<sheet name="{}" sheetId="{n}" r:id="rId{n}"/>"#,
            esc(name)
        ));
        wb_rels.push_str(&format!(
            r#"<Relationship Id="rId{n}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet{n}.xml"/>"#
        ));
        let mut data = String::new();
        for (r, row) in rows.iter().enumerate() {
            data.push_str(&format!(r#"<row r="{}">"#, r + 1));
            for c in row {
                match c {
                    Cell::Shared(t) => {
                        let idx = shared.iter().position(|x| x == t).unwrap();
                        data.push_str(&format!(r#"<c t="s"><v>{idx}</v></c>"#));
                    }
                    Cell::Inline(t) => data.push_str(&format!(
                        r#"<c t="inlineStr"><is><t>{}</t></is></c>"#,
                        esc(t)
                    )),
                    Cell::Number(v) => data.push_str(&format!("<c><f>SUM(A1)</f><v>{v}</v></c>")),
                }
            }
            data.push_str("</row>");
        }
        entries.push((
            format!("xl/worksheets/sheet{n}.xml"),
            s(&format!(
                r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{data}</sheetData></worksheet>"#
            )),
        ));
    }
    wb_rels.push_str(r#"<Relationship Id="rIdS" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>"#);
    entries.push((
        "xl/workbook.xml".to_owned(),
        s(&format!(
            r#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets>{wb_sheets}</sheets></workbook>"#
        )),
    ));
    entries.push((
        "xl/_rels/workbook.xml.rels".to_owned(),
        s(&format!(
            r#"<Relationships xmlns="{RELS_NS}">{wb_rels}</Relationships>"#
        )),
    ));
    let sst: String = shared
        .iter()
        .map(|t| format!("<si><t>{}</t><rPh><t>PHONETIC</t></rPh></si>", esc(t)))
        .collect();
    entries.push((
        "xl/sharedStrings.xml".to_owned(),
        s(&format!(
            r#"<?xml version="1.0"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">{sst}</sst>"#
        )),
    ));
    let refs: Vec<(&str, Vec<u8>)> = entries
        .iter()
        .map(|(n, d)| (n.as_str(), d.clone()))
        .collect();
    zip_with(&refs, false)
}

/// A PPTX with one slide per item (paragraphs per slide). Slide files are
/// numbered in reverse of presentation order, to check that order comes
/// from `presentation.xml`.
pub fn pptx(slides: &[&[&str]]) -> Vec<u8> {
    let n = slides.len();
    let mut entries = vec![
        ("[Content_Types].xml".to_owned(), s("<Types/>")),
        ("_rels/.rels".to_owned(), root_rels("ppt/presentation.xml")),
        ("docProps/core.xml".to_owned(), core("عرض", "Bob")),
    ];
    let mut ids = String::new();
    let mut rels = String::new();
    for (i, paras) in slides.iter().enumerate() {
        let file_no = n - i;
        ids.push_str(&format!(
            r#"<p:sldId id="{}" r:id="rId{}"/>"#,
            256 + i,
            i + 10
        ));
        rels.push_str(&format!(
            r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide{file_no}.xml"/>"#,
            i + 10
        ));
        let body: String = paras
            .iter()
            .map(|p| format!("<a:p><a:r><a:t>{}</a:t></a:r></a:p>", esc(p)))
            .collect();
        entries.push((
            format!("ppt/slides/slide{file_no}.xml"),
            s(&format!(
                r#"<?xml version="1.0"?><p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody>{body}</p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
            )),
        ));
    }
    entries.push((
        "ppt/presentation.xml".to_owned(),
        s(&format!(
            r#"<?xml version="1.0"?><p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst>{ids}</p:sldIdLst></p:presentation>"#
        )),
    ));
    entries.push((
        "ppt/_rels/presentation.xml.rels".to_owned(),
        s(&format!(
            r#"<Relationships xmlns="{RELS_NS}">{rels}</Relationships>"#
        )),
    ));
    let refs: Vec<(&str, Vec<u8>)> = entries
        .iter()
        .map(|(n, d)| (n.as_str(), d.clone()))
        .collect();
    zip_with(&refs, false)
}

/// An OpenDocument package with the given `office:body` content.
pub fn odf(mime: &str, body: &str, extra: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0"><office:body>{body}</office:body></office:document-content>"#
    );
    let meta = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-meta xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0" xmlns:dc="http://purl.org/dc/elements/1.1/"><office:meta><dc:title>وثيقة</dc:title><meta:initial-creator>ليلى</meta:initial-creator><meta:creation-date>2024-05-06T07:08:09</meta:creation-date></office:meta></office:document-meta>"#;
    let mut entries = vec![
        ("mimetype", s(mime)),
        ("content.xml", s(&content)),
        ("meta.xml", s(meta)),
        (
            "META-INF/manifest.xml",
            s(
                r#"<?xml version="1.0"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"><manifest:file-entry manifest:full-path="/" manifest:media-type="x"/></manifest:manifest>"#,
            ),
        ),
    ];
    entries.extend(extra.iter().map(|(n, d)| (*n, d.clone())));
    zip_with(&entries, false)
}

/// An EPUB with chapters (title, paragraphs). The manifest lists chapters
/// in reverse; the spine gives the reading order.
pub fn epub(chapters: &[(&str, &[&str])]) -> Vec<u8> {
    let mut manifest = String::new();
    let mut spine = String::new();
    let mut entries = vec![
        ("mimetype".to_owned(), s("application/epub+zip")),
        (
            "META-INF/container.xml".to_owned(),
            s(
                r#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
            ),
        ),
    ];
    for (i, (title, paras)) in chapters.iter().enumerate().rev() {
        manifest.push_str(&format!(
            r#"<item id="c{i}" href="text/ch%20{i}.xhtml" media-type="application/xhtml+xml"/>"#
        ));
        let body: String = paras.iter().map(|p| format!("<p>{}</p>", esc(p))).collect();
        entries.push((
            format!("OEBPS/text/ch {i}.xhtml"),
            s(&format!(
                r#"<?xml version="1.0" encoding="utf-8"?><!DOCTYPE html><html xmlns="http://www.w3.org/1999/xhtml" dir="rtl"><head><title>{t}</title><style>p {{ color: red }}</style></head><body><h1>{t}</h1>{body}</body></html>"#,
                t = esc(title)
            )),
        ));
    }
    for i in 0..chapters.len() {
        spine.push_str(&format!(r#"<itemref idref="c{i}"/>"#));
    }
    entries.push((
        "OEBPS/content.opf".to_owned(),
        s(&format!(
            r#"<?xml version="1.0" encoding="utf-8"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>كتاب الاختبار</dc:title><dc:creator>المؤلف</dc:creator><dc:date>2020-01-01</dc:date></metadata><manifest>{manifest}</manifest><spine>{spine}</spine></package>"#
        )),
    ));
    let refs: Vec<(&str, Vec<u8>)> = entries
        .iter()
        .map(|(n, d)| (n.as_str(), d.clone()))
        .collect();
    zip_with(&refs, false)
}

/// Base64 encoding for building test mails.
pub fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for k in 0..4 {
            if k <= chunk.len() {
                out.push(T[((n >> (18 - 6 * k)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

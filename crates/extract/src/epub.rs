//! EPUB 2/3: documents in spine (reading) order.

use std::collections::{HashMap, HashSet};

use crate::container::{Zip, resolve, split_dir};
use crate::sink::Sink;
use crate::xml::{self, Flow, XmlEvent};
use crate::{ExtractError, Limits, Metadata, html};

/// Algorithms used to obfuscate embedded fonts; they do not hide text.
const FONT_OBFUSCATION: &[&str] = &[
    "http://www.idpf.org/2008/embedding",
    "http://ns.adobe.com/pdf/enc#RC",
];

pub(crate) fn extract(
    bytes: &[u8],
    limits: &Limits,
    sink: &mut Sink,
    meta: &mut Metadata,
) -> Result<(), ExtractError> {
    let mut zip = Zip::open(bytes, limits)?;
    let encrypted = encrypted_resources(&mut zip)?;

    let container = zip
        .read_xml("META-INF/container.xml")?
        .ok_or_else(|| ExtractError::Corrupt("epub: no META-INF/container.xml".into()))?;
    let mut opf_path = None;
    xml::walk(&container, "container.xml", |ev| {
        if let XmlEvent::Start(t) | XmlEvent::Empty(t) = ev {
            if t.name() == b"rootfile" {
                opf_path = t.attr(b"full-path");
                if opf_path.is_some() {
                    return Flow::Stop;
                }
            }
        }
        Flow::Continue
    })?;
    let opf_path = resolve(
        "",
        &opf_path.ok_or_else(|| ExtractError::Corrupt("epub: no rootfile".into()))?,
    );
    let opf = zip
        .read_xml(&opf_path)?
        .ok_or_else(|| ExtractError::Corrupt(format!("epub: missing package {opf_path}")))?;
    let package = Package::parse(&opf, &opf_path, meta)?;
    let (opf_dir, _) = split_dir(&opf_path);

    let mut any = false;
    for idref in &package.spine {
        if sink.is_full() {
            break;
        }
        let Some(href) = package.manifest.get(idref) else {
            continue;
        };
        let path = resolve(opf_dir, href);
        if encrypted.contains(&path) {
            return Err(ExtractError::Encrypted);
        }
        let Some(doc) = zip.read(&path)? else {
            continue;
        };
        any = true;
        let (src, _) = html::decode(&doc);
        sink.newline();
        html::to_text(&src, sink, &mut Metadata::default());
    }
    if !any && !package.spine.is_empty() {
        return Err(ExtractError::Corrupt(
            "epub: no spine document found".into(),
        ));
    }
    Ok(())
}

/// Resources listed in `META-INF/encryption.xml` with a real (non-font)
/// encryption algorithm.
fn encrypted_resources(zip: &mut Zip<'_>) -> Result<HashSet<String>, ExtractError> {
    let mut out = HashSet::new();
    let Some(src) = zip.read_xml("META-INF/encryption.xml")? else {
        return Ok(out);
    };
    let mut algorithm = String::new();
    xml::walk(&src, "encryption.xml", |ev| {
        if let XmlEvent::Start(t) | XmlEvent::Empty(t) = ev {
            match t.name() {
                b"EncryptionMethod" => algorithm = t.attr(b"Algorithm").unwrap_or_default(),
                b"CipherReference" if !FONT_OBFUSCATION.contains(&algorithm.as_str()) => {
                    if let Some(uri) = t.attr(b"URI") {
                        out.insert(resolve("", &uri));
                    }
                }
                _ => {}
            }
        }
        Flow::Continue
    })?;
    Ok(out)
}

/// The parts of the OPF package document we need.
struct Package {
    /// Manifest item id to href.
    manifest: HashMap<String, String>,
    /// Item ids in reading order.
    spine: Vec<String>,
}

impl Package {
    fn parse(src: &str, name: &str, meta: &mut Metadata) -> Result<Self, ExtractError> {
        let mut manifest = HashMap::new();
        let mut spine = Vec::new();
        let mut current: Option<&'static str> = None;
        let mut value = String::new();
        xml::walk(src, name, |ev| {
            let is_start = matches!(ev, XmlEvent::Start(_));
            match ev {
                XmlEvent::Start(t) | XmlEvent::Empty(t) => match t.name() {
                    b"item" => {
                        if let (Some(id), Some(href)) = (t.attr(b"id"), t.attr(b"href")) {
                            manifest.insert(id, href);
                        }
                    }
                    b"itemref" => {
                        if let Some(id) = t.attr(b"idref") {
                            spine.push(id);
                        }
                    }
                    b"title" if is_start => current = Some("title"),
                    b"creator" if is_start => current = Some("creator"),
                    b"date" if is_start => current = Some("date"),
                    _ => current = None,
                },
                XmlEvent::Text(s) if current.is_some() => value.push_str(s),
                XmlEvent::Text(_) => {}
                XmlEvent::End(_) => {
                    let v = value.split_whitespace().collect::<Vec<_>>().join(" ");
                    value.clear();
                    let slot = match current.take() {
                        Some("title") => &mut meta.title,
                        Some("creator") => &mut meta.author,
                        Some("date") => &mut meta.created,
                        _ => return Flow::Continue,
                    };
                    if slot.is_none() && !v.is_empty() {
                        *slot = Some(v);
                    }
                }
            }
            Flow::Continue
        })?;
        Ok(Package { manifest, spine })
    }
}

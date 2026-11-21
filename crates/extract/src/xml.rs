//! Lenient streaming XML walker on top of `quick-xml`.

use std::borrow::Cow;

use encoding_rs::{Encoding, UTF_8};
use quick_xml::Reader;
use quick_xml::events::{BytesRef, BytesStart, Event};

use crate::ExtractError;

/// One XML event, with element names reduced to their local part (the
/// namespace prefix is ignored, so `w:t` and `a:t` are both `t`).
pub(crate) enum XmlEvent<'a, 'b> {
    /// Opening tag.
    Start(&'b Tag<'a>),
    /// Self-closing tag (`<br/>`).
    Empty(&'b Tag<'a>),
    /// Closing tag, by local name.
    End(&'b [u8]),
    /// Character data with entities resolved.
    Text(&'b str),
}

/// An opening tag.
pub(crate) struct Tag<'a>(BytesStart<'a>);

impl Tag<'_> {
    /// Local name (without namespace prefix).
    pub(crate) fn name(&self) -> &[u8] {
        local(self.0.name().into_inner())
    }

    /// Value of the first attribute with this local name, unescaped.
    pub(crate) fn attr(&self, local_name: &[u8]) -> Option<String> {
        self.0
            .attributes()
            .with_checks(false)
            .flatten()
            .find(|a| local(a.key.into_inner()) == local_name)
            .map(|a| match a.unescape_value() {
                Ok(v) => v.into_owned(),
                Err(_) => String::from_utf8_lossy(&a.value).into_owned(),
            })
    }

    /// Value of the attribute with this exact qualified name.
    pub(crate) fn attr_qualified(&self, name: &[u8]) -> Option<String> {
        self.0
            .attributes()
            .with_checks(false)
            .flatten()
            .find(|a| a.key.into_inner() == name)
            .map(|a| match a.unescape_value() {
                Ok(v) => v.into_owned(),
                Err(_) => String::from_utf8_lossy(&a.value).into_owned(),
            })
    }
}

fn local(qname: &[u8]) -> &[u8] {
    match qname.iter().rposition(|&b| b == b':') {
        Some(i) => &qname[i + 1..],
        None => qname,
    }
}

/// Whether to keep walking.
#[derive(PartialEq, Eq)]
pub(crate) enum Flow {
    Continue,
    Stop,
}

/// Walks `xml`, calling `f` for every event, until the end of the
/// document or until `f` returns [`Flow::Stop`]. Mismatched end tags are
/// tolerated; any other syntax error fails with [`ExtractError::Corrupt`].
pub(crate) fn walk(
    xml: &str,
    what: &str,
    mut f: impl FnMut(XmlEvent<'_, '_>) -> Flow,
) -> Result<(), ExtractError> {
    let mut reader = Reader::from_str(xml);
    let config = reader.config_mut();
    config.trim_text(false);
    config.check_end_names = false;
    config.allow_unmatched_ends = true;
    config.allow_dangling_amp = true;
    config.check_comments = false;
    config.expand_empty_elements = false;

    loop {
        let event = reader
            .read_event()
            .map_err(|e| ExtractError::Corrupt(format!("{what}: {e}")))?;
        let flow = match event {
            Event::Start(e) => f(XmlEvent::Start(&Tag(e))),
            Event::Empty(e) => f(XmlEvent::Empty(&Tag(e))),
            Event::End(e) => f(XmlEvent::End(local(e.name().into_inner()))),
            Event::Text(e) => {
                let text = e
                    .decode()
                    .unwrap_or_else(|_| String::from_utf8_lossy(&e).into_owned().into());
                f(XmlEvent::Text(&text))
            }
            Event::CData(e) => {
                let text = String::from_utf8_lossy(&e);
                f(XmlEvent::Text(&text))
            }
            Event::GeneralRef(e) => match resolve_ref(&e) {
                Some(text) => f(XmlEvent::Text(&text)),
                None => Flow::Continue,
            },
            Event::Eof => return Ok(()),
            _ => Flow::Continue,
        };
        if flow == Flow::Stop {
            return Ok(());
        }
    }
}

/// Resolves `&...;`: character references, XML's predefined entities, and
/// HTML named entities (XHTML in EPUBs uses `&nbsp;` and friends).
fn resolve_ref(r: &BytesRef<'_>) -> Option<Cow<'static, str>> {
    if r.is_char_ref() {
        return match r.resolve_char_ref() {
            Ok(Some(c)) => Some(Cow::Owned(c.to_string())),
            _ => None,
        };
    }
    let name = std::str::from_utf8(r).ok()?;
    crate::html::named_entity(name).map(Cow::Borrowed)
}

/// Decodes an XML document to UTF-8: BOM first, then the encoding named in
/// the XML declaration, else UTF-8 (invalid sequences become U+FFFD).
pub(crate) fn decode(bytes: &[u8]) -> Cow<'_, str> {
    if let Some((encoding, bom)) = Encoding::for_bom(bytes) {
        return encoding.decode_without_bom_handling(&bytes[bom..]).0;
    }
    let encoding = declared_encoding(bytes).unwrap_or(UTF_8);
    encoding.decode_without_bom_handling(bytes).0
}

/// The `encoding="..."` of an XML declaration at the start of `bytes`.
fn declared_encoding(bytes: &[u8]) -> Option<&'static Encoding> {
    let head = &bytes[..bytes.len().min(256)];
    if !head.starts_with(b"<?xml") {
        return None;
    }
    let end = head.iter().position(|&b| b == b'>')?;
    let decl = &head[..end];
    let at = crate::find(decl, b"encoding")? + b"encoding".len();
    let rest = &decl[at..];
    let quote = rest.iter().position(|&b| b == b'"' || b == b'\'')?;
    let q = rest[quote];
    let value = &rest[quote + 1..];
    let close = value.iter().position(|&b| b == q)?;
    let encoding = Encoding::for_label(&value[..close])?;
    // A UTF-16 label on bytes without a BOM is not trustworthy.
    (encoding.output_encoding() == encoding).then_some(encoding)
}

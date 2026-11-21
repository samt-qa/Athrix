//! E-mail (RFC 5322 / MIME): headers, body parts, transfer encodings and
//! RFC 2047 encoded words.

use encoding_rs::{Encoding, UTF_8};

use crate::container::hex;
use crate::sink::Sink;
use crate::{ExtractError, Limits, Metadata, html};

/// Most header fields kept per entity; the rest are ignored.
const MAX_HEADERS: usize = 1000;
/// Longest encoded-word payload looked for (RFC 2047 allows 75 bytes per
/// word; real mailers sometimes exceed it).
const MAX_WORD: usize = 1024;
/// Longest header value kept, in bytes.
const MAX_HEADER_BYTES: usize = 64 * 1024;

type Headers = Vec<(String, Vec<u8>)>;

struct Walker<'l> {
    limits: &'l Limits,
    parts: usize,
}

/// Extracts the Subject, From, To and Cc values (one per line), then the
/// readable body: `text/plain` parts, or `text/html` where no plain
/// alternative exists. Attachments are skipped; attached messages
/// (`message/rfc822`) are walked like the outer message.
pub(crate) fn extract(
    bytes: &[u8],
    limits: &Limits,
    sink: &mut Sink,
    meta: &mut Metadata,
) -> Result<(), ExtractError> {
    let (headers, body) = split_entity(bytes);
    if headers.is_empty() {
        return Err(ExtractError::Corrupt("eml: no header fields".into()));
    }
    meta.title = header(&headers, "subject").filter(|s| !s.is_empty());
    meta.author = header(&headers, "from").filter(|s| !s.is_empty());
    meta.created = header(&headers, "date").filter(|s| !s.is_empty());
    let mut walker = Walker { limits, parts: 0 };
    walker.message(&headers, body, 0, sink)
}

impl Walker<'_> {
    fn message(
        &mut self,
        headers: &Headers,
        body: &[u8],
        depth: usize,
        sink: &mut Sink,
    ) -> Result<(), ExtractError> {
        for name in ["subject", "from", "to", "cc"] {
            if let Some(value) = header(headers, name) {
                sink.newline();
                sink.text(&value);
                sink.newline();
            }
        }
        self.entity(headers, body, depth, sink)
    }

    fn entity(
        &mut self,
        headers: &Headers,
        body: &[u8],
        depth: usize,
        sink: &mut Sink,
    ) -> Result<(), ExtractError> {
        self.parts += 1;
        if self.parts > self.limits.max_entries {
            return Err(ExtractError::TooLarge(format!(
                "more than {} MIME parts",
                self.limits.max_entries
            )));
        }
        if depth > self.limits.max_depth || sink.is_full() {
            return Ok(());
        }
        let content_type = ContentType::parse(header_raw(headers, "content-type"));
        let attachment = header_raw(headers, "content-disposition")
            .is_some_and(|d| trim(d).to_ascii_lowercase().starts_with(b"attachment"));
        let mime = content_type.mime.as_str();

        if let Some(sub) = mime.strip_prefix("multipart/") {
            let Some(boundary) = content_type.param("boundary") else {
                return Ok(());
            };
            let parts = split_multipart(body, boundary.as_bytes());
            if sub == "alternative" {
                if let Some(best) = pick_alternative(&parts) {
                    sink.newline();
                    let (h, b) = split_entity(best);
                    self.entity(&h, b, depth + 1, sink)?;
                }
            } else {
                for part in parts {
                    sink.newline();
                    let (h, b) = split_entity(part);
                    self.entity(&h, b, depth + 1, sink)?;
                    if sink.is_full() {
                        break;
                    }
                }
            }
            return Ok(());
        }
        if attachment {
            return Ok(());
        }
        match mime {
            "message/rfc822" | "message/global" => {
                let decoded = transfer_decode(headers, body);
                let (h, b) = split_entity(&decoded);
                sink.newline();
                self.message(&h, b, depth + 1, sink)
            }
            "text/plain" | "" => {
                let text = decode_charset(
                    &transfer_decode(headers, body),
                    content_type.param("charset"),
                );
                sink.lines(&text.replace("\r\n", "\n").replace('\r', "\n"));
                Ok(())
            }
            "text/html" => {
                let text = decode_charset(
                    &transfer_decode(headers, body),
                    content_type.param("charset"),
                );
                html::to_text(&text, sink, &mut Metadata::default());
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// Of the parts of a `multipart/alternative`, the plain-text one, else the
/// HTML one, else the first nested multipart.
fn pick_alternative<'a>(parts: &[&'a [u8]]) -> Option<&'a [u8]> {
    let type_of =
        |p: &[u8]| ContentType::parse(header_raw(&split_entity(p).0, "content-type")).mime;
    let types: Vec<String> = parts.iter().map(|p| type_of(p)).collect();
    let pick = |want: &dyn Fn(&str) -> bool| types.iter().position(|t| want(t)).map(|i| parts[i]);
    pick(&|t| t == "text/plain" || t.is_empty())
        .or_else(|| pick(&|t| t == "text/html"))
        .or_else(|| pick(&|t| t.starts_with("multipart/")))
}

/// Splits an entity into its unfolded header fields and its body.
fn split_entity(bytes: &[u8]) -> (Headers, &[u8]) {
    let mut headers: Headers = Vec::new();
    let mut pos = 0;
    let mut first = true;
    while pos < bytes.len() {
        let end = bytes[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |p| pos + p);
        let mut line = &bytes[pos..end];
        if let Some(l) = line.strip_suffix(b"\r") {
            line = l;
        }
        let next = (end + 1).min(bytes.len());
        if line.is_empty() {
            return (headers, &bytes[next..]);
        }
        // An mbox "From " separator line may precede the first header.
        let mbox_from = first && line.starts_with(b"From ");
        if line[0] == b' ' || line[0] == b'\t' {
            if let Some((_, value)) = headers.last_mut() {
                let room = MAX_HEADER_BYTES.saturating_sub(value.len());
                value.extend_from_slice(&line[..line.len().min(room)]);
            }
        } else if let Some(colon) = line.iter().position(|&b| b == b':') {
            let name = trim(&line[..colon]);
            let valid = !name.is_empty() && name.iter().all(|b| b.is_ascii_graphic());
            if valid && headers.len() < MAX_HEADERS {
                headers.push((
                    String::from_utf8_lossy(name).to_ascii_lowercase(),
                    trim(&line[colon + 1..])
                        .iter()
                        .take(MAX_HEADER_BYTES)
                        .copied()
                        .collect(),
                ));
            } else if !valid && !mbox_from {
                // Not a header: the entity has no header block.
                return (headers, &bytes[pos..]);
            }
        } else if !mbox_from {
            return (headers, &bytes[pos..]);
        }
        first = false;
        pos = next;
    }
    (headers, &[])
}

fn header_raw<'h>(headers: &'h Headers, name: &str) -> Option<&'h [u8]> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_slice())
}

/// A header value decoded: raw 8-bit bytes by UTF-8 (or a detected legacy
/// encoding), then RFC 2047 encoded words; whitespace collapsed.
fn header(headers: &Headers, name: &str) -> Option<String> {
    let raw = header_raw(headers, name)?;
    let text = decode_charset(raw, None);
    let decoded = decode_words(&text);
    Some(decoded.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Decodes RFC 2047 encoded words (`=?charset?B|Q?text?=`). Whitespace
/// between adjacent encoded words is dropped, and adjacent words in the
/// same charset are decoded together so a character split across words
/// survives.
pub(crate) fn decode_words(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending: Option<(&'static Encoding, Vec<u8>)> = None;
    let mut gap = ""; // Whitespace seen after an encoded word.
    let mut rest = s;
    while !rest.is_empty() {
        if let Some((encoding, bytes, len)) = encoded_word(rest) {
            match &mut pending {
                Some((enc, buf)) if *enc == encoding => buf.extend_from_slice(&bytes),
                _ => {
                    flush(&mut pending, &mut out);
                    pending = Some((encoding, bytes));
                }
            }
            gap = "";
            rest = &rest[len..];
            continue;
        }
        let ws = rest.len() - rest.trim_start().len();
        if ws > 0 && pending.is_some() {
            gap = &rest[..ws];
            rest = &rest[ws..];
            if encoded_word(rest).is_none() {
                flush(&mut pending, &mut out);
                out.push_str(gap);
                gap = "";
            }
            continue;
        }
        flush(&mut pending, &mut out);
        out.push_str(gap);
        gap = "";
        let c = rest.chars().next().unwrap_or(' ');
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    flush(&mut pending, &mut out);
    out.push_str(gap);
    out
}

fn flush(pending: &mut Option<(&'static Encoding, Vec<u8>)>, out: &mut String) {
    if let Some((encoding, bytes)) = pending.take() {
        out.push_str(&encoding.decode_without_bom_handling(&bytes).0);
    }
}

/// Parses one encoded word at the start of `s`: its charset, decoded bytes
/// and length in `s`.
fn encoded_word(s: &str) -> Option<(&'static Encoding, Vec<u8>, usize)> {
    let inner = s.strip_prefix("=?")?;
    let q1 = crate::find(&inner.as_bytes()[..inner.len().min(80)], b"?")?;
    let charset = &inner[..q1];
    let charset = charset.split('*').next().unwrap_or_default(); // RFC 2231 language
    let after = &inner[q1 + 1..];
    let ab = after.as_bytes();
    if ab.len() < 2 || ab[1] != b'?' {
        return None;
    }
    let mode = ab[0].to_ascii_uppercase();
    let payload_all = &after[2..];
    // Bounded search keeps a header full of "=?" linear.
    let window = &payload_all.as_bytes()[..payload_all.len().min(MAX_WORD)];
    let end = crate::find(window, b"?=")?;
    let payload = &payload_all[..end];
    if payload.contains(char::is_whitespace) {
        return None;
    }
    let encoding = Encoding::for_label(charset.trim().as_bytes()).unwrap_or(UTF_8);
    let bytes = match mode {
        b'B' => base64(payload.as_bytes()),
        b'Q' => quoted_printable(payload.as_bytes(), true),
        _ => return None,
    };
    let len = 2 + q1 + 1 + 2 + end + 2;
    Some((encoding, bytes, len))
}

/// The body with its `Content-Transfer-Encoding` undone.
fn transfer_decode(headers: &Headers, body: &[u8]) -> Vec<u8> {
    let cte = header_raw(headers, "content-transfer-encoding")
        .map(|v| trim(v).to_ascii_lowercase())
        .unwrap_or_default();
    match cte.as_slice() {
        b"base64" => base64(body),
        b"quoted-printable" => quoted_printable(body, false),
        _ => body.to_vec(),
    }
}

/// Lenient base64: characters outside the alphabet are skipped, decoding
/// stops at padding.
pub(crate) fn base64(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in input {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// Quoted-printable; `header` selects the RFC 2047 "Q" variant where `_`
/// is a space.
pub(crate) fn quoted_printable(input: &[u8], header: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        if b == b'=' {
            // Soft line break: "=" + optional trailing blanks + (CR)LF.
            let mut j = i + 1;
            while j < input.len() && (input[j] == b' ' || input[j] == b'\t') {
                j += 1;
            }
            if input.get(j) == Some(&b'\r') && input.get(j + 1) == Some(&b'\n') {
                i = j + 2;
                continue;
            }
            if input.get(j) == Some(&b'\n') {
                i = j + 1;
                continue;
            }
            if let (Some(h), Some(l)) = (
                input.get(i + 1).copied().and_then(hex),
                input.get(i + 2).copied().and_then(hex),
            ) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
            out.push(b'=');
        } else if header && b == b'_' {
            out.push(b' ');
        } else {
            out.push(b);
        }
        i += 1;
    }
    out
}

/// Decodes body or header bytes in `charset`, or, when it is absent or
/// unknown, as UTF-8 if valid, else in the detected legacy encoding.
fn decode_charset(bytes: &[u8], charset: Option<&str>) -> String {
    let encoding = charset
        .and_then(|c| Encoding::for_label(c.trim().as_bytes()))
        .unwrap_or_else(|| crate::text::detect_encoding(bytes));
    encoding.decode_without_bom_handling(bytes).0.into_owned()
}

/// Splits a multipart body at its boundary delimiter lines; the preamble
/// and epilogue are dropped.
fn split_multipart<'a>(body: &'a [u8], boundary: &[u8]) -> Vec<&'a [u8]> {
    let mut delimiter = b"--".to_vec();
    delimiter.extend_from_slice(boundary);
    let mut parts = Vec::new();
    let mut start: Option<usize> = None;
    let mut pos = 0;
    while pos < body.len() {
        let end = body[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(body.len(), |p| pos + p);
        let line = trim_end(&body[pos..end]);
        if line.starts_with(&delimiter) {
            let tail = &line[delimiter.len()..];
            let closing = tail.starts_with(b"--");
            if closing || trim(tail).is_empty() {
                if let Some(s) = start {
                    // The line break before the delimiter belongs to it.
                    let mut e = pos;
                    if e > s && body[e - 1] == b'\n' {
                        e -= 1;
                        if e > s && body[e - 1] == b'\r' {
                            e -= 1;
                        }
                    }
                    parts.push(&body[s..e]);
                }
                if closing {
                    return parts;
                }
                start = Some((end + 1).min(body.len()));
            }
        }
        pos = end + 1;
    }
    if let Some(s) = start {
        parts.push(&body[s..]);
    }
    parts
}

/// A parsed `Content-Type` value.
struct ContentType {
    /// Lowercase `type/subtype`, empty when absent.
    mime: String,
    params: Vec<(String, String)>,
}

impl ContentType {
    fn parse(value: Option<&[u8]>) -> Self {
        let Some(value) = value else {
            return ContentType {
                mime: String::new(),
                params: Vec::new(),
            };
        };
        let value = String::from_utf8_lossy(value);
        let mut pieces = split_params(&value).into_iter();
        let mime = pieces
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let params = pieces
            .filter_map(|p| {
                let (k, v) = p.split_once('=')?;
                let v = v.trim();
                let v = v
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .unwrap_or(v);
                Some((k.trim().to_ascii_lowercase(), v.replace("\\\"", "\"")))
            })
            .collect();
        ContentType { mime, params }
    }

    fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k == name || k.strip_suffix('*') == Some(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Splits at `;` outside double quotes.
fn split_params(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut in_quote = false;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '"' => in_quote = !in_quote,
            ';' if !in_quote => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

fn trim(b: &[u8]) -> &[u8] {
    trim_end(&b[b.iter().take_while(|c| c.is_ascii_whitespace()).count()..])
}

fn trim_end(b: &[u8]) -> &[u8] {
    let n = b
        .iter()
        .rev()
        .take_while(|c| c.is_ascii_whitespace())
        .count();
    &b[..b.len() - n]
}

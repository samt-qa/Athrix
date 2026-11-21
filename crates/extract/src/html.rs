//! HTML to text: a small, lenient tag stripper.
//!
//! Not a full HTML5 parser: it only needs to find text, drop markup,
//! scripts and styles, decode entities and keep block structure. It runs in
//! linear time on any input.

use encoding_rs::{Encoding, UTF_8};

use crate::Metadata;
use crate::sink::Sink;

/// Elements whose end (and start) breaks the paragraph.
const BLOCK: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "body",
    "caption",
    "center",
    "dd",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "frameset",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hgroup",
    "hr",
    "html",
    "legend",
    "li",
    "main",
    "menu",
    "nav",
    "ol",
    "option",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "tbody",
    "tfoot",
    "thead",
    "tr",
    "ul",
];

/// Elements whose content is not text.
const SKIP_CONTENT: &[&str] = &[
    "script", "style", "noscript", "template", "iframe", "object", "noembed", "noframes", "xmp",
    "math", "svg",
];

/// Decodes HTML bytes: BOM, then a `<meta charset>` / `http-equiv`
/// declaration in the first 4 KiB, then UTF-8 validity, then legacy-encoding
/// detection.
pub(crate) fn decode(bytes: &[u8]) -> (String, &'static Encoding) {
    if let Some((encoding, bom)) = Encoding::for_bom(bytes) {
        let (text, _) = encoding.decode_without_bom_handling(&bytes[bom..]);
        return (text.into_owned(), encoding);
    }
    let encoding = meta_charset(&bytes[..bytes.len().min(4096)])
        .unwrap_or_else(|| crate::text::detect_encoding(bytes));
    let (text, _) = encoding.decode_without_bom_handling(bytes);
    (text.into_owned(), encoding)
}

/// The charset declared by a `<meta>` tag, if any. UTF-16 declarations
/// are read as UTF-8, as browsers do for BOM-less documents.
fn meta_charset(head: &[u8]) -> Option<&'static Encoding> {
    let lower = head.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = crate::find(&lower[from..], b"charset") {
        let at = from + i + b"charset".len();
        from = at;
        let rest = &lower[at..];
        let rest = &rest[rest.iter().take_while(|b| b.is_ascii_whitespace()).count()..];
        let Some(rest) = rest.strip_prefix(b"=") else {
            continue;
        };
        let rest = &rest[rest
            .iter()
            .take_while(|&&b| b.is_ascii_whitespace() || b == b'"' || b == b'\'')
            .count()..];
        let len = rest
            .iter()
            .take_while(|&&b| {
                !matches!(b, b'"' | b'\'' | b';' | b'>' | b'/') && !b.is_ascii_whitespace()
            })
            .count();
        if let Some(encoding) = Encoding::for_label(&rest[..len]) {
            return Some(if encoding.output_encoding() == encoding {
                encoding
            } else {
                UTF_8
            });
        }
    }
    None
}

/// Writes the text of the HTML document `src` into `sink`; fills the title
/// (`<title>`), author (`<meta name="author">`) and creation date
/// (`<meta name="dcterms.created">` / `date`) when present.
pub(crate) fn to_text(src: &str, sink: &mut Sink, meta: &mut Metadata) {
    let bytes = src.as_bytes();
    let mut i = 0;
    let mut in_pre = 0usize;
    while i < bytes.len() && !sink.is_full() {
        let Some(lt) = memchr(b'<', &bytes[i..]).map(|p| i + p) else {
            emit(&src[i..], in_pre > 0, sink);
            break;
        };
        if lt > i {
            emit(&src[i..lt], in_pre > 0, sink);
        }
        let rest = &bytes[lt..];
        if rest.starts_with(b"<!--") {
            i = match find_from(bytes, lt + 4, b"-->") {
                Some(end) => end + 3,
                None => bytes.len(),
            };
            continue;
        }
        let next = rest.get(1).copied().unwrap_or(0);
        if next == b'!' || next == b'?' {
            i = memchr(b'>', &bytes[lt..]).map_or(bytes.len(), |p| lt + p + 1);
            continue;
        }
        let closing = next == b'/';
        let name_start = lt + 1 + usize::from(closing);
        let name_len = bytes[name_start..]
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b':' | b'_'))
            .count();
        if name_len == 0 || !bytes[name_start].is_ascii_alphabetic() {
            // Not a tag: a literal '<'.
            emit("<", in_pre > 0, sink);
            i = lt + 1;
            continue;
        }
        let name = src[name_start..name_start + name_len].to_ascii_lowercase();
        let name = name.rsplit(':').next().unwrap_or_default().to_owned();
        let Some(tag_end) = tag_end(bytes, name_start + name_len) else {
            break; // Unterminated tag at the end of the input.
        };
        let attrs = &src[name_start + name_len..tag_end];
        i = tag_end + 1;

        if closing {
            if name == "pre" || name == "textarea" || name == "listing" {
                in_pre = in_pre.saturating_sub(1);
            }
            if BLOCK.contains(&name.as_str()) {
                sink.newline();
            } else if matches!(name.as_str(), "td" | "th") {
                sink.space();
            }
            continue;
        }
        let self_closing = attrs.trim_end().ends_with('/');
        match name.as_str() {
            "title" => {
                let end = find_ci(bytes, i, b"</title").unwrap_or(bytes.len());
                if meta.title.is_none() {
                    let title = collapse(&decode_entities(&src[i..end]));
                    if !title.is_empty() {
                        meta.title = Some(title);
                    }
                }
                i = skip_close(bytes, end);
            }
            n if SKIP_CONTENT.contains(&n) && !self_closing => {
                let close = format!("</{n}");
                let end = find_ci(bytes, i, close.as_bytes()).unwrap_or(bytes.len());
                i = skip_close(bytes, end);
            }
            "meta" => meta_tag(attrs, meta),
            "br" => sink.newline(),
            "td" | "th" => sink.space(),
            "img" => {
                if let Some(alt) = attr(attrs, "alt") {
                    sink.space();
                    sink.text(&alt);
                    sink.space();
                }
            }
            n => {
                if BLOCK.contains(&n) {
                    sink.newline();
                }
                if matches!(n, "pre" | "textarea" | "listing") && !self_closing {
                    in_pre += 1;
                }
            }
        }
    }
}

/// Index of the `>` ending a tag whose attributes start at `from`, skipping
/// quoted attribute values.
fn tag_end(bytes: &[u8], from: usize) -> Option<usize> {
    let mut quote = 0u8;
    for (k, &b) in bytes[from..].iter().enumerate() {
        if quote != 0 {
            if b == quote {
                quote = 0;
            }
        } else if b == b'"' || b == b'\'' {
            // Quotes only open a value right after `=` (possibly spaced).
            let before = bytes[from..from + k]
                .iter()
                .rev()
                .find(|c| !c.is_ascii_whitespace());
            if before == Some(&b'=') {
                quote = b;
            }
        } else if b == b'>' {
            return Some(from + k);
        }
    }
    None
}

/// Position after the `>` of a closing tag that starts at `at`.
fn skip_close(bytes: &[u8], at: usize) -> usize {
    if at >= bytes.len() {
        return bytes.len();
    }
    memchr(b'>', &bytes[at..]).map_or(bytes.len(), |p| at + p + 1)
}

fn emit(text: &str, pre: bool, sink: &mut Sink) {
    let decoded = decode_entities(text);
    if pre {
        sink.lines(&decoded.replace("\r\n", "\n").replace('\r', "\n"));
    } else {
        sink.text(&decoded);
    }
}

fn meta_tag(attrs: &str, meta: &mut Metadata) {
    let Some(name) = attr(attrs, "name").or_else(|| attr(attrs, "property")) else {
        return;
    };
    let Some(content) = attr(attrs, "content").map(|c| collapse(&c)) else {
        return;
    };
    if content.is_empty() {
        return;
    }
    let slot = match name.to_ascii_lowercase().as_str() {
        "author" | "dc.creator" | "dcterms.creator" | "article:author" => &mut meta.author,
        "dcterms.created" | "dc.date" | "date" | "article:published_time" => &mut meta.created,
        _ => return,
    };
    if slot.is_none() {
        *slot = Some(content);
    }
}

/// The value of attribute `name` in a tag's attribute text, entities
/// decoded.
fn attr(attrs: &str, name: &str) -> Option<String> {
    let bytes = attrs.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        let start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && !matches!(bytes[i], b'=' | b'>' | b'/')
        {
            i += 1;
        }
        let key = &attrs[start..i];
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = "";
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let q = bytes[i];
                let vstart = i + 1;
                let vend = memchr(q, &bytes[vstart..]).map_or(bytes.len(), |p| vstart + p);
                value = &attrs[vstart..vend];
                i = (vend + 1).min(bytes.len());
            } else {
                let vstart = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                value = &attrs[vstart..i];
            }
        }
        if key.eq_ignore_ascii_case(name) {
            return Some(decode_entities(value));
        }
        if i == start {
            i += 1; // A stray character; make progress.
        }
    }
    None
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn memchr(needle: u8, hay: &[u8]) -> Option<usize> {
    hay.iter().position(|&b| b == needle)
}

fn find_from(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    crate::find(hay.get(from..)?, needle).map(|p| from + p)
}

/// ASCII case-insensitive search for `needle` (lowercase) from `from`.
fn find_ci(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    let hay_from = hay.get(from..)?;
    if hay_from.len() < needle.len() {
        return None;
    }
    hay_from
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
        .map(|p| from + p)
}

/// Decodes character references: `&#1575;`, `&#x627;` and named entities.
/// Unknown or malformed references stay as written.
pub(crate) fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let body_len = rest[1..]
            .bytes()
            .take(33)
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'#')
            .count();
        let body = &rest[1..1 + body_len];
        let has_semicolon = rest.as_bytes().get(1 + body_len) == Some(&b';');
        let consumed = 1 + body_len + usize::from(has_semicolon);
        let decoded: Option<String> = if let Some(num) = body.strip_prefix('#') {
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => num.parse::<u32>().ok(),
            };
            code.filter(|_| !num.is_empty()).map(|c| {
                char::from_u32(c)
                    .filter(|&c| c != '\0')
                    .unwrap_or('\u{FFFD}')
                    .to_string()
            })
        } else if has_semicolon || matches!(body, "amp" | "lt" | "gt" | "quot" | "nbsp") {
            named_entity(body).map(str::to_owned)
        } else {
            None
        };
        match decoded {
            Some(text) => {
                out.push_str(&text);
                rest = &rest[consumed..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The common HTML named character references.
pub(crate) fn named_entity(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => "\u{A0}",
        "ensp" => "\u{2002}",
        "emsp" => "\u{2003}",
        "thinsp" => "\u{2009}",
        "zwnj" => "\u{200C}",
        "zwj" => "\u{200D}",
        "lrm" => "\u{200E}",
        "rlm" => "\u{200F}",
        "shy" => "\u{AD}",
        "ndash" => "–",
        "mdash" => "—",
        "hellip" => "…",
        "lsquo" => "‘",
        "rsquo" => "’",
        "sbquo" => "‚",
        "ldquo" => "“",
        "rdquo" => "”",
        "bdquo" => "„",
        "laquo" => "«",
        "raquo" => "»",
        "lsaquo" => "‹",
        "rsaquo" => "›",
        "bull" => "•",
        "middot" => "·",
        "prime" => "′",
        "Prime" => "″",
        "dagger" => "†",
        "Dagger" => "‡",
        "permil" => "‰",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "deg" => "°",
        "plusmn" => "±",
        "times" => "×",
        "divide" => "÷",
        "minus" => "−",
        "frac12" => "½",
        "frac14" => "¼",
        "frac34" => "¾",
        "sup1" => "¹",
        "sup2" => "²",
        "sup3" => "³",
        "micro" => "µ",
        "para" => "¶",
        "sect" => "§",
        "cent" => "¢",
        "pound" => "£",
        "yen" => "¥",
        "euro" => "€",
        "curren" => "¤",
        "iexcl" => "¡",
        "iquest" => "¿",
        "ordf" => "ª",
        "ordm" => "º",
        "not" => "¬",
        "macr" => "¯",
        "acute" => "´",
        "cedil" => "¸",
        "uml" => "¨",
        "larr" => "←",
        "rarr" => "→",
        "uarr" => "↑",
        "darr" => "↓",
        "harr" => "↔",
        "le" => "≤",
        "ge" => "≥",
        "ne" => "≠",
        "asymp" => "≈",
        "infin" => "∞",
        "sum" => "∑",
        "radic" => "√",
        "Agrave" => "À",
        "Aacute" => "Á",
        "Acirc" => "Â",
        "Atilde" => "Ã",
        "Auml" => "Ä",
        "Aring" => "Å",
        "AElig" => "Æ",
        "Ccedil" => "Ç",
        "Egrave" => "È",
        "Eacute" => "É",
        "Ecirc" => "Ê",
        "Euml" => "Ë",
        "Igrave" => "Ì",
        "Iacute" => "Í",
        "Icirc" => "Î",
        "Iuml" => "Ï",
        "Ntilde" => "Ñ",
        "Ograve" => "Ò",
        "Oacute" => "Ó",
        "Ocirc" => "Ô",
        "Otilde" => "Õ",
        "Ouml" => "Ö",
        "Oslash" => "Ø",
        "Ugrave" => "Ù",
        "Uacute" => "Ú",
        "Ucirc" => "Û",
        "Uuml" => "Ü",
        "Yacute" => "Ý",
        "szlig" => "ß",
        "agrave" => "à",
        "aacute" => "á",
        "acirc" => "â",
        "atilde" => "ã",
        "auml" => "ä",
        "aring" => "å",
        "aelig" => "æ",
        "ccedil" => "ç",
        "egrave" => "è",
        "eacute" => "é",
        "ecirc" => "ê",
        "euml" => "ë",
        "igrave" => "ì",
        "iacute" => "í",
        "icirc" => "î",
        "iuml" => "ï",
        "ntilde" => "ñ",
        "ograve" => "ò",
        "oacute" => "ó",
        "ocirc" => "ô",
        "otilde" => "õ",
        "ouml" => "ö",
        "oslash" => "ø",
        "ugrave" => "ù",
        "uacute" => "ú",
        "ucirc" => "û",
        "uuml" => "ü",
        "yacute" => "ý",
        "yuml" => "ÿ",
        "OElig" => "Œ",
        "oelig" => "œ",
        "Scaron" => "Š",
        "scaron" => "š",
        "Yuml" => "Ÿ",
        "fnof" => "ƒ",
        "circ" => "ˆ",
        "tilde" => "˜",
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "pi" => "π",
        "sigma" => "σ",
        "omega" => "ω",
        "Omega" => "Ω",
        _ => return None,
    })
}

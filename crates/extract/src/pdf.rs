//! PDF text via `pdf-extract` (pure Rust, on `lopdf`). Behind the `pdf`
//! feature, off by default.
//!
//! Why it is opt-in:
//!
//! - `pdf-extract` panics on a sizeable share of malformed files (about one
//!   in eight randomly mutated PDFs in our evaluation). The panics are
//!   caught by the crate-wide `catch_unwind` guard and reported as
//!   [`ExtractError::Corrupt`], but they still reach the panic hook (stderr)
//!   and would abort under `panic = "abort"`.
//! - Deeply self-referencing content (forms drawing themselves) can recurse
//!   without bound inside `pdf-extract`, which no guard can catch.
//! - `lopdf` inflates object streams (`/Type /ObjStm`) at load time with no
//!   size limit. This module hooks `lopdf`'s load filter to inflate each
//!   object stream with a cap first (`load_bounded`) and bounds every other
//!   `FlateDecode` stream before text extraction. Residual gap: `lopdf`
//!   ignores the load filter for encrypted documents, whose object streams
//!   are still expanded unbounded while loading and decrypting.
//!
//! The safe way to run this code on untrusted files is out of process, in
//! the `athrix-extract-worker` binary through [`crate::WorkerPool`]: a
//! panic, stack overflow or memory blow-up there kills only the worker.
//!
//! Arabic: PDF producers draw Arabic glyphs in visual order (left to
//! right on the page) and often map them to presentation forms
//! (U+FB50–U+FDFF, U+FE70–U+FEFF). Glyphs are therefore collected with
//! their positions, each line is sorted visually and converted back to
//! logical order glyph by glyph (so a lam-alef ligature keeps its inner
//! order), and presentation forms are mapped to base letters with NFKC.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Read;

use pdf_extract::{
    Document, MediaBox, Object, ObjectId, OutputDev, OutputError, Reader, Stream, Transform,
};
use unicode_normalization::UnicodeNormalization;

use crate::sink::Sink;
use crate::{ExtractError, Limits, Metadata};

pub(crate) fn extract(
    bytes: &[u8],
    limits: &Limits,
    sink: &mut Sink,
    meta: &mut Metadata,
) -> Result<(), ExtractError> {
    if crate::find(&bytes[..bytes.len().min(1024)], b"%PDF-").is_none() {
        return Err(ExtractError::Corrupt("pdf: no %PDF header".into()));
    }
    let mut doc = load_bounded(bytes, limits)?;
    if doc.is_encrypted() && doc.decrypt("").is_err() {
        return Err(ExtractError::Encrypted);
    }
    check_streams(&doc, limits)?;
    info(&doc, meta);

    let mut dev = Collector {
        sink,
        line: Vec::new(),
    };
    let result = pdf_extract::output_doc(&doc, &mut dev);
    dev.flush_line();
    match result {
        Ok(()) => Ok(()),
        Err(_) if dev.sink.is_full() => Ok(()),
        Err(e) => Err(ExtractError::Corrupt(format!("pdf: {e}"))),
    }
}

/// Budget for the object streams of the document being loaded on this
/// thread. `lopdf`'s load filter is a plain `fn`, so it reaches the limits
/// through this thread-local.
struct ObjStmBudget {
    /// Largest inflated size of one object stream.
    cap: u64,
    /// Inflated bytes left for all object streams of the document.
    remaining: u64,
    /// Why an object stream was refused, if one was.
    refused: Option<ExtractError>,
}

thread_local! {
    static OBJSTM_BUDGET: RefCell<Option<ObjStmBudget>> = const { RefCell::new(None) };
}

/// [`Document::load_mem`] with every object stream inflated under a cap
/// before `lopdf` expands it.
fn load_bounded(bytes: &[u8], limits: &Limits) -> Result<Document, ExtractError> {
    OBJSTM_BUDGET.with(|b| {
        *b.borrow_mut() = Some(ObjStmBudget {
            cap: limits.max_entry_bytes,
            remaining: limits.max_total_bytes,
            refused: None,
        });
    });
    let reader = Reader {
        buffer: bytes,
        document: Document::new(),
        encryption_state: None,
        raw_objects: BTreeMap::new(),
    };
    let result = reader.read(Some(objstm_filter));
    let budget = OBJSTM_BUDGET.with(|b| b.borrow_mut().take());
    if let Some(err) = budget.and_then(|b| b.refused) {
        return Err(err);
    }
    result.map_err(|e| ExtractError::Corrupt(format!("pdf: {e}")))
}

/// `lopdf` load filter: drops (and records) an object stream that would
/// inflate past the budget, before `lopdf` decompresses it. Returning
/// `None` drops the object.
///
/// `lopdf` calls the filter on every top-level object, ignoring the
/// returned value, and on every object unpacked from an object stream,
/// keeping the returned value. Streams are always top-level (the PDF
/// specification forbids them inside object streams), so a stream gets a
/// cheap placeholder back instead of a copy.
fn objstm_filter(id: ObjectId, object: &mut Object) -> Option<(ObjectId, Object)> {
    match object {
        Object::Stream(stream) => {
            if stream.dict.has_type(b"ObjStm") && !objstm_allowed(stream) {
                return None;
            }
            Some((id, Object::Null))
        }
        other => Some((id, other.clone())),
    }
}

/// Charges an object stream against the thread's budget.
fn objstm_allowed(stream: &Stream) -> bool {
    OBJSTM_BUDGET.with(|cell| {
        let mut slot = cell.borrow_mut();
        // Without a budget (the filter ran on another thread, e.g. if
        // `lopdf`'s `rayon` feature were enabled), fall back to the default
        // per-stream cap.
        let mut fallback = ObjStmBudget {
            cap: Limits::default().max_entry_bytes,
            remaining: Limits::default().max_total_bytes,
            refused: None,
        };
        let budget = slot.as_mut().unwrap_or(&mut fallback);
        if budget.refused.is_some() {
            return false;
        }
        let cap = budget.cap.min(budget.remaining);
        let size = match filter_names(stream).as_slice() {
            [] => stream.content.len() as u64,
            [name] if name == b"FlateDecode" => inflated_size(&stream.content, cap),
            _ => {
                budget.refused = Some(ExtractError::Unsupported(
                    "pdf object stream with a filter other than FlateDecode".into(),
                ));
                return false;
            }
        };
        if size > cap {
            budget.refused = Some(ExtractError::TooLarge(format!(
                "pdf object stream inflates past {cap} bytes"
            )));
            return false;
        }
        budget.remaining -= size;
        true
    })
}

/// The `/Filter` names of a stream (a name or an array of names).
fn filter_names(stream: &Stream) -> Vec<Vec<u8>> {
    match stream.dict.get(b"Filter") {
        Ok(Object::Name(n)) => vec![n.clone()],
        Ok(Object::Array(a)) => a
            .iter()
            .map(|f| match f {
                Object::Name(n) => n.clone(),
                _ => Vec::new(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Inflated size of zlib `data`, reading at most `cap + 1` bytes (so a
/// result above `cap` means "too large"). Corrupt data counts as what
/// inflated before the error.
fn inflated_size(data: &[u8], cap: u64) -> u64 {
    let mut decoder = flate2::read::ZlibDecoder::new(data).take(cap.saturating_add(1));
    let mut sink = CountingSink(0);
    let _ = std::io::copy(&mut decoder, &mut sink);
    sink.0
}

/// A writer that only counts bytes, so a partial copy still reports its
/// size.
struct CountingSink(u64);

impl std::io::Write for CountingSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Inflates every `FlateDecode` stream with a cap, so that a deflate bomb
/// fails with [`ExtractError::TooLarge`] before `pdf-extract` expands it.
fn check_streams(doc: &Document, limits: &Limits) -> Result<(), ExtractError> {
    let mut budget = limits.max_total_bytes;
    for object in doc.objects.values() {
        let Object::Stream(stream) = object else {
            continue;
        };
        let flate = match stream.dict.get(b"Filter") {
            Ok(Object::Name(n)) => n == b"FlateDecode",
            Ok(Object::Array(a)) => a
                .iter()
                .any(|f| matches!(f, Object::Name(n) if n == b"FlateDecode")),
            _ => false,
        };
        if !flate {
            continue;
        }
        let cap = limits.max_entry_bytes.min(budget);
        let mut decoder =
            flate2::read::ZlibDecoder::new(&stream.content[..]).take(cap.saturating_add(1));
        let size = std::io::copy(&mut decoder, &mut std::io::sink()).unwrap_or(0);
        if size > cap {
            return Err(ExtractError::TooLarge(format!(
                "pdf stream inflates past {cap} bytes"
            )));
        }
        budget -= size;
    }
    Ok(())
}

/// Title, Author and CreationDate from the document information dictionary.
fn info(doc: &Document, meta: &mut Metadata) {
    let Ok(Object::Reference(id)) = doc.trailer.get(b"Info") else {
        return;
    };
    let Ok(Object::Dictionary(dict)) = doc.get_object(*id) else {
        return;
    };
    let text = |key: &[u8]| match dict.get(key) {
        Ok(Object::String(s, _)) => Some(pdf_string(s)).filter(|s| !s.is_empty()),
        _ => None,
    };
    meta.title = text(b"Title");
    meta.author = text(b"Author");
    meta.created = text(b"CreationDate");
}

/// Decodes a PDF text string: UTF-16BE with BOM, UTF-8 with BOM, else
/// PDFDocEncoding (approximated by Latin-1).
fn pdf_string(s: &[u8]) -> String {
    if let Some(be) = s.strip_prefix(b"\xFE\xFF") {
        let units: Vec<u16> = be
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units).trim().to_owned();
    }
    if let Some(utf8) = s.strip_prefix(b"\xEF\xBB\xBF") {
        return String::from_utf8_lossy(utf8).trim().to_owned();
    }
    s.iter()
        .map(|&b| char::from(b))
        .collect::<String>()
        .trim()
        .to_owned()
}

/// A glyph as drawn: position, advance end, size and Unicode text.
struct Glyph {
    x: f64,
    y: f64,
    end: f64,
    size: f64,
    text: String,
}

/// Collects glyphs line by line and writes each line in logical order.
struct Collector<'s> {
    sink: &'s mut Sink,
    line: Vec<Glyph>,
}

impl Collector<'_> {
    fn flush_line(&mut self) {
        if self.line.is_empty() {
            return;
        }
        let mut glyphs = std::mem::take(&mut self.line);
        glyphs.sort_by(|a, b| a.x.total_cmp(&b.x));
        // Visual tokens with inferred word gaps.
        let mut tokens: Vec<String> = Vec::with_capacity(glyphs.len() * 2);
        let mut prev_end: Option<(f64, f64)> = None;
        for g in glyphs {
            if let Some((end, size)) = prev_end {
                if g.x - end > 0.15 * size.max(g.size) {
                    tokens.push(" ".to_owned());
                }
            }
            prev_end = Some((g.end, g.size));
            tokens.push(g.text);
        }
        let logical = visual_to_logical(tokens);
        let text: String = logical.concat();
        self.sink.text(&normalize_forms(&text));
        self.sink.newline();
    }
}

impl OutputDev for Collector<'_> {
    fn begin_page(
        &mut self,
        _: u32,
        _: &MediaBox,
        _: Option<(f64, f64, f64, f64)>,
    ) -> Result<(), OutputError> {
        Ok(())
    }

    fn end_page(&mut self) -> Result<(), OutputError> {
        self.flush_line();
        self.sink.newline();
        Ok(())
    }

    fn output_character(
        &mut self,
        trm: &Transform,
        width: f64,
        _spacing: f64,
        font_size: f64,
        text: &str,
    ) -> Result<(), OutputError> {
        if self.sink.is_full() {
            return Err(OutputError::FormatError(std::fmt::Error));
        }
        let scale = (trm.m11 * trm.m22 - trm.m12 * trm.m21).abs().sqrt();
        let size = (font_size * scale).abs().max(0.01);
        let (x, y) = (trm.m31, trm.m32);
        if !(x.is_finite() && y.is_finite() && size.is_finite()) {
            return Ok(());
        }
        if let Some(first) = self.line.first() {
            if (y - first.y).abs() > 0.5 * size.max(first.size) {
                self.flush_line();
            }
        }
        if self.line.len() >= 100_000 {
            self.flush_line();
        }
        let end = x + (width * size).abs();
        self.line.push(Glyph {
            x,
            y,
            end,
            size,
            text: text.to_owned(),
        });
        Ok(())
    }

    fn begin_word(&mut self) -> Result<(), OutputError> {
        Ok(())
    }

    fn end_word(&mut self) -> Result<(), OutputError> {
        Ok(())
    }

    fn end_line(&mut self) -> Result<(), OutputError> {
        Ok(())
    }
}

/// Bidi class of a token, reduced to what visual-to-logical needs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    /// Right-to-left letter (Arabic, Hebrew, Syriac, Thaana...).
    Rtl,
    /// Left-to-right letter.
    Ltr,
    /// Digits (European or Arabic-Indic): always laid out left to right.
    Digit,
    /// White space.
    Space,
    /// Punctuation and symbols.
    Neutral,
}

fn is_rtl(c: char) -> bool {
    matches!(c, '\u{0590}'..='\u{08FF}' | '\u{FB1D}'..='\u{FDFF}' | '\u{FE70}'..='\u{FEFE}')
        && !c.is_numeric()
}

fn class(token: &str) -> Class {
    if token.chars().any(is_rtl) {
        Class::Rtl
    } else if token.chars().any(char::is_numeric) {
        Class::Digit
    } else if token.chars().any(char::is_alphabetic) {
        Class::Ltr
    } else if token.chars().all(char::is_whitespace) {
        Class::Space
    } else {
        Class::Neutral
    }
}

/// Reorders one line of glyph tokens from visual (left to right on the
/// page) to logical order: a simplified inverse of the Unicode
/// bidirectional algorithm that works on whole glyphs.
///
/// A line with more right-to-left than left-to-right letters is an RTL
/// paragraph: it is reversed, then its embedded left-to-right runs (Latin
/// words with the numbers attached to them, and standalone numbers such as
/// `1,250.5`) are put back in reading order. In an LTR line, each
/// right-to-left run is reversed, keeping the numbers inside it left to
/// right.
fn visual_to_logical(mut tokens: Vec<String>) -> Vec<String> {
    let mut classes: Vec<Class> = tokens.iter().map(|t| class(t)).collect();
    let rtl = classes.iter().filter(|&&c| c == Class::Rtl).count();
    let ltr = classes.iter().filter(|&&c| c == Class::Ltr).count();
    if rtl == 0 {
        return tokens;
    }
    let number_inner = |c: Class| c == Class::Neutral;
    if rtl > ltr {
        tokens.reverse();
        classes.reverse();
        let mut covered = vec![false; tokens.len()];
        let latin = runs(
            &classes,
            0,
            tokens.len(),
            |c| c == Class::Ltr,
            |c| c != Class::Rtl,
        );
        for (mut s, mut e) in latin {
            while s > 0 && classes[s - 1] == Class::Digit {
                s -= 1;
            }
            while e + 1 < classes.len() && classes[e + 1] == Class::Digit {
                e += 1;
            }
            tokens[s..=e].reverse();
            covered[s..=e].iter_mut().for_each(|c| *c = true);
        }
        let mut i = 0;
        while i < tokens.len() {
            if covered[i] {
                i += 1;
                continue;
            }
            let end = (i..tokens.len())
                .find(|&k| covered[k])
                .unwrap_or(tokens.len());
            for (s, e) in runs(&classes, i, end, |c| c == Class::Digit, number_inner) {
                tokens[s..=e].reverse();
            }
            i = end;
        }
    } else {
        let arabic = runs(
            &classes,
            0,
            tokens.len(),
            |c| c == Class::Rtl,
            |c| c != Class::Ltr,
        );
        for (s, e) in arabic {
            tokens[s..=e].reverse();
            classes[s..=e].reverse();
            for (ds, de) in runs(&classes, s, e + 1, |c| c == Class::Digit, number_inner) {
                tokens[ds..=de].reverse();
            }
        }
    }
    tokens
}

/// Maximal runs `(first, last)` within `from..to` that start and end with
/// an `edge` token and contain only `edge` or `inner` tokens.
fn runs(
    classes: &[Class],
    from: usize,
    to: usize,
    edge: impl Fn(Class) -> bool,
    inner: impl Fn(Class) -> bool,
) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = from;
    while i < to {
        if !edge(classes[i]) {
            i += 1;
            continue;
        }
        let mut last = i;
        let mut j = i + 1;
        while j < to && (edge(classes[j]) || inner(classes[j])) {
            if edge(classes[j]) {
                last = j;
            }
            j += 1;
        }
        out.push((i, last));
        i = last + 1;
    }
    out
}

/// Maps Arabic presentation forms and Latin ligatures to their
/// compatibility decomposition (NFKC), leaving all other text unchanged.
fn normalize_forms(s: &str) -> String {
    let needs = |c: char| matches!(c, '\u{FB00}'..='\u{FB06}' | '\u{FB50}'..='\u{FDFF}' | '\u{FE70}'..='\u{FEFE}');
    if !s.chars().any(needs) {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if needs(c) {
            out.extend(std::iter::once(c).nfkc());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<String> {
        s.chars().map(String::from).collect()
    }

    #[test]
    fn rtl_line_with_numbers_and_latin() {
        // As drawn (left to right) for the logical line
        // "عام 2024 PDF جديد، 1,250.5 A4".
        let visual = chars("A4 1,250.5 ،ديدج PDF 2024 ماع");
        assert_eq!(
            visual_to_logical(visual).concat(),
            "عام 2024 PDF جديد، 1,250.5 A4"
        );
        // Two numbers in a row are two runs in an RTL paragraph.
        let visual = chars("2025 2024 ماع");
        assert_eq!(visual_to_logical(visual).concat(), "عام 2024 2025");
    }

    #[test]
    fn ltr_line_with_arabic_run_and_numbers() {
        let visual = chars("Item 5 مكيلع 2024 مالس now in stock 42");
        assert_eq!(
            visual_to_logical(visual).concat(),
            "Item 5 سلام 2024 عليكم now in stock 42"
        );
    }

    #[test]
    fn presentation_forms_after_reordering() {
        // سلام as seen-initial, lam-alef final ligature, meem isolated,
        // drawn in visual order.
        let visual = chars("\u{FEE1}\u{FEFC}\u{FEB3}");
        assert_eq!(normalize_forms(&visual_to_logical(visual).concat()), "سلام");
    }
}

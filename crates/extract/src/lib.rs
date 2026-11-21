//! Safe, bounded plain-text extraction from documents.
//!
//! [`extract`] reads a file and returns its text as one [`String`], ready for
//! content indexing. [`extract_bytes`] does the same for bytes already in
//! memory. The format comes from the file extension, checked against the
//! file's magic bytes (see [`Kind`] and [`sniff`]).
//!
//! Supported formats:
//!
//! - plain text, Markdown and source code, with encoding detection: UTF-8
//!   (with or without BOM), UTF-16 LE/BE, and legacy code pages such as the
//!   Arabic Windows-1256 and ISO-8859-6;
//! - HTML / XHTML;
//! - DOCX, XLSX, PPTX (Office Open XML);
//! - ODT, ODS, ODP (OpenDocument);
//! - EPUB 2/3, in reading (spine) order;
//! - EML (RFC 5322 / MIME mail);
//! - PDF, only with the `pdf` cargo feature (pure Rust, off by default),
//!   and safely only out of process: see below.
//!
//! # Safety model
//!
//! Every input is untrusted. All work is bounded by [`Limits`]: input size,
//! output size, the number of archive entries or MIME parts, the
//! decompressed size of each entry and of the whole archive (zip-bomb
//! protection), and MIME nesting depth. Malformed input yields an
//! [`ExtractError`], never a panic. As a second line of defence, each parser
//! runs under [`std::panic::catch_unwind`], and a panic that escapes a parser
//! (a bug) is reported as [`ExtractError::Corrupt`].
//!
//! That guard cannot catch a stack overflow, memory exhaustion or a parser
//! that never returns. [`WorkerPool`] runs extraction in the
//! `athrix-extract-worker` process instead, with a per-file timeout, a
//! heap cap and automatic restarts; [`WorkerPool::extract_isolated`] sends
//! PDFs there and keeps the other formats in-process. See [`worker`].
//!
//! # Output
//!
//! Text is kept in logical (stored) order, so right-to-left text such as
//! Arabic comes out exactly as typed. Structured formats collapse runs of
//! whitespace to one space and end each paragraph, table row, slide or
//! heading with a single `\n`. Plain text keeps its own line structure
//! (line endings normalised to `\n`, control characters dropped).

mod container;
mod eml;
mod epub;
mod error;
mod html;
mod odf;
mod ooxml;
#[cfg(feature = "pdf")]
mod pdf;
mod sink;
mod text;
pub mod worker;
mod xml;

#[cfg(test)]
mod tests;

use std::fs::File;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

pub use error::ExtractError;
pub use text::{TEXT_EXTENSIONS, TEXT_FILE_NAMES};
pub use worker::{WorkerConfig, WorkerPool, WorkerStats};

use sink::Sink;

/// A document format this crate can extract text from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Kind {
    /// Plain text, Markdown, source code, config files, logs, CSV...
    Text,
    /// HTML or XHTML.
    Html,
    /// Word document (Office Open XML).
    Docx,
    /// Excel workbook (Office Open XML).
    Xlsx,
    /// PowerPoint presentation (Office Open XML).
    Pptx,
    /// OpenDocument text.
    Odt,
    /// OpenDocument spreadsheet.
    Ods,
    /// OpenDocument presentation.
    Odp,
    /// EPUB e-book.
    Epub,
    /// E-mail message (RFC 5322 / MIME).
    Eml,
    /// PDF. Extracted only with the `pdf` cargo feature.
    Pdf,
}

impl Kind {
    /// Short lowercase name of the format, such as `"docx"`.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Html => "html",
            Kind::Docx => "docx",
            Kind::Xlsx => "xlsx",
            Kind::Pptx => "pptx",
            Kind::Odt => "odt",
            Kind::Ods => "ods",
            Kind::Odp => "odp",
            Kind::Epub => "epub",
            Kind::Eml => "eml",
            Kind::Pdf => "pdf",
        }
    }

    /// The format for a file extension (without the dot, any case).
    pub fn from_extension(ext: &str) -> Option<Kind> {
        let ext = ext.to_ascii_lowercase();
        let kind = match ext.as_str() {
            "html" | "htm" | "xhtml" | "xht" | "shtml" => Kind::Html,
            "docx" | "docm" | "dotx" | "dotm" => Kind::Docx,
            "xlsx" | "xlsm" | "xltx" | "xltm" => Kind::Xlsx,
            "pptx" | "pptm" | "potx" | "potm" | "ppsx" | "ppsm" => Kind::Pptx,
            "odt" | "ott" => Kind::Odt,
            "ods" | "ots" => Kind::Ods,
            "odp" | "otp" => Kind::Odp,
            "epub" => Kind::Epub,
            "eml" | "mht" | "mhtml" => Kind::Eml,
            "pdf" => Kind::Pdf,
            e if TEXT_EXTENSIONS.contains(&e) => Kind::Text,
            _ => return None,
        };
        Some(kind)
    }

    /// The format for a path, from its extension or, for extension-less
    /// files such as `Makefile` or `README`, from its well-known name.
    pub fn from_path(path: &Path) -> Option<Kind> {
        if let Some(kind) = path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(Kind::from_extension)
        {
            return Some(kind);
        }
        let name = path.file_name()?.to_str()?.to_ascii_lowercase();
        TEXT_FILE_NAMES
            .contains(&name.as_str())
            .then_some(Kind::Text)
    }

    /// Whether extraction for this format is compiled in.
    pub fn is_supported(self) -> bool {
        self != Kind::Pdf || cfg!(feature = "pdf")
    }

    fn is_zip(self) -> bool {
        matches!(
            self,
            Kind::Docx | Kind::Xlsx | Kind::Pptx | Kind::Odt | Kind::Ods | Kind::Odp | Kind::Epub
        )
    }
}

/// Bounds on the work done for one document.
///
/// The defaults suit a background indexer on a desktop: generous enough for
/// real documents, small enough that a hostile file cannot exhaust memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Largest input accepted, in bytes. Larger plain-text files are read up
    /// to this size and reported as truncated; larger documents of other
    /// formats fail with [`ExtractError::TooLarge`].
    pub max_input_bytes: u64,
    /// Most characters (Unicode scalar values) of text returned. Extraction
    /// stops there and sets [`Extracted::truncated`].
    pub max_output_chars: usize,
    /// Most entries in a zip container, and most MIME parts in an e-mail.
    pub max_entries: usize,
    /// Largest decompressed size of one zip entry, in bytes.
    pub max_entry_bytes: u64,
    /// Largest decompressed size of all zip entries read for one document,
    /// in bytes.
    pub max_total_bytes: u64,
    /// Deepest nesting of MIME multiparts and attached messages.
    pub max_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_input_bytes: 64 * 1024 * 1024,
            max_output_chars: 4 * 1024 * 1024,
            max_entries: 10_000,
            max_entry_bytes: 128 * 1024 * 1024,
            max_total_bytes: 256 * 1024 * 1024,
            max_depth: 16,
        }
    }
}

/// Document properties, filled when the format stores them cheaply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Metadata {
    /// Document title (HTML `<title>`, Office/ODF/EPUB title, mail subject).
    pub title: Option<String>,
    /// Author or creator (mail `From`).
    pub author: Option<String>,
    /// Creation date as stored, usually ISO 8601 (mail `Date` as written).
    pub created: Option<String>,
}

/// Text extracted from a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extracted {
    /// The plain text, in logical order, paragraphs separated by `\n`.
    pub text: String,
    /// The format it was extracted as.
    pub kind: Kind,
    /// Whether a limit cut the text short.
    pub truncated: bool,
    /// Title, author and creation date, when available.
    pub metadata: Metadata,
    /// Character encoding detected for plain text and HTML (WHATWG name,
    /// such as `"UTF-8"` or `"windows-1256"`); `None` for other formats.
    pub encoding: Option<&'static str>,
}

/// Size of the file prefix read to sniff the format of a file whose
/// extension is unknown.
const SNIFF_BYTES: u64 = 8 * 1024;

/// Reads `path` and extracts its text.
///
/// The format comes from [`Kind::from_path`]; files with an unknown
/// extension are sniffed by their magic bytes ([`sniff`]) and rejected with
/// [`ExtractError::Unsupported`] when that fails.
pub fn extract(path: impl AsRef<Path>, limits: &Limits) -> Result<Extracted, ExtractError> {
    let path = path.as_ref();
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();

    let kind = match Kind::from_path(path) {
        Some(kind) => kind,
        None => {
            let head = read_prefix(&mut file, SNIFF_BYTES.min(limits.max_input_bytes))?;
            let kind = sniff(&head)
                .ok_or_else(|| ExtractError::Unsupported("unknown file type".into()))?;
            // Rewind by reopening: simpler than Seek on every platform.
            file = File::open(path)?;
            kind
        }
    };
    check_supported(kind)?;

    if kind == Kind::Text {
        let cap = text::input_cap(limits);
        let bytes = read_prefix(&mut file, cap)?;
        let input_truncated = len > cap || (bytes.len() as u64 >= cap && more_data(&mut file)?);
        return guarded(|| text::extract(&bytes, input_truncated, limits));
    }

    if len > limits.max_input_bytes {
        return Err(ExtractError::TooLarge(format!(
            "{len} bytes (limit {})",
            limits.max_input_bytes
        )));
    }
    let bytes = read_prefix(&mut file, limits.max_input_bytes)?;
    if bytes.len() as u64 >= limits.max_input_bytes && more_data(&mut file)? {
        return Err(ExtractError::TooLarge("file grew while reading".into()));
    }
    extract_bytes(kind, &bytes, limits)
}

/// Extracts the text of `bytes`, which hold a document of format `kind`.
///
/// Plain text longer than [`Limits::max_input_bytes`] is cut there and
/// reported as truncated; any other format that large fails with
/// [`ExtractError::TooLarge`].
pub fn extract_bytes(kind: Kind, bytes: &[u8], limits: &Limits) -> Result<Extracted, ExtractError> {
    check_supported(kind)?;
    if kind == Kind::Text {
        let cap = usize::try_from(text::input_cap(limits)).unwrap_or(usize::MAX);
        let input_truncated = bytes.len() > cap;
        let bytes = &bytes[..bytes.len().min(cap)];
        return guarded(|| text::extract(bytes, input_truncated, limits));
    }
    if bytes.len() as u64 > limits.max_input_bytes {
        return Err(ExtractError::TooLarge(format!(
            "{} bytes (limit {})",
            bytes.len(),
            limits.max_input_bytes
        )));
    }
    guarded(|| extract_structured(kind, bytes, limits))
}

/// Guesses the format of a document from its first bytes.
///
/// Recognises PDF, HTML, the zip-based formats that declare themselves with
/// a leading `mimetype` entry (ODF, EPUB), and zip files whose first entry
/// names an Office Open XML part. Plain text is not sniffed: a file is
/// treated as text only by extension or well-known name.
pub fn sniff(head: &[u8]) -> Option<Kind> {
    let trimmed = trim_ascii_start(strip_utf8_bom(head));
    if find(&head[..head.len().min(1024)], b"%PDF-").is_some() {
        return Some(Kind::Pdf);
    }
    if starts_with_ci(trimmed, b"<!doctype html") || starts_with_ci(trimmed, b"<html") {
        return Some(Kind::Html);
    }
    if head.starts_with(b"PK\x03\x04") {
        return container::sniff_zip_head(head);
    }
    None
}

fn check_supported(kind: Kind) -> Result<(), ExtractError> {
    if kind.is_supported() {
        Ok(())
    } else {
        Err(ExtractError::Unsupported(format!(
            "{} extraction is not compiled in",
            kind.name()
        )))
    }
}

fn extract_structured(
    kind: Kind,
    bytes: &[u8],
    limits: &Limits,
) -> Result<Extracted, ExtractError> {
    if kind.is_zip() {
        container::check_zip_magic(kind, bytes)?;
    }
    let mut sink = Sink::new(limits.max_output_chars);
    let mut metadata = Metadata::default();
    let mut encoding = None;
    match kind {
        Kind::Text => return text::extract(bytes, false, limits),
        Kind::Html => {
            let (src, enc) = html::decode(bytes);
            encoding = Some(enc.name());
            html::to_text(&src, &mut sink, &mut metadata);
        }
        Kind::Docx => ooxml::docx(bytes, limits, &mut sink, &mut metadata)?,
        Kind::Xlsx => ooxml::xlsx(bytes, limits, &mut sink, &mut metadata)?,
        Kind::Pptx => ooxml::pptx(bytes, limits, &mut sink, &mut metadata)?,
        Kind::Odt | Kind::Ods | Kind::Odp => {
            odf::extract(kind, bytes, limits, &mut sink, &mut metadata)?;
        }
        Kind::Epub => epub::extract(bytes, limits, &mut sink, &mut metadata)?,
        Kind::Eml => eml::extract(bytes, limits, &mut sink, &mut metadata)?,
        #[cfg(feature = "pdf")]
        Kind::Pdf => pdf::extract(bytes, limits, &mut sink, &mut metadata)?,
        #[cfg(not(feature = "pdf"))]
        Kind::Pdf => return Err(ExtractError::Unsupported("pdf".into())),
    }
    let (text, truncated) = sink.finish();
    Ok(Extracted {
        text,
        kind,
        truncated,
        metadata,
        encoding,
    })
}

/// Runs a parser, turning an escaped panic (a bug) into an error.
fn guarded(f: impl FnOnce() -> Result<Extracted, ExtractError>) -> Result<Extracted, ExtractError> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| {
        Err(ExtractError::Corrupt(
            "parser failure on malformed input".into(),
        ))
    })
}

fn read_prefix(file: &mut File, cap: u64) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.by_ref().take(cap).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn more_data(file: &mut File) -> std::io::Result<bool> {
    let mut one = [0u8; 1];
    Ok(file.read(&mut one)? > 0)
}

fn strip_utf8_bom(b: &[u8]) -> &[u8] {
    b.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(b)
}

fn trim_ascii_start(b: &[u8]) -> &[u8] {
    let n = b.iter().take_while(|c| c.is_ascii_whitespace()).count();
    &b[n..]
}

fn starts_with_ci(hay: &[u8], needle: &[u8]) -> bool {
    hay.len() >= needle.len() && hay[..needle.len()].eq_ignore_ascii_case(needle)
}

pub(crate) fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

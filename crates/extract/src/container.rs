//! Zip containers with zip-bomb protection, and package-relationship helpers.

use std::borrow::Cow;
use std::io::{Cursor, Read};

use zip::ZipArchive;
use zip::result::ZipError;

use crate::xml::{self, Flow, XmlEvent};
use crate::{ExtractError, Kind, Limits};

/// Magic bytes of an OLE compound file. Password-protected OOXML documents
/// are stored in one (an `EncryptionInfo` stream plus the encrypted package).
const CFB_MAGIC: &[u8] = b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1";

/// Checks that `bytes` look like the zip container `kind` expects.
pub(crate) fn check_zip_magic(kind: Kind, bytes: &[u8]) -> Result<(), ExtractError> {
    if bytes.starts_with(b"PK") {
        return Ok(());
    }
    if bytes.starts_with(CFB_MAGIC) {
        let ooxml = matches!(kind, Kind::Docx | Kind::Xlsx | Kind::Pptx);
        // "EncryptionInfo" as UTF-16LE, the stream name in the CFB directory.
        let marker: Vec<u8> = "EncryptionInfo"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        if ooxml && crate::find(bytes, &marker).is_some() {
            return Err(ExtractError::Encrypted);
        }
        return Err(ExtractError::Unsupported(
            "legacy binary Office document (OLE compound file)".into(),
        ));
    }
    Err(ExtractError::Corrupt(format!(
        "{} is not a zip container",
        kind.name()
    )))
}

/// Guesses a zip-based format from the first local file header: ODF and
/// EPUB store an uncompressed `mimetype` entry first; OOXML usually starts
/// with `[Content_Types].xml` and the main part follows somewhere.
pub(crate) fn sniff_zip_head(head: &[u8]) -> Option<Kind> {
    let name_len = usize::from(u16::from_le_bytes([*head.get(26)?, *head.get(27)?]));
    let extra_len = usize::from(u16::from_le_bytes([*head.get(28)?, *head.get(29)?]));
    let name = head.get(30..30 + name_len)?;
    if name == b"mimetype" {
        let data = head.get(30 + name_len + extra_len..)?;
        let data = &data[..data.len().min(80)];
        let has = |m: &[u8]| crate::find(data, m).is_some();
        return if has(b"application/epub+zip") {
            Some(Kind::Epub)
        } else if has(b"opendocument.text") {
            Some(Kind::Odt)
        } else if has(b"opendocument.spreadsheet") {
            Some(Kind::Ods)
        } else if has(b"opendocument.presentation") {
            Some(Kind::Odp)
        } else {
            None
        };
    }
    let has = |m: &[u8]| crate::find(head, m).is_some();
    if has(b"word/") {
        Some(Kind::Docx)
    } else if has(b"xl/") {
        Some(Kind::Xlsx)
    } else if has(b"ppt/") {
        Some(Kind::Pptx)
    } else {
        None
    }
}

/// A zip archive read under [`Limits`].
pub(crate) struct Zip<'a> {
    archive: ZipArchive<Cursor<&'a [u8]>>,
    /// Decompressed bytes still allowed for this document.
    budget: u64,
    max_entry: u64,
}

impl<'a> Zip<'a> {
    /// Opens the archive, rejecting more than `max_entries` entries.
    pub(crate) fn open(bytes: &'a [u8], limits: &Limits) -> Result<Self, ExtractError> {
        let archive = ZipArchive::new(Cursor::new(bytes)).map_err(map_zip_error)?;
        if archive.len() > limits.max_entries {
            return Err(ExtractError::TooLarge(format!(
                "{} zip entries (limit {})",
                archive.len(),
                limits.max_entries
            )));
        }
        Ok(Zip {
            archive,
            budget: limits.max_total_bytes,
            max_entry: limits.max_entry_bytes,
        })
    }

    /// Index of the entry named `name` (a leading `/` is ignored; falls
    /// back to an ASCII case-insensitive match, as OOXML part names are
    /// case-insensitive).
    fn index_of(&self, name: &str) -> Option<usize> {
        let name = name.trim_start_matches('/');
        if let Some(i) = self.archive.index_for_name(name) {
            return Some(i);
        }
        self.archive
            .file_names()
            .find(|n| n.eq_ignore_ascii_case(name))
            .and_then(|n| self.archive.index_for_name(n))
    }

    /// Names of all entries.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.archive.file_names()
    }

    /// Whether an entry named `name` exists.
    pub(crate) fn contains(&self, name: &str) -> bool {
        self.index_of(name).is_some()
    }

    /// Reads the entry `name`, or `None` when absent. The decompressed size
    /// is bounded by the per-entry and per-document limits whatever the
    /// headers claim.
    pub(crate) fn read(&mut self, name: &str) -> Result<Option<Vec<u8>>, ExtractError> {
        let Some(index) = self.index_of(name) else {
            return Ok(None);
        };
        let file = self.archive.by_index(index).map_err(map_zip_error)?;
        if file.encrypted() {
            return Err(ExtractError::Encrypted);
        }
        let cap = self.max_entry.min(self.budget);
        if file.size() > cap {
            return Err(too_large(name, file.size(), cap));
        }
        let mut out = Vec::with_capacity(usize::try_from(file.size()).unwrap_or(0).min(1 << 20));
        file.take(cap.saturating_add(1))
            .read_to_end(&mut out)
            .map_err(|e| ExtractError::Corrupt(format!("{name}: {e}")))?;
        let read = out.len() as u64;
        if read > cap {
            return Err(too_large(name, read, cap));
        }
        self.budget -= read;
        Ok(Some(out))
    }

    /// Reads the XML entry `name` decoded to UTF-8.
    pub(crate) fn read_xml(&mut self, name: &str) -> Result<Option<String>, ExtractError> {
        Ok(self
            .read(name)?
            .map(|bytes| xml::decode(&bytes).into_owned()))
    }
}

fn too_large(name: &str, size: u64, cap: u64) -> ExtractError {
    ExtractError::TooLarge(format!(
        "zip entry {name}: {size}+ bytes decompressed (limit {cap})"
    ))
}

fn map_zip_error(e: ZipError) -> ExtractError {
    match e {
        ZipError::InvalidPassword => ExtractError::Encrypted,
        ZipError::UnsupportedArchive(m) if m.to_ascii_lowercase().contains("password") => {
            ExtractError::Encrypted
        }
        ZipError::UnsupportedArchive(m) => ExtractError::Unsupported(format!("zip: {m}")),
        ZipError::Io(e) => ExtractError::Corrupt(format!("zip: {e}")),
        other => ExtractError::Corrupt(format!("zip: {other}")),
    }
}

/// One package relationship (`_rels/*.rels`).
#[derive(Debug, Clone)]
pub(crate) struct Rel {
    pub(crate) id: String,
    pub(crate) kind: String,
    /// Resolved absolute part name (no leading `/`).
    pub(crate) target: String,
    pub(crate) external: bool,
}

/// Reads the relationships of `part` (`""` for the package root).
pub(crate) fn rels(zip: &mut Zip<'_>, part: &str) -> Result<Vec<Rel>, ExtractError> {
    let (dir, file) = split_dir(part);
    let rels_name = if part.is_empty() {
        "_rels/.rels".to_owned()
    } else {
        format!("{dir}_rels/{file}.rels")
    };
    let Some(src) = zip.read_xml(&rels_name)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    xml::walk(&src, &rels_name, |ev| {
        if let XmlEvent::Start(t) | XmlEvent::Empty(t) = ev {
            if t.name() == b"Relationship" {
                let target = t.attr(b"Target").unwrap_or_default();
                let external = t
                    .attr(b"TargetMode")
                    .is_some_and(|m| m.eq_ignore_ascii_case("external"));
                out.push(Rel {
                    id: t.attr(b"Id").unwrap_or_default(),
                    kind: t.attr(b"Type").unwrap_or_default(),
                    target: if external {
                        target
                    } else {
                        resolve(dir, &target)
                    },
                    external,
                });
            }
        }
        Flow::Continue
    })?;
    Ok(out)
}

/// The target of the first internal relationship whose type ends with
/// `suffix` (such as `/officeDocument`).
pub(crate) fn rel_target(rels: &[Rel], suffix: &str) -> Option<String> {
    rels.iter()
        .find(|r| !r.external && r.kind.ends_with(suffix))
        .map(|r| r.target.clone())
}

/// Splits a part name into its directory (with trailing `/`, or empty) and
/// file name.
pub(crate) fn split_dir(part: &str) -> (&str, &str) {
    match part.rfind('/') {
        Some(i) => (&part[..=i], &part[i + 1..]),
        None => ("", part),
    }
}

/// Resolves a relative reference against a directory inside the archive:
/// percent-decodes, drops any `#fragment`, handles `/`, `.` and `..`.
pub(crate) fn resolve(base_dir: &str, target: &str) -> String {
    let target = target.split('#').next().unwrap_or_default();
    let target = percent_decode(target);
    let joined: Cow<'_, str> = match target.strip_prefix('/') {
        Some(absolute) => Cow::Borrowed(absolute),
        None => Cow::Owned(format!("{base_dir}{target}")),
    };
    let mut parts: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// Decodes `%XX` escapes; invalid escapes stay as written.
pub(crate) fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_owned();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

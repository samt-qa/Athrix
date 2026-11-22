//! Previews of result entries for the preview pane (`preview_entry`), and the
//! `athrix-preview` URI scheme that serves previewed images.
//!
//! - Text and code: the first `maxChars` characters through [`athrix_extract`] (encoding
//!   detection, Arabic code pages), with a language hint from the extension.
//! - Documents (DOCX, XLSX, PPTX, ODF, EPUB, HTML, EML): their extracted text and title/author.
//! - PDF: a `document` with its text and title/author, extracted by the isolated worker process
//!   through the shared [`WorkerPool`] of [`crate::pdf_worker`] (never in the app process). A
//!   missing or failing-to-start worker gives [`UnsupportedReason::Pdf`]; a timeout or a worker
//!   crash gives [`UnsupportedReason::Unreadable`]. PDFs are found by extension or, without
//!   one, by their `%PDF` magic.
//! - Images: never sent as bytes over IPC. The preview registers the file under a random,
//!   short-lived token in [`PreviewAssets`] and returns a URL of the [`SCHEME`] scheme; the
//!   scheme handler serves only registered tokens (only the last [`ASSET_SLOTS`] images
//!   previewed), only to the main window, with a fixed image MIME type, `nosniff` and a
//!   locked-down CSP. The UI never sends a path: it asks by result id, and the URL carries no
//!   path either.
//! - Folders: the child count and the first children, from the index.
//!
//! Everything here is blocking and bounded; the command runs it on the blocking pool, one at a
//! time, and drops work a newer request has cancelled.

use std::collections::VecDeque;
use std::fs::File;
use std::hash::{BuildHasher, RandomState};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use athrix_extract::{ExtractError, Kind, Limits, WorkerPool};
use athrix_fs_index::EntryId;

use crate::indexing::IndexView;
use crate::model::{Preview, PreviewBody, PreviewChild, PreviewEntry, UnsupportedReason};

/// Characters of text returned when the request does not say (`maxChars`).
pub const DEFAULT_MAX_CHARS: usize = 64 * 1024;
/// Upper bound for `maxChars`, whatever the UI asks.
pub const MAX_CHARS: usize = 1024 * 1024;
/// Largest document (DOCX, EPUB...) read for a preview. Plain text is only read as far as
/// `maxChars` needs, whatever its size.
pub const MAX_DOCUMENT_BYTES: u64 = 32 * 1024 * 1024;
/// Largest image served for a preview.
pub const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
/// Children listed in a folder preview (the count covers all of them).
pub const MAX_CHILDREN: usize = 200;
/// Children collected before sorting; past this the listing keeps index order.
const MAX_CHILDREN_SORTED: usize = 10_000;
/// Index entries scanned between two cancel checks in a folder preview.
const CANCEL_STRIDE: usize = 64 * 1024;

/// The URI scheme serving previewed images.
pub const SCHEME: &str = "athrix-preview";
/// The only window allowed to load from [`SCHEME`] (the quick popup has no preview).
pub const PREVIEW_WINDOW: &str = "main";
/// Images previewed recently whose URLs still work.
pub const ASSET_SLOTS: usize = 8;

/// Why a preview stopped early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

/// The image formats previewed inline, by extension, with the MIME type they are served as.
/// SVG is served as `image/svg+xml` and shown only through `<img>`, where scripts never run.
pub fn image_mime(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "png" => "image/png",
        "jpg" | "jpeg" | "jfif" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        _ => return None,
    })
}

/// Language hint for coloring source code, by extension. Text files without one are prose.
pub fn language_for(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "rs" => "rust",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "inl" | "ino" => "cpp",
        "cs" | "csx" => "csharp",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "scala" | "sc" | "groovy" | "gradle" => "scala",
        "go" => "go",
        "py" | "pyw" | "pyi" | "pyx" => "python",
        "rb" | "rake" | "gemspec" => "ruby",
        "php" | "phtml" => "php",
        "pl" | "pm" => "perl",
        "lua" => "lua",
        "r" => "r",
        "swift" => "swift",
        "dart" => "dart",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" | "tsx" => "typescript",
        "vue" | "svelte" | "astro" => "javascript",
        "css" | "scss" | "sass" | "less" | "styl" => "css",
        "sql" | "psql" => "sql",
        "sh" | "bash" | "zsh" | "fish" | "ksh" => "shell",
        "ps1" | "psm1" | "psd1" => "powershell",
        "bat" | "cmd" => "batch",
        "json" | "jsonc" | "json5" | "ndjson" | "jsonl" | "geojson" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "ini" | "cfg" | "conf" | "properties" | "editorconfig" => "ini",
        "xml" | "xsd" | "xsl" | "xslt" | "plist" | "csproj" | "vbproj" | "fsproj" | "vcxproj"
        | "props" | "targets" | "resx" | "xaml" | "svg" | "rss" | "atom" => "xml",
        "hs" => "haskell",
        "ex" | "exs" => "elixir",
        "zig" => "zig",
        "nim" => "nim",
        "dockerfile" | "containerfile" => "dockerfile",
        "makefile" | "mk" | "mak" | "make" | "cmake" => "makefile",
        "graphql" | "gql" => "graphql",
        "proto" => "protobuf",
        "tf" | "tfvars" | "hcl" => "hcl",
        _ => return None,
    })
}

/// Header of the preview of `entry`, a live entry of `view`.
pub fn entry_header(view: &IndexView, entry: EntryId) -> PreviewEntry {
    let name = view.name(entry);
    let meta = view.meta(entry);
    PreviewEntry {
        name: name.to_owned(),
        path: view.path(entry).to_string_lossy().into_owned(),
        is_dir: meta.is_dir,
        size: (!meta.is_dir).then_some(meta.size),
        modified: meta.modified,
        ext: extension(name, meta.is_dir),
    }
}

/// Lowercase extension of a file name, `""` for folders and names without one.
fn extension(name: &str, is_dir: bool) -> String {
    if is_dir {
        return String::new();
    }
    Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// Builds the preview of `entry` (a live entry of `view`). Blocking: reads the file.
/// `max_chars` is clamped to `1..=`[`MAX_CHARS`]. Stops with [`Cancelled`] when `cancel` is set
/// at a check point (the folder scan checks often; an extraction runs to its end, bounded by
/// its limits).
pub fn build_preview(
    view: &IndexView,
    entry: EntryId,
    max_chars: usize,
    cancel: &AtomicBool,
    assets: &PreviewAssets,
) -> Result<Preview, Cancelled> {
    let header = entry_header(view, entry);
    let body = if header.is_dir {
        folder_body(view, entry, cancel)?
    } else {
        file_body(
            Path::new(&header.path),
            &header.ext,
            max_chars.clamp(1, MAX_CHARS),
            assets,
        )
    };
    Ok(Preview {
        entry: header,
        body,
    })
}

/// The body for a file at `path` with extension `ext`. PDFs go through the app's shared
/// worker pool ([`crate::pdf_worker::shared_pool`]).
pub fn file_body(path: &Path, ext: &str, max_chars: usize, assets: &PreviewAssets) -> PreviewBody {
    if let Some(mime) = image_mime(ext) {
        return image_body(path, mime, assets);
    }
    file_body_with(path, ext, max_chars, &crate::pdf_worker::shared_pool())
}

/// [`file_body`] for a non-image file, with PDFs extracted through `pool`
/// ([`WorkerPool::extract_isolated`]: other formats stay in-process, same results as
/// [`athrix_extract::extract`]).
fn file_body_with(path: &Path, ext: &str, max_chars: usize, pool: &WorkerPool) -> PreviewBody {
    let limits = Limits {
        max_input_bytes: MAX_DOCUMENT_BYTES,
        max_output_chars: max_chars,
        max_entry_bytes: MAX_DOCUMENT_BYTES * 2,
        max_total_bytes: MAX_DOCUMENT_BYTES * 4,
        ..Limits::default()
    };
    match pool.extract_isolated(path, &limits) {
        Ok(x) if x.kind == Kind::Text => {
            let encoding = x.encoding.map(str::to_owned);
            match language_for(ext).or_else(|| language_for(&file_name_lower(path))) {
                Some(language) => PreviewBody::Code {
                    text: x.text,
                    truncated: x.truncated,
                    encoding,
                    language: language.to_owned(),
                },
                None => PreviewBody::Text {
                    text: x.text,
                    truncated: x.truncated,
                    encoding,
                },
            }
        }
        // Documents, PDFs included (`format: "pdf"`).
        Ok(x) => PreviewBody::Document {
            text: x.text,
            truncated: x.truncated,
            format: x.kind.name().to_owned(),
            title: x.metadata.title.filter(|t| !t.trim().is_empty()),
            author: x.metadata.author.filter(|a| !a.trim().is_empty()),
        },
        Err(e) => extract_failure(&e),
    }
}

fn extract_failure(e: &ExtractError) -> PreviewBody {
    let reason = match e {
        // No worker (not shipped, failed to start, or in its restart back-off), or a worker
        // built without PDF support: PDF previews are not available, whatever the file.
        ExtractError::WorkerUnavailable(_) => UnsupportedReason::Pdf,
        ExtractError::Unsupported(_) if is_pdf_error(e) => UnsupportedReason::Pdf,
        ExtractError::Unsupported(_) => UnsupportedReason::Format,
        ExtractError::TooLarge(_) => UnsupportedReason::TooLarge,
        ExtractError::Corrupt(_) => UnsupportedReason::Corrupt,
        ExtractError::Encrypted => UnsupportedReason::Encrypted,
        ExtractError::Io(io) => io_reason(io),
        // Worker timeout or crash (the worker was replaced), and future variants: it could
        // not be read.
        _ => UnsupportedReason::Unreadable,
    };
    unsupported(reason, e.to_string())
}

/// Whether an `Unsupported` error says PDF support is not compiled in (a worker built
/// without the `pdf` feature).
fn is_pdf_error(e: &ExtractError) -> bool {
    matches!(e, ExtractError::Unsupported(m)
        if *m == format!("{} extraction is not compiled in", Kind::Pdf.name()))
}

fn file_name_lower(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

fn unsupported(reason: UnsupportedReason, message: String) -> PreviewBody {
    PreviewBody::Unsupported { reason, message }
}

fn io_reason(e: &std::io::Error) -> UnsupportedReason {
    if e.kind() == ErrorKind::NotFound {
        UnsupportedReason::Missing
    } else {
        UnsupportedReason::Unreadable
    }
}

fn image_body(path: &Path, mime: &'static str, assets: &PreviewAssets) -> PreviewBody {
    match std::fs::metadata(path) {
        Ok(m) if !m.is_file() => unsupported(UnsupportedReason::Format, "not a file".into()),
        Ok(m) if m.len() > MAX_IMAGE_BYTES => unsupported(
            UnsupportedReason::TooLarge,
            format!("{} bytes (limit {MAX_IMAGE_BYTES})", m.len()),
        ),
        Ok(_) => {
            let token = assets.register(path.to_path_buf(), mime);
            PreviewBody::Image {
                src: asset_url(&token),
                mime: mime.to_owned(),
            }
        }
        Err(e) => unsupported(io_reason(&e), format!("cannot read the image: {e}")),
    }
}

/// URL of a registered asset as the webview must request it: `http://<scheme>.localhost/…` on
/// Windows and Android (WebView2 / Android WebView), `<scheme>://localhost/…` elsewhere.
pub fn asset_url(token: &str) -> String {
    if cfg!(any(windows, target_os = "android")) {
        format!("http://{SCHEME}.localhost/{token}")
    } else {
        format!("{SCHEME}://localhost/{token}")
    }
}

/// The children of folder `dir`: count, and the first [`MAX_CHILDREN`] (folders first, then by
/// name, case-insensitive). Scans the whole view (the index has no child lists), checking
/// `cancel` every [`CANCEL_STRIDE`] entries.
fn folder_body(
    view: &IndexView,
    dir: EntryId,
    cancel: &AtomicBool,
) -> Result<PreviewBody, Cancelled> {
    let mut count = 0usize;
    let mut children: Vec<(EntryId, bool)> = Vec::new();
    for (i, id) in (0..view.id_bound()).enumerate() {
        if i % CANCEL_STRIDE == 0 && cancel.load(Ordering::Relaxed) {
            return Err(Cancelled);
        }
        let Ok(id) = EntryId::try_from(id) else {
            break;
        };
        if id == dir || !view.contains(id) || view.parent(id) != Some(dir) {
            continue;
        }
        count += 1;
        if children.len() < MAX_CHILDREN_SORTED {
            children.push((id, view.meta(id).is_dir));
        }
    }
    let mut named: Vec<(String, PreviewChild)> = children
        .into_iter()
        .map(|(id, is_dir)| {
            let name = view.name(id).to_owned();
            (name.to_lowercase(), PreviewChild { name, is_dir })
        })
        .collect();
    named.sort_by(|(ka, a), (kb, b)| b.is_dir.cmp(&a.is_dir).then_with(|| ka.cmp(kb)));
    named.truncate(MAX_CHILDREN);
    Ok(PreviewBody::Folder {
        child_count: count,
        children: named.into_iter().map(|(_, c)| c).collect(),
    })
}

/// Images that may be served by the [`SCHEME`] handler: the last [`ASSET_SLOTS`] previewed,
/// each under a random token. Nothing else on the disk is reachable through the scheme.
#[derive(Debug, Default)]
pub struct PreviewAssets {
    slots: Mutex<VecDeque<Asset>>,
    counter: AtomicU64,
}

#[derive(Debug, Clone)]
struct Asset {
    token: String,
    path: PathBuf,
    mime: &'static str,
}

/// What the scheme handler answers: HTTP status, content type and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    /// HTTP status code.
    pub status: u16,
    /// `Content-Type` header.
    pub mime: &'static str,
    /// Response body.
    pub body: Vec<u8>,
}

impl Served {
    fn error(status: u16) -> Self {
        Served {
            status,
            mime: "text/plain",
            body: Vec::new(),
        }
    }

    /// 403: a request from a window that has no preview, or not a GET.
    pub fn forbidden() -> Self {
        Self::error(403)
    }
}

impl PreviewAssets {
    /// Registers `path` (an image of type `mime`) and returns its new token. The oldest
    /// registration is dropped past [`ASSET_SLOTS`].
    pub fn register(&self, path: PathBuf, mime: &'static str) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        // RandomState carries random keys: the token is not guessable from the counter.
        let token = format!(
            "{:016x}{:016x}",
            RandomState::new().hash_one(n),
            RandomState::new().hash_one(&path)
        );
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        slots.push_back(Asset {
            token: token.clone(),
            path,
            mime,
        });
        while slots.len() > ASSET_SLOTS {
            slots.pop_front();
        }
        token
    }

    fn lookup(&self, token: &str) -> Option<Asset> {
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|a| a.token == token)
            .cloned()
    }

    /// Answers a request for `uri_path` (`/<token>`): the registered image, up to
    /// [`MAX_IMAGE_BYTES`]; 404 for an unknown token, 413 if the file grew past the limit.
    pub fn serve(&self, uri_path: &str) -> Served {
        let token = uri_path.trim_start_matches('/');
        let Some(asset) = self.lookup(token) else {
            return Served::error(404);
        };
        let mut body = Vec::new();
        let read = File::open(&asset.path)
            .and_then(|f| f.take(MAX_IMAGE_BYTES + 1).read_to_end(&mut body));
        match read {
            Ok(_) if body.len() as u64 > MAX_IMAGE_BYTES => Served::error(413),
            Ok(_) => Served {
                status: 200,
                mime: asset.mime,
                body,
            },
            Err(e) if e.kind() == ErrorKind::NotFound => Served::error(404),
            Err(_) => Served::error(500),
        }
    }
}

/// Headers sent with every scheme response: no MIME sniffing, no caching, and a CSP that
/// blocks scripts and sub-resources should the image (SVG) ever be opened as a document.
pub const SECURITY_HEADERS: [(&str, &str); 3] = [
    ("X-Content-Type-Options", "nosniff"),
    ("Cache-Control", "no-store"),
    (
        "Content-Security-Policy",
        "default-src 'none'; style-src 'unsafe-inline'; img-src data:; sandbox",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use athrix_fs_index::{EntryMeta, IndexBuilder};
    use std::fs;

    fn dir_meta() -> EntryMeta {
        EntryMeta {
            is_dir: true,
            ..Default::default()
        }
    }

    fn body(path: &Path, max: usize) -> PreviewBody {
        let ext = extension(&path.file_name().unwrap().to_string_lossy(), false);
        file_body(path, &ext, max, &PreviewAssets::default())
    }

    #[test]
    fn text_is_cut_at_max_chars_and_cp1256_is_decoded() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("notes.txt");
        fs::write(&p, "سطر أول\nline two\n".repeat(100)).unwrap();
        match body(&p, 10) {
            PreviewBody::Text {
                text, truncated, ..
            } => {
                assert_eq!(text.chars().count(), 10);
                assert!(text.starts_with("سطر أول"));
                assert!(truncated);
            }
            other => panic!("{other:?}"),
        }
        // "مرحبا بكم في أثركس" in Windows-1256.
        let line: [u8; 20] = [
            0xE3, 0xD1, 0xCD, 0xC8, 0xC7, 0x20, 0xC8, 0xDF, 0xE3, 0x20, 0xDD, 0xED, 0x20, 0xC3,
            0xCB, 0xD1, 0xDF, 0xD3, 0x0D, 0x0A,
        ];
        let cp1256 = line.repeat(20);
        let p = tmp.path().join("legacy.txt");
        fs::write(&p, cp1256).unwrap();
        match body(&p, DEFAULT_MAX_CHARS) {
            PreviewBody::Text { text, encoding, .. } => {
                assert!(text.starts_with("مرحبا بكم في أثركس\n"), "{text:?}");
                assert_eq!(encoding.as_deref(), Some("windows-1256"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn code_gets_a_language_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("main.rs");
        fs::write(&p, "fn main() {}\n").unwrap();
        assert!(matches!(
            body(&p, 100),
            PreviewBody::Code { ref language, .. } if language == "rust"
        ));
        let p = tmp.path().join("Makefile");
        fs::write(&p, "all:\n\techo hi\n").unwrap();
        assert!(matches!(
            body(&p, 100),
            PreviewBody::Code { ref language, .. } if language == "makefile"
        ));
    }

    #[test]
    fn html_is_a_document_with_its_title() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("page.html");
        fs::write(
            &p,
            "<html><head><title>تقرير</title><script>alert(1)</script></head><body><p>مرحبا</p></body></html>",
        )
        .unwrap();
        match body(&p, 100) {
            PreviewBody::Document {
                text,
                format,
                title,
                ..
            } => {
                assert_eq!(format, "html");
                assert_eq!(title.as_deref(), Some("تقرير"));
                assert!(text.contains("مرحبا"));
                assert!(!text.contains("alert"));
            }
            other => panic!("{other:?}"),
        }
    }

    /// A pool whose worker never starts ("no worker shipped").
    fn missing_worker_pool() -> WorkerPool {
        let mut config = athrix_extract::WorkerConfig::default();
        config.program = Some(PathBuf::from("no-such-dir/athrix-extract-worker-missing"));
        WorkerPool::new(config)
    }

    /// A one-page PDF drawing `line` in Helvetica, with a title.
    fn simple_pdf(line: &str, title: &str) -> Vec<u8> {
        let content = format!("BT /F1 12 Tf 72 720 Td ({line}) Tj ET");
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
             /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_owned(),
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
            format!("<< /Title ({title}) >>"),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        let n = objs.len() + 1;
        out.extend_from_slice(format!("xref\n0 {n}\n0000000000 65535 f \n").as_bytes());
        for o in offsets {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!("trailer\n<< /Size {n} /Root 1 0 R /Info 6 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                .as_bytes(),
        );
        out
    }

    #[test]
    fn pdf_without_a_worker_is_unsupported_pdf() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = missing_worker_pool();
        let p = tmp.path().join("a.pdf");
        fs::write(&p, simple_pdf("hello", "t")).unwrap();
        for _ in 0..2 {
            // The second call hits the restart back-off: same answer, no wait.
            assert!(matches!(
                file_body_with(&p, "pdf", 100, &pool),
                PreviewBody::Unsupported {
                    reason: UnsupportedReason::Pdf,
                    ..
                }
            ));
        }
        // Found by its magic without the extension too.
        let p = tmp.path().join("scan");
        fs::write(&p, simple_pdf("hello", "t")).unwrap();
        assert!(matches!(
            file_body_with(&p, "", 100, &pool),
            PreviewBody::Unsupported {
                reason: UnsupportedReason::Pdf,
                ..
            }
        ));
        // Other formats do not need the worker.
        let p = tmp.path().join("a.txt");
        fs::write(&p, "نص").unwrap();
        assert!(matches!(
            file_body_with(&p, "txt", 100, &pool),
            PreviewBody::Text { ref text, .. } if text == "نص"
        ));
    }

    /// Through the real worker staged by `npm run worker` in `src-tauri/binaries/`.
    #[test]
    fn pdf_is_a_document_through_the_worker() {
        // The staged worker, not the copy beside the test binary: `cargo test --workspace`
        // also builds the worker crate (without `pdf`) into `target/<profile>/`, and newer
        // cargo puts test binaries in another directory.
        let program = crate::pdf_worker::staged_worker()
            .expect("no staged worker: run `npm run worker` in apps/desktop");
        let mut config = athrix_extract::WorkerConfig::default();
        config.program = Some(program);
        let pool = WorkerPool::new(config);
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("report.pdf");
        fs::write(&p, simple_pdf("Hello Athrix preview", "Quarterly")).unwrap();
        match file_body_with(&p, "pdf", 100, &pool) {
            PreviewBody::Document {
                text,
                format,
                title,
                truncated,
                ..
            } => {
                assert_eq!(format, "pdf");
                assert!(text.contains("Hello Athrix preview"), "{text:?}");
                assert_eq!(title.as_deref(), Some("Quarterly"));
                assert!(!truncated);
            }
            other => panic!("{other:?}"),
        }
        // `maxChars` bounds the worker's output too.
        match file_body_with(&p, "pdf", 5, &pool) {
            PreviewBody::Document {
                text, truncated, ..
            } => {
                assert_eq!(text.chars().count(), 5);
                assert!(truncated);
            }
            other => panic!("{other:?}"),
        }
        // A broken PDF is the document's problem, not a missing feature.
        let p = tmp.path().join("broken.pdf");
        fs::write(&p, b"%PDF-1.7\n").unwrap();
        assert!(matches!(
            file_body_with(&p, "pdf", 100, &pool),
            PreviewBody::Unsupported {
                reason: UnsupportedReason::Corrupt,
                ..
            }
        ));
        assert_eq!(pool.stats().spawned, 1);
    }

    #[test]
    fn binary_and_missing_files_are_unsupported() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("tool.exe");
        fs::write(&p, b"MZ\x90\x00\x03\x00\x00\x00").unwrap();
        assert!(matches!(
            body(&p, 100),
            PreviewBody::Unsupported {
                reason: UnsupportedReason::Format,
                ..
            }
        ));
        assert!(matches!(
            body(&tmp.path().join("gone.txt"), 100),
            PreviewBody::Unsupported {
                reason: UnsupportedReason::Missing,
                ..
            }
        ));
        // A document that is not what its extension says.
        let p = tmp.path().join("fake.docx");
        fs::write(&p, b"not a zip").unwrap();
        assert!(matches!(body(&p, 100), PreviewBody::Unsupported { .. }));
    }

    #[test]
    fn images_are_served_by_token_only() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("logo.svg");
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script></svg>"#;
        fs::write(&p, svg).unwrap();
        let assets = PreviewAssets::default();
        let PreviewBody::Image { src, mime } = file_body(&p, "svg", 100, &assets) else {
            panic!("expected an image");
        };
        assert_eq!(mime, "image/svg+xml");
        let token = src.rsplit('/').next().unwrap();
        assert_eq!(token.len(), 32);
        assert!(!src.contains("logo"), "the URL carries no path: {src}");
        let served = assets.serve(&format!("/{token}"));
        assert_eq!(served.status, 200);
        assert_eq!(served.mime, "image/svg+xml");
        assert_eq!(served.body, svg);
        // Unknown tokens, paths and traversal attempts: nothing.
        assert_eq!(assets.serve("/0123").status, 404);
        assert_eq!(assets.serve(&p.to_string_lossy()).status, 404);
        assert_eq!(assets.serve("/../logo.svg").status, 404);
        // Old registrations expire.
        for _ in 0..ASSET_SLOTS {
            assets.register(p.clone(), "image/png");
        }
        assert_eq!(assets.serve(&format!("/{token}")).status, 404);
    }

    #[test]
    fn folders_list_children_folders_first() {
        let mut b = IndexBuilder::new();
        let root = b.push(None, "/data", dir_meta());
        let docs = b.push(Some(root), "docs", dir_meta());
        b.push(Some(docs), "inner.txt", EntryMeta::default());
        b.push(Some(root), "b.txt", EntryMeta::default());
        b.push(Some(root), "A.txt", EntryMeta::default());
        b.push(Some(root), "zeta", dir_meta());
        let view = IndexView::from_index(1, b.finish());
        let no = AtomicBool::new(false);
        let p = build_preview(&view, root, 100, &no, &PreviewAssets::default()).unwrap();
        assert!(p.entry.is_dir);
        assert_eq!(p.entry.size, None);
        match p.body {
            PreviewBody::Folder {
                child_count,
                children,
            } => {
                assert_eq!(child_count, 4);
                let names: Vec<_> = children.iter().map(|c| c.name.as_str()).collect();
                assert_eq!(names, ["docs", "zeta", "A.txt", "b.txt"]);
            }
            other => panic!("{other:?}"),
        }
        let yes = AtomicBool::new(true);
        assert_eq!(
            build_preview(&view, root, 100, &yes, &PreviewAssets::default()),
            Err(Cancelled)
        );
    }

    #[test]
    fn serializes_tagged_by_kind() {
        let body = PreviewBody::Folder {
            child_count: 1,
            children: vec![PreviewChild {
                name: "x".into(),
                is_dir: true,
            }],
        };
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            serde_json::json!({
                "kind": "folder", "childCount": 1, "children": [{ "name": "x", "isDir": true }]
            })
        );
        let body = unsupported(UnsupportedReason::TooLarge, "big".into());
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            serde_json::json!({ "kind": "unsupported", "reason": "tooLarge", "message": "big" })
        );
    }
}

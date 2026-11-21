//! Plain text, Markdown and source code: encoding and binary detection.

use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE};

use crate::{ExtractError, Extracted, Kind, Limits, Metadata};

/// File extensions (lowercase, without the dot) extracted as plain text:
/// prose, markup kept as source, data/config files and source code.
pub const TEXT_EXTENSIONS: &[&str] = &[
    // Prose and markup kept as source
    "txt",
    "text",
    "md",
    "markdown",
    "mdown",
    "mkd",
    "mkdn",
    "mdx",
    "rst",
    "adoc",
    "asciidoc",
    "org",
    "tex",
    "ltx",
    "bib",
    "rtx",
    "log",
    "nfo",
    "diz",
    "me",
    "1st",
    "srt",
    "vtt",
    "sub",
    "po",
    "pot", // Data and configuration
    "csv",
    "tsv",
    "psv",
    "json",
    "jsonc",
    "json5",
    "ndjson",
    "jsonl",
    "geojson",
    "yaml",
    "yml",
    "toml",
    "ini",
    "cfg",
    "conf",
    "config",
    "properties",
    "env",
    "reg",
    "inf",
    "xml",
    "xsd",
    "xsl",
    "xslt",
    "dtd",
    "svg",
    "plist",
    "rss",
    "atom",
    "opml",
    "kml",
    "gpx",
    "csproj",
    "vbproj",
    "fsproj",
    "vcxproj",
    "sln",
    "props",
    "targets",
    "resx",
    "xaml",
    "manifest",
    "editorconfig",
    "gitignore",
    "gitattributes",
    "gitmodules",
    "dockerignore",
    "npmrc",
    "lock",
    "graphql",
    "gql",
    "proto",
    "thrift",
    "avsc",
    "tf",
    "tfvars",
    "hcl",
    "nix",
    "diff",
    "patch",
    "http",
    "rest", // Source code
    "rs",
    "c",
    "h",
    "cc",
    "cpp",
    "cxx",
    "c++",
    "hpp",
    "hh",
    "hxx",
    "inl",
    "ipp",
    "cs",
    "csx",
    "java",
    "kt",
    "kts",
    "scala",
    "sc",
    "groovy",
    "gradle",
    "go",
    "py",
    "pyw",
    "pyi",
    "pyx",
    "rb",
    "rake",
    "gemspec",
    "php",
    "phtml",
    "pl",
    "pm",
    "t",
    "lua",
    "r",
    "jl",
    "swift",
    "m",
    "mm",
    "dart",
    "js",
    "mjs",
    "cjs",
    "jsx",
    "ts",
    "mts",
    "cts",
    "tsx",
    "vue",
    "svelte",
    "astro",
    "css",
    "scss",
    "sass",
    "less",
    "styl",
    "sql",
    "psql",
    "sh",
    "bash",
    "zsh",
    "fish",
    "ksh",
    "ps1",
    "psm1",
    "psd1",
    "bat",
    "cmd",
    "vbs",
    "vba",
    "bas",
    "asm",
    "s",
    "nasm",
    "f",
    "for",
    "f90",
    "f95",
    "hs",
    "lhs",
    "ml",
    "mli",
    "fs",
    "fsi",
    "fsx",
    "ex",
    "exs",
    "erl",
    "hrl",
    "clj",
    "cljs",
    "cljc",
    "edn",
    "elm",
    "nim",
    "zig",
    "v",
    "sv",
    "svh",
    "vhd",
    "vhdl",
    "cmake",
    "mk",
    "mak",
    "make",
    "dockerfile",
    "containerfile",
    "ino",
    "pde",
    "glsl",
    "hlsl",
    "wgsl",
    "frag",
    "vert",
    "shader",
    "cu",
    "cuh",
    "d",
    "pas",
    "pp",
    "lisp",
    "lsp",
    "el",
    "scm",
    "rkt",
    "tcl",
    "awk",
    "sed",
    "ahk",
    "au3",
    "applescript",
    "coffee",
    "ls",
    "cr",
    "rbs",
    "sol",
    "move",
    "cairo",
    "gd",
    "hx",
    "haml",
    "pug",
    "jade",
    "ejs",
    "hbs",
    "mustache",
    "liquid",
    "jinja",
    "j2",
    "twig",
    "tpl",
    "razor",
    "cshtml",
    "aspx",
    "jsp",
    "erb",
];

/// Lowercase file names without an extension that are extracted as text.
pub const TEXT_FILE_NAMES: &[&str] = &[
    "readme",
    "license",
    "licence",
    "copying",
    "notice",
    "authors",
    "contributors",
    "changelog",
    "changes",
    "news",
    "history",
    "todo",
    "install",
    "makefile",
    "gnumakefile",
    "dockerfile",
    "containerfile",
    "gemfile",
    "rakefile",
    "procfile",
    "vagrantfile",
    "jenkinsfile",
    "brewfile",
    "podfile",
    "justfile",
    "cmakelists.txt",
    ".gitignore",
    ".gitattributes",
    ".editorconfig",
    ".bashrc",
    ".zshrc",
    ".profile",
    ".env",
];

/// How many bytes are inspected for binary content and UTF-16 patterns.
const SAMPLE_BYTES: usize = 8 * 1024;
/// How many bytes are fed to the legacy-encoding detector.
const DETECT_BYTES: usize = 1024 * 1024;

/// Bytes of plain text worth reading for `limits`: enough for the output
/// budget even at 4 bytes per character.
pub(crate) fn input_cap(limits: &Limits) -> u64 {
    let for_output = (limits.max_output_chars as u64)
        .saturating_mul(4)
        .saturating_add(4);
    for_output.min(limits.max_input_bytes)
}

/// Extracts plain text. `input_truncated` says the caller cut the input.
pub(crate) fn extract(
    bytes: &[u8],
    input_truncated: bool,
    limits: &Limits,
) -> Result<Extracted, ExtractError> {
    let (decoded, encoding) = decode(bytes)?;
    let mut decoded = decoded.as_str();
    if input_truncated {
        // The cut may have split the last character.
        decoded = decoded.trim_end_matches('\u{FFFD}');
    }
    let (text, out_truncated) = normalize(decoded, limits.max_output_chars);
    Ok(Extracted {
        text,
        kind: Kind::Text,
        truncated: input_truncated || out_truncated,
        metadata: Metadata::default(),
        encoding: Some(encoding.name()),
    })
}

/// Decodes text bytes with BOM, UTF-16 and legacy-encoding detection.
/// Binary content fails with [`ExtractError::Unsupported`].
pub(crate) fn decode(bytes: &[u8]) -> Result<(String, &'static Encoding), ExtractError> {
    if let Some((encoding, bom)) = Encoding::for_bom(bytes) {
        let (text, _) = encoding.decode_without_bom_handling(&bytes[bom..]);
        return Ok((text.into_owned(), encoding));
    }
    let sample = &bytes[..bytes.len().min(SAMPLE_BYTES)];
    if let Some(encoding) = utf16_without_bom(sample) {
        let (text, _) = encoding.decode_without_bom_handling(bytes);
        return Ok((text.into_owned(), encoding));
    }
    if looks_binary(sample) {
        return Err(ExtractError::Unsupported("binary content".into()));
    }
    let encoding = detect_encoding(bytes);
    let (text, _) = encoding.decode_without_bom_handling(bytes);
    Ok((text.into_owned(), encoding))
}

/// Picks the encoding of BOM-less, non-UTF-16 text: UTF-8 when the bytes
/// are valid UTF-8 (a character cut at the very end is tolerated), else the
/// legacy encoding `chardetng` finds most likely (Windows-1256 or
/// ISO-8859-6 for Arabic).
pub(crate) fn detect_encoding(bytes: &[u8]) -> &'static Encoding {
    match std::str::from_utf8(bytes) {
        Ok(_) => return UTF_8,
        Err(e) if e.error_len().is_none() && bytes.len() - e.valid_up_to() < 4 => return UTF_8,
        Err(_) => {}
    }
    let sample = &bytes[..bytes.len().min(DETECT_BYTES)];
    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    detector.feed(sample, sample.len() == bytes.len());
    detector.guess(None, chardetng::Utf8Detection::Allow)
}

/// Recognises UTF-16 text without a BOM: one byte column (the high bytes of
/// Latin text) is mostly zero while the other almost never is.
fn utf16_without_bom(sample: &[u8]) -> Option<&'static Encoding> {
    let pairs = sample.len() / 2;
    if pairs < 4 {
        return None;
    }
    let (mut even_zero, mut odd_zero) = (0usize, 0usize);
    for pair in sample.chunks_exact(2) {
        even_zero += usize::from(pair[0] == 0);
        odd_zero += usize::from(pair[1] == 0);
    }
    let encoding = if odd_zero * 10 >= pairs * 3 && even_zero * 50 <= pairs {
        UTF_16LE
    } else if even_zero * 10 >= pairs * 3 && odd_zero * 50 <= pairs {
        UTF_16BE
    } else {
        return None;
    };
    // Confirm: the decoded sample must be clean text.
    let (text, had_errors) = encoding.decode_without_bom_handling(sample);
    let total = text.chars().count().max(1);
    let controls = text
        .chars()
        .filter(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\u{0C}'))
        .count();
    let bad = text.chars().filter(|&c| c == '\u{FFFD}').count();
    // One replacement char is allowed for a sample cut inside a surrogate pair.
    (!had_errors || bad <= 1).then_some(())?;
    (controls * 100 <= total).then_some(encoding)
}

/// Binary content: any NUL byte, or more than 5% control bytes other than
/// tab, line feed, form feed, carriage return and escape.
fn looks_binary(sample: &[u8]) -> bool {
    if sample.contains(&0) {
        return true;
    }
    let controls = sample
        .iter()
        .filter(|&&b| b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r' | 0x0C | 0x1B | 0x1A))
        .count();
    controls * 20 > sample.len()
}

/// Normalises decoded plain text: line endings become `\n`, control
/// characters (other than tab) and a leading BOM are dropped, and the
/// result is cut at `max_chars`.
fn normalize(text: &str, max_chars: usize) -> (String, bool) {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let mut out = String::with_capacity(text.len().min(max_chars.saturating_mul(2)));
    let mut count = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let c = match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                '\n'
            }
            '\n' | '\t' => c,
            '\u{0C}' => '\n',
            c if c.is_control() => continue,
            c => c,
        };
        if count >= max_chars {
            return (out, true);
        }
        out.push(c);
        count += 1;
    }
    (out, false)
}

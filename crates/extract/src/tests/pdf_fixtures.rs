//! Hand-written PDF builders, shared by the unit tests (`tests::pdf`) and
//! the worker integration tests (`tests/worker.rs`, through `#[path]`).
//! PDFs are written by hand with a computed xref table.

// Fixture builders favour readability over allocation-free string building.
#![allow(clippy::format_collect, dead_code)]

/// A PDF from object bodies (object numbers start at 1; object 7 is the
/// information dictionary).
pub fn build(objs: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(o);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R /Info 7 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objs.len() + 1
        )
        .as_bytes(),
    );
    out
}

pub fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut v = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    v.extend_from_slice(data);
    v.extend_from_slice(b"\nendstream");
    v
}

pub fn utf16hex(s: &str) -> String {
    s.encode_utf16().map(|u| format!("{u:04X}")).collect()
}

/// One page: a line of Arabic glyphs (Type0 font, Identity-H, ToUnicode
/// `map`) drawn in the given glyph order, a line of Helvetica `english`,
/// and the Arabic line again. `content_filter` optionally replaces the
/// page content stream (data, filter name).
pub fn arabic_pdf(
    map: &[(u16, &str)],
    drawn: &[u16],
    english: &str,
    content: Option<(Vec<u8>, &str)>,
) -> Vec<u8> {
    let mut cmap = String::from(
        "/CIDInit /ProcSet findresource begin 12 dict begin begincmap /CMapName /X def 1 begincodespacerange <0000> <FFFF> endcodespacerange\n",
    );
    cmap.push_str(&format!("{} beginbfchar\n", map.len()));
    for (cid, u) in map {
        cmap.push_str(&format!("<{cid:04X}> <{}>\n", utf16hex(u)));
    }
    cmap.push_str("endbfchar endcmap CMapName currentdict /CMap defineresource pop end end");
    let glyphs: String = drawn.iter().map(|g| format!("{g:04X}")).collect();
    let text = format!(
        "BT /F1 24 Tf 100 700 Td <{glyphs}> Tj ET BT /F2 12 Tf 100 650 Td ({english}) Tj ET BT /F1 24 Tf 100 600 Td <{glyphs}> Tj ET"
    );
    let page_content = match content {
        Some((data, filter)) => stream(&format!("/Filter /{filter}"), &data),
        None => stream("", text.as_bytes()),
    };
    build(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R /F2 8 0 R >> >> >>".to_vec(),
        page_content,
        b"<< /Type /Font /Subtype /Type0 /BaseFont /Arial /Encoding /Identity-H /DescendantFonts [6 0 R] /ToUnicode 9 0 R >>".to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /Arial /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /DW 500 /FontDescriptor 10 0 R >>".to_vec(),
        format!("<< /Title <FEFF{}> /Author (Sami) /CreationDate (D:20240101120000Z) >>", utf16hex("عنوان")).into_bytes(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        stream("", cmap.as_bytes()),
        b"<< /Type /FontDescriptor /FontName /Arial /Flags 32 /FontBBox [0 0 1000 1000] /ItalicAngle 0 /Ascent 900 /Descent -200 /CapHeight 700 /StemV 80 >>".to_vec(),
    ])
}

/// "سلام" as presentation forms: seen initial, lam-alef final, meem isolated.
pub const FORMS: &[(u16, &str)] = &[(1, "\u{FEB3}"), (2, "\u{FEFC}"), (3, "\u{FEE1}")];
/// "سلام" as base letters; the lam-alef glyph maps to two characters.
pub const BASE: &[(u16, &str)] = &[(1, "\u{0633}"), (2, "\u{0644}\u{0627}"), (3, "\u{0645}")];

/// One page whose content stream is `content` (unfiltered), with no
/// resources: for content-operator edge cases. `extra` objects are
/// numbered from 8.
pub fn content_pdf(content: &[u8], extra: &[Vec<u8>]) -> Vec<u8> {
    let mut objs = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << >> >>"
            .to_vec(),
        stream("", content),
        b"<< >>".to_vec(),
        b"<< >>".to_vec(),
        b"<< /Title (content) >>".to_vec(),
    ];
    objs.extend_from_slice(extra);
    build(&objs)
}

/// A PDF that makes `pdf-extract` panic: the `Do` operator with a number
/// instead of an XObject name (the library unwraps the operand).
pub fn panicking_pdf() -> Vec<u8> {
    content_pdf(b"BT /F1 12 Tf ET 1 Do", &[])
}

/// A page that draws Form XObject `/X1`, whose own resources name itself
/// and whose content draws `/X1` again. `pdf-extract` recurses on `Do`
/// without a depth limit, so this overflows the stack: never extract it
/// in-process.
pub fn self_drawing_pdf() -> Vec<u8> {
    build(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 5 0 R /Resources << /XObject << /X1 4 0 R >> >> >>".to_vec(),
        stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << /XObject << /X1 4 0 R >> >>",
            b"/X1 Do",
        ),
        stream("", b"/X1 Do"),
        b"<< >>".to_vec(),
        b"<< /Title (loop) >>".to_vec(),
    ])
}

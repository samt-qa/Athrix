//! PDF tests (only with the `pdf` feature). PDFs are written by hand with
//! a computed xref table.

use std::io::Write;

use proptest::prelude::*;

use crate::{ExtractError, Kind, Limits, extract_bytes};

/// A PDF from object bodies (object numbers start at 1; object 7 is the
/// information dictionary).
fn build(objs: &[Vec<u8>]) -> Vec<u8> {
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

fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut v = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    v.extend_from_slice(data);
    v.extend_from_slice(b"\nendstream");
    v
}

fn utf16hex(s: &str) -> String {
    s.encode_utf16().map(|u| format!("{u:04X}")).collect()
}

/// One page: a line of Arabic glyphs (Type0 font, Identity-H, ToUnicode
/// `map`) drawn in the given glyph order, a line of Helvetica `english`,
/// and the Arabic line again. `content_filter` optionally replaces the
/// page content stream (data, filter name).
fn arabic_pdf(
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
const FORMS: &[(u16, &str)] = &[(1, "\u{FEB3}"), (2, "\u{FEFC}"), (3, "\u{FEE1}")];
/// "سلام" as base letters; the lam-alef glyph maps to two characters.
const BASE: &[(u16, &str)] = &[(1, "\u{0633}"), (2, "\u{0644}\u{0627}"), (3, "\u{0645}")];

#[test]
fn arabic_visual_order_and_presentation_forms() {
    for map in [FORMS, BASE] {
        for drawn in [&[3, 2, 1][..], &[1, 2, 3][..]] {
            // Glyph order in the stream does not matter: positions do. With
            // the default advance (DW 500) glyphs 1,2,3 drawn in that order
            // land left to right, so [1,2,3] is a (broken) logical-order
            // drawing that reads backwards on the page.
            let pdf = arabic_pdf(map, drawn, "Hello 2024", None);
            let e = extract_bytes(Kind::Pdf, &pdf, &Limits::default()).unwrap();
            let expected_ar = if drawn[0] == 3 {
                "سلام"
            } else {
                "ملاس"
            };
            assert_eq!(
                e.text,
                format!("{expected_ar}\nHello 2024\n{expected_ar}"),
                "{map:?} {drawn:?}"
            );
        }
    }
    let e = extract_bytes(
        Kind::Pdf,
        &arabic_pdf(FORMS, &[3, 2, 1], "x", None),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(e.metadata.title.as_deref(), Some("عنوان"));
    assert_eq!(e.metadata.author.as_deref(), Some("Sami"));
}

#[test]
fn flate_bomb_is_rejected() {
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    let zeros = vec![b' '; 1 << 20];
    for _ in 0..64 {
        z.write_all(&zeros).unwrap();
    }
    let data = z.finish().unwrap();
    let pdf = arabic_pdf(FORMS, &[3, 2, 1], "x", Some((data, "FlateDecode")));
    let limits = Limits {
        max_entry_bytes: 4 << 20,
        ..Limits::default()
    };
    let r = extract_bytes(Kind::Pdf, &pdf, &limits);
    assert!(matches!(r, Err(ExtractError::TooLarge(_))), "{r:?}");
}

#[test]
fn not_a_pdf_is_corrupt() {
    let r = extract_bytes(Kind::Pdf, b"hello", &Limits::default());
    assert!(matches!(r, Err(ExtractError::Corrupt(_))));
}

proptest! {
    #![proptest_config(ProptestConfig { cases: super::cases(128), ..ProptestConfig::default() })]

    /// Through the public, guarded API: `pdf-extract` panics on some of
    /// these inputs, which must come back as errors.
    #[test]
    fn mutated_pdfs_return(
        edits in proptest::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..12),
        cut in any::<prop::sample::Index>(),
        do_cut in any::<bool>(),
    ) {
        let mut pdf = arabic_pdf(FORMS, &[3, 2, 1], "Hello 2024", None);
        for (at, value) in edits {
            let i = at.index(pdf.len());
            pdf[i] = value;
        }
        if do_cut {
            pdf.truncate(cut.index(pdf.len()));
        }
        let _ = extract_bytes(Kind::Pdf, &pdf, &Limits::default());
    }
}

//! PDF tests (only with the `pdf` feature). The PDFs come from the
//! hand-written builders in `pdf_fixtures`.

use std::io::Write;

use proptest::prelude::*;

use super::pdf_fixtures::*;
use crate::{ExtractError, Kind, Limits, extract_bytes};

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

/// A PDF whose only page dictionary (object 9) lives in a compressed object
/// stream, followed by `padding` spaces of inflated filler.
fn objstm_pdf(padding: usize) -> Vec<u8> {
    let page = b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F2 5 0 R >> >> >>";
    let header = b"9 0 ";
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    z.write_all(header).unwrap();
    z.write_all(page).unwrap();
    let chunk = vec![b' '; 1 << 20];
    let mut left = padding;
    while left > 0 {
        let n = left.min(chunk.len());
        z.write_all(&chunk[..n]).unwrap();
        left -= n;
    }
    let data = z.finish().unwrap();
    build(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [9 0 R] /Count 1 >>".to_vec(),
        stream(
            &format!(
                "/Type /ObjStm /N 1 /First {} /Filter /FlateDecode",
                header.len()
            ),
            &data,
        ),
        stream("", b"BT /F2 12 Tf 100 650 Td (Hello from ObjStm) Tj ET"),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        b"<< >>".to_vec(),
        b"<< /Title (objstm) >>".to_vec(),
    ])
}

#[test]
fn object_streams_load_within_budget() {
    let e = extract_bytes(Kind::Pdf, &objstm_pdf(0), &Limits::default()).unwrap();
    assert_eq!(e.text, "Hello from ObjStm");
}

#[test]
fn object_stream_bomb_is_rejected_before_lopdf_inflates_it() {
    let limits = Limits {
        max_entry_bytes: 4 << 20,
        ..Limits::default()
    };
    let r = extract_bytes(Kind::Pdf, &objstm_pdf(64 << 20), &limits);
    assert!(matches!(r, Err(ExtractError::TooLarge(_))), "{r:?}");
    // The total budget applies across object streams too.
    let limits = Limits {
        max_total_bytes: 1 << 20,
        ..Limits::default()
    };
    let r = extract_bytes(Kind::Pdf, &objstm_pdf(2 << 20), &limits);
    assert!(matches!(r, Err(ExtractError::TooLarge(_))), "{r:?}");
}

#[test]
fn library_panic_is_reported_as_corrupt() {
    // In-process, the crate-wide guard catches the panic; out of process the
    // worker does the same and keeps serving (see `tests/worker.rs`).
    let r = extract_bytes(Kind::Pdf, &panicking_pdf(), &Limits::default());
    assert!(
        matches!(&r, Err(ExtractError::Corrupt(m)) if m.contains("parser failure")),
        "{r:?}"
    );
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

//! Property tests: no parser panics on random or mutated input, and output
//! always respects the limits. Parsers are called without the
//! `catch_unwind` guard so a panic fails the test.

use proptest::prelude::*;

use super::fixtures::*;
use crate::sink::Sink;
use crate::{Kind, Limits, Metadata, eml, extract_structured, html, text};

const KINDS: &[Kind] = &[
    Kind::Text,
    Kind::Html,
    Kind::Docx,
    Kind::Xlsx,
    Kind::Pptx,
    Kind::Odt,
    Kind::Ods,
    Kind::Odp,
    Kind::Epub,
    Kind::Eml,
];

fn small_limits() -> Limits {
    Limits {
        max_input_bytes: 1 << 20,
        max_output_chars: 10_000,
        max_entries: 200,
        max_entry_bytes: 1 << 20,
        max_total_bytes: 4 << 20,
        max_depth: 8,
    }
}

fn run(kind: Kind, bytes: &[u8]) {
    let limits = small_limits();
    if let Ok(e) = extract_structured(kind, bytes, &limits) {
        assert!(e.text.chars().count() <= limits.max_output_chars);
        assert!(kind == Kind::Text || !e.text.ends_with('\n'));
    }
}

/// Valid documents of every zip-based kind, stored uncompressed so that
/// mutations reach the XML parsers instead of failing the CRC check.
fn seeds() -> Vec<(Kind, Vec<u8>)> {
    vec![
        (Kind::Docx, docx(&[AR, "a|b", EN], true)),
        (
            Kind::Xlsx,
            xlsx(&[(
                "s",
                vec![vec![Cell::Shared(AR2), Cell::Inline(EN), Cell::Number("1")]],
            )]),
        ),
        (Kind::Pptx, pptx(&[&[AR2], &[EN]])),
        (
            Kind::Odt,
            odf(
                "application/vnd.oasis.opendocument.text",
                &format!("<office:text><text:p>{AR2}<text:s/></text:p></office:text>"),
                &[],
            ),
        ),
        (Kind::Epub, epub(&[("t", &[AR2])])),
    ]
}

/// Markup-flavoured strings reach deeper into the HTML/MIME parsers than
/// uniform random bytes.
fn markupish() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            Just("<".to_owned()),
            Just(">".to_owned()),
            Just("</".to_owned()),
            Just("<!--".to_owned()),
            Just("-->".to_owned()),
            Just("&".to_owned()),
            Just("&#".to_owned()),
            Just("&#x".to_owned()),
            Just(";".to_owned()),
            Just("\"".to_owned()),
            Just("'".to_owned()),
            Just("=".to_owned()),
            Just("=?".to_owned()),
            Just("?=".to_owned()),
            Just("?B?".to_owned()),
            Just("?Q?".to_owned()),
            Just("utf-8".to_owned()),
            Just("script".to_owned()),
            Just("title".to_owned()),
            Just("pre".to_owned()),
            Just("p".to_owned()),
            Just("meta charset=".to_owned()),
            Just("\n".to_owned()),
            Just("\r\n".to_owned()),
            Just("--b".to_owned()),
            Just("Content-Type: multipart/mixed; boundary=b\n".to_owned()),
            Just("Content-Type: message/rfc822\n".to_owned()),
            Just("Content-Transfer-Encoding: base64\n".to_owned()),
            Just("Content-Transfer-Encoding: quoted-printable\n".to_owned()),
            Just("Subject: ".to_owned()),
            Just(" ".to_owned()),
            Just("ب".to_owned()),
            Just("\u{FFFD}".to_owned()),
            "[a-z0-9]{1,4}",
        ],
        0..120,
    )
    .prop_map(|v| v.concat())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: super::cases(256), ..ProptestConfig::default() })]

    #[test]
    fn random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        for &kind in KINDS {
            run(kind, &bytes);
        }
    }

    #[test]
    fn zip_prefixed_random_bytes_never_panic(tail in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let mut bytes = b"PK\x03\x04".to_vec();
        bytes.extend_from_slice(&tail);
        for &kind in &KINDS[2..9] {
            run(kind, &bytes);
        }
    }

    #[test]
    fn mutated_documents_never_panic(
        seed in 0usize..5,
        edits in proptest::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..16),
        cut in any::<prop::sample::Index>(),
        do_cut in any::<bool>(),
    ) {
        let (kind, mut bytes) = seeds().swap_remove(seed);
        for (at, value) in edits {
            let i = at.index(bytes.len());
            bytes[i] = value;
        }
        if do_cut {
            bytes.truncate(cut.index(bytes.len()));
        }
        run(kind, &bytes);
    }

    #[test]
    fn markup_never_panics(src in markupish()) {
        let mut sink = Sink::new(1000);
        html::to_text(&src, &mut sink, &mut Metadata::default());
        let _ = eml::decode_words(&src);
        let _ = eml::quoted_printable(src.as_bytes(), true);
        let _ = eml::base64(src.as_bytes());
        run(Kind::Eml, src.as_bytes());
        run(Kind::Html, src.as_bytes());
        let _ = text::decode(src.as_bytes());
    }

    #[test]
    fn sink_respects_budget(parts in proptest::collection::vec(("\\PC{0,20}", 0u8..3), 0..50), max in 0usize..40) {
        let mut sink = Sink::new(max);
        for (s, op) in &parts {
            match op {
                0 => sink.text(s),
                1 => sink.newline(),
                _ => sink.space(),
            }
        }
        let (out, _) = sink.finish();
        prop_assert!(out.chars().count() <= max);
        prop_assert!(!out.contains("\n\n"));
        prop_assert!(!out.contains("  "));
        prop_assert!(!out.starts_with(' ') && !out.starts_with('\n'));
    }
}

//! Behaviour of each format, with Arabic and English content.

use encoding_rs::{ISO_8859_6, UTF_16BE, UTF_16LE, WINDOWS_1256};

use super::fixtures::*;
use crate::{ExtractError, Kind, Limits, extract, extract_bytes, sniff};

fn ok(kind: Kind, bytes: &[u8]) -> crate::Extracted {
    extract_bytes(kind, bytes, &Limits::default()).unwrap_or_else(|e| panic!("{kind:?}: {e}"))
}

// ---------------------------------------------------------------- text

#[test]
fn text_utf8_with_and_without_bom() {
    let src = format!("{AR}\r\n{EN}\n");
    let plain = ok(Kind::Text, src.as_bytes());
    assert_eq!(plain.text, format!("{AR}\n{EN}\n"));
    assert_eq!(plain.encoding, Some("UTF-8"));

    let mut bom = b"\xEF\xBB\xBF".to_vec();
    bom.extend_from_slice(src.as_bytes());
    assert_eq!(ok(Kind::Text, &bom).text, plain.text);
}

fn utf16(s: &str, le: bool, bom: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if bom {
        out.extend_from_slice(if le { b"\xFF\xFE" } else { b"\xFE\xFF" });
    }
    for u in s.encode_utf16() {
        out.extend_from_slice(&if le { u.to_le_bytes() } else { u.to_be_bytes() });
    }
    out
}

#[test]
fn text_utf16_with_bom() {
    let src = format!("{AR} — {EN}\n");
    for le in [true, false] {
        let e = ok(Kind::Text, &utf16(&src, le, true));
        assert_eq!(e.text, src);
        assert_eq!(
            e.encoding,
            Some(if le { UTF_16LE.name() } else { UTF_16BE.name() })
        );
    }
}

#[test]
fn text_utf16_without_bom() {
    let src = format!("{EN}: {AR}, {AR2}. Plain ASCII words help detection.\n");
    for le in [true, false] {
        let e = ok(Kind::Text, &utf16(&src, le, false));
        assert_eq!(e.text, src, "le={le}");
    }
}

#[test]
fn text_arabic_windows_1256() {
    let samples = [
        AR2,
        "بسم الله الرحمن الرحيم",
        "قال المدير إن الاجتماع سيعقد يوم الأحد في مقر الشركة الرئيسي.",
        "تقرير المبيعات السنوي 2024 – الربع الأول: ارتفعت الإيرادات بنسبة 12%.",
        AR,
    ];
    for sample in samples {
        let (bytes, _, unmappable) = WINDOWS_1256.encode(sample);
        assert!(!unmappable);
        let e = ok(Kind::Text, &bytes);
        assert_eq!(e.text, sample, "detected {:?}", e.encoding);
        assert_eq!(e.encoding, Some("windows-1256"));
    }
}

#[test]
fn text_arabic_iso_8859_6() {
    // ISO-8859-6 has no Persian letters or tashkeel beyond the basic set.
    let sample = "قال المدير إن الاجتماع سيعقد يوم الأحد في مقر الشركة الرئيسي";
    let (bytes, _, unmappable) = ISO_8859_6.encode(sample);
    assert!(!unmappable);
    let e = ok(Kind::Text, &bytes);
    assert_eq!(e.text, sample, "detected {:?}", e.encoding);
}

#[test]
fn text_arabic_cp1256_mixed_with_ascii_code() {
    let sample =
        "// تعليق: حساب المجموع\nfn sum(a: i32, b: i32) -> i32 { a + b } // نهاية الدالة\n";
    let (bytes, _, _) = WINDOWS_1256.encode(sample);
    assert_eq!(ok(Kind::Text, &bytes).text, sample);
}

#[test]
fn text_binary_is_unsupported() {
    let mut bin = b"MZ\x90\x00\x03\x00\x00\x00".to_vec();
    bin.extend((0..2000u32).map(|i| (i * 7 % 256) as u8));
    assert!(matches!(
        extract_bytes(Kind::Text, &bin, &Limits::default()),
        Err(ExtractError::Unsupported(_))
    ));
}

#[test]
fn text_control_chars_dropped_tabs_kept() {
    let e = ok(
        Kind::Text,
        b"a\tb\x07c\rd\x0Ce and some more ordinary text here",
    );
    assert_eq!(e.text, "a\tbc\nd\ne and some more ordinary text here");
}

#[test]
fn text_output_truncation() {
    let limits = Limits {
        max_output_chars: 5,
        ..Limits::default()
    };
    let e = extract_bytes(Kind::Text, AR2.as_bytes(), &limits).unwrap();
    assert_eq!(e.text.chars().count(), 5);
    assert!(e.truncated);
    assert!(AR2.starts_with(&e.text));
}

#[test]
fn text_input_cap_cuts_cleanly_inside_a_character() {
    let limits = Limits {
        max_input_bytes: 5, // cuts the third Arabic letter in half
        ..Limits::default()
    };
    let e = extract_bytes(Kind::Text, AR2.as_bytes(), &limits).unwrap();
    assert_eq!(e.text, "مر");
    assert!(e.truncated);
}

// ---------------------------------------------------------------- html

#[test]
fn html_structure_entities_scripts() {
    let src = r#"<!DOCTYPE html><html lang="ar" dir="rtl"><head><meta charset="utf-8">
<title>عنوان &amp; صفحة</title><meta name="author" content="كاتب">
<style>body { color: red }</style><script>var x = "<p>not text</p>";</script></head>
<body><h1>مرحبا</h1><p>نص   عربي<br>سطر&nbsp;ثان &lt;b&gt; &#1575;&#x644; &unknown; R&amp;D</p>
<!-- comment <p>hidden</p> --><ul><li>one</li><li>two</li></ul>
<table><tr><td>a</td><td>b</td></tr></table><pre>x  y
z</pre><img alt="صورة"> 1 < 2</body></html>"#;
    let e = ok(Kind::Html, src.as_bytes());
    assert_eq!(
        e.text,
        "مرحبا\nنص عربي\nسطر ثان <b> ال &unknown; R&D\none\ntwo\na b\nx y\nz\nصورة 1 < 2"
    );
    assert_eq!(e.metadata.title.as_deref(), Some("عنوان & صفحة"));
    assert_eq!(e.metadata.author.as_deref(), Some("كاتب"));
}

#[test]
fn html_meta_charset_windows_1256() {
    let doc = format!(
        r#"<html><head><meta http-equiv="Content-Type" content="text/html; charset=windows-1256"></head><body><p>{AR2}</p></body></html>"#
    );
    let (bytes, _, _) = WINDOWS_1256.encode(&doc);
    let e = ok(Kind::Html, &bytes);
    assert_eq!(e.text, AR2);
    assert_eq!(e.encoding, Some("windows-1256"));
}

#[test]
fn html_undeclared_cp1256_is_detected() {
    let doc = format!(
        "<html><body><p>{AR2}</p><p>قال المدير إن الاجتماع سيعقد يوم الأحد</p></body></html>"
    );
    let (bytes, _, _) = WINDOWS_1256.encode(&doc);
    let e = ok(Kind::Html, &bytes);
    assert!(e.text.starts_with(AR2), "{}", e.text);
}

// ---------------------------------------------------------------- docx

#[test]
fn docx_runs_paragraphs_metadata() {
    let bytes = docx(&[&format!("{AR}|، "), "مرح|با", EN], false);
    let e = ok(Kind::Docx, &bytes);
    assert_eq!(e.text, format!("{AR}،\nمرحبا\n{EN}"));
    assert_eq!(e.metadata.title.as_deref(), Some("تقرير"));
    assert_eq!(e.metadata.author.as_deref(), Some("سامي"));
    assert_eq!(e.metadata.created.as_deref(), Some("2025-03-01T10:00:00Z"));
}

#[test]
fn docx_skips_deleted_text_and_field_codes() {
    let body = r#"<w:p><w:r><w:t>keep</w:t></w:r><w:del><w:r><w:delText>gone</w:delText></w:r></w:del><w:r><w:instrText> PAGE </w:instrText></w:r><w:r><w:tab/><w:t>tab</w:t><w:br/><w:t>line</w:t></w:r></w:p><w:tbl><w:tr><w:tc><w:p><w:r><w:t>c1</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>c2</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#;
    let e = ok(Kind::Docx, &docx_raw(body, false));
    assert_eq!(e.text, "keep tab\nline\nc1 c2");
}

#[test]
fn docx_truncates_at_output_limit() {
    let paras: Vec<String> = (0..100).map(|i| format!("فقرة رقم {i}")).collect();
    let refs: Vec<&str> = paras.iter().map(String::as_str).collect();
    let limits = Limits {
        max_output_chars: 50,
        ..Limits::default()
    };
    let e = extract_bytes(Kind::Docx, &docx(&refs, false), &limits).unwrap();
    assert!(e.truncated);
    assert!(e.text.chars().count() <= 50);
    assert!(e.text.starts_with("فقرة رقم 0\nفقرة رقم 1"));
}

#[test]
fn ooxml_password_protected_is_encrypted() {
    let mut cfb = b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1".to_vec();
    cfb.extend(std::iter::repeat_n(0u8, 100));
    cfb.extend("EncryptionInfo".encode_utf16().flat_map(u16::to_le_bytes));
    assert!(matches!(
        extract_bytes(Kind::Docx, &cfb, &Limits::default()),
        Err(ExtractError::Encrypted)
    ));
}

#[test]
fn zip_entry_with_encryption_flag_is_encrypted() {
    let mut bytes = docx(&[EN], true);
    // Set bit 0 (encrypted) of the general-purpose flags in every local
    // (offset 6) and central (offset 8) header.
    let mut i = 0;
    while i + 4 <= bytes.len() {
        match &bytes[i..i + 4] {
            b"PK\x03\x04" => bytes[i + 6] |= 1,
            b"PK\x01\x02" => bytes[i + 8] |= 1,
            _ => {}
        }
        i += 1;
    }
    let r = extract_bytes(Kind::Docx, &bytes, &Limits::default());
    assert!(matches!(r, Err(ExtractError::Encrypted)), "{r:?}");
}

#[test]
fn not_a_zip_is_corrupt() {
    assert!(matches!(
        extract_bytes(Kind::Docx, b"hello", &Limits::default()),
        Err(ExtractError::Corrupt(_))
    ));
}

// ---------------------------------------------------------------- limits

#[test]
fn zip_bomb_entry_is_rejected() {
    // 64 MiB of spaces deflates to ~64 KiB.
    let big = vec![b' '; 64 * 1024 * 1024];
    let mut doc = String::from(r#"<w:document xmlns:w="w"><w:body><w:p><w:r><w:t>"#);
    doc.push_str(std::str::from_utf8(&big).unwrap());
    doc.push_str("</w:t></w:r></w:p></w:body></w:document>");
    let bytes = zip_with(
        &[
            ("_rels/.rels", b"<Relationships/>".to_vec()),
            ("word/document.xml", doc.into_bytes()),
        ],
        false,
    );
    assert!(bytes.len() < 1024 * 1024, "{}", bytes.len());
    let limits = Limits {
        max_entry_bytes: 8 * 1024 * 1024,
        ..Limits::default()
    };
    let r = extract_bytes(Kind::Docx, &bytes, &limits);
    assert!(matches!(r, Err(ExtractError::TooLarge(_))), "{r:?}");

    let limits = Limits {
        max_total_bytes: 1024 * 1024,
        ..Limits::default()
    };
    let r = extract_bytes(Kind::Docx, &bytes, &limits);
    assert!(matches!(r, Err(ExtractError::TooLarge(_))), "{r:?}");
}

#[test]
fn too_many_zip_entries() {
    let entries: Vec<(String, Vec<u8>)> = (0..50).map(|i| (format!("f{i}"), Vec::new())).collect();
    let refs: Vec<(&str, Vec<u8>)> = entries
        .iter()
        .map(|(n, d)| (n.as_str(), d.clone()))
        .collect();
    let limits = Limits {
        max_entries: 10,
        ..Limits::default()
    };
    let r = extract_bytes(Kind::Docx, &zip_with(&refs, true), &limits);
    assert!(matches!(r, Err(ExtractError::TooLarge(_))), "{r:?}");
}

#[test]
fn non_text_input_over_limit_is_too_large() {
    let limits = Limits {
        max_input_bytes: 10,
        ..Limits::default()
    };
    let r = extract_bytes(Kind::Html, b"<p>more than ten bytes</p>", &limits);
    assert!(matches!(r, Err(ExtractError::TooLarge(_))));
}

// ---------------------------------------------------------------- xlsx / pptx

#[test]
fn xlsx_sheets_shared_inline_numbers() {
    let bytes = xlsx(&[
        (
            "المبيعات",
            vec![
                vec![Cell::Shared("المنتج"), Cell::Shared("الكمية")],
                vec![Cell::Shared("قلم"), Cell::Number("12.5")],
                vec![Cell::Inline("دفتر"), Cell::Number("3")],
            ],
        ),
        ("Sheet2", vec![vec![Cell::Shared("قلم"), Cell::Inline(EN)]]),
    ]);
    let e = ok(Kind::Xlsx, &bytes);
    assert_eq!(
        e.text,
        "المبيعات\nالمنتج الكمية\nقلم 12.5\nدفتر 3\nSheet2\nقلم Hello world"
    );
    assert!(!e.text.contains("PHONETIC"));
    assert!(!e.text.contains("SUM"));
    assert_eq!(e.metadata.title.as_deref(), Some("جدول"));
}

#[test]
fn pptx_slides_in_presentation_order() {
    let bytes = pptx(&[&["الشريحة الأولى", "نقطة"], &["Second slide"], &[AR2]]);
    let e = ok(Kind::Pptx, &bytes);
    assert_eq!(e.text, format!("الشريحة الأولى\nنقطة\nSecond slide\n{AR2}"));
    assert_eq!(e.metadata.author.as_deref(), Some("Bob"));
}

// ---------------------------------------------------------------- odf

#[test]
fn odt_paragraphs_spaces_and_meta() {
    let body = format!(
        r#"<office:text><text:sequence-decls><text:sequence-decl text:name="Figure"/></text:sequence-decls><text:h text:outline-level="1">{AR2}</text:h><text:p>نص<text:s text:c="3"/>عربي<text:tab/>{EN}<text:line-break/>سطر</text:p><text:tracked-changes><text:changed-region><text:deletion><text:p>محذوف</text:p></text:deletion></text:changed-region></text:tracked-changes><text:list><text:list-item><text:p>بند</text:p></text:list-item></text:list></office:text>"#
    );
    let bytes = odf("application/vnd.oasis.opendocument.text", &body, &[]);
    let e = ok(Kind::Odt, &bytes);
    assert_eq!(e.text, format!("{AR2}\nنص عربي {EN}\nسطر\nبند"));
    assert_eq!(e.metadata.title.as_deref(), Some("وثيقة"));
    assert_eq!(e.metadata.author.as_deref(), Some("ليلى"));
    assert_eq!(e.metadata.created.as_deref(), Some("2024-05-06T07:08:09"));
    assert_eq!(sniff(&bytes), Some(Kind::Odt));
}

#[test]
fn ods_sheet_names_and_cells() {
    let body = r#"<office:spreadsheet><table:table table:name="ورقة1"><table:table-row><table:table-cell><text:p>اسم</text:p></table:table-cell><table:table-cell office:value-type="float"><text:p>42</text:p></table:table-cell></table:table-row><table:table-row table:number-rows-repeated="1048575"><table:table-cell table:number-columns-repeated="1024"/></table:table-row></table:table></office:spreadsheet>"#;
    let e = ok(
        Kind::Ods,
        &odf("application/vnd.oasis.opendocument.spreadsheet", body, &[]),
    );
    assert_eq!(e.text, "ورقة1\nاسم 42");
}

#[test]
fn odp_pages() {
    let body = format!(
        r#"<office:presentation><draw:page draw:name="p1"><draw:frame><draw:text-box><text:p>{AR2}</text:p></draw:text-box></draw:frame></draw:page><draw:page draw:name="p2"><draw:frame><draw:text-box><text:p>{EN}</text:p></draw:text-box></draw:frame></draw:page></office:presentation>"#
    );
    let e = ok(
        Kind::Odp,
        &odf(
            "application/vnd.oasis.opendocument.presentation",
            &body,
            &[],
        ),
    );
    assert_eq!(e.text, format!("{AR2}\n{EN}"));
}

#[test]
fn odf_encrypted_manifest() {
    let manifest = br#"<manifest:manifest><manifest:file-entry manifest:full-path="content.xml"><manifest:encryption-data/></manifest:file-entry></manifest:manifest>"#;
    let entries = [
        (
            "mimetype",
            b"application/vnd.oasis.opendocument.text".to_vec(),
        ),
        ("META-INF/manifest.xml", manifest.to_vec()),
        ("content.xml", vec![0x8F; 64]),
    ];
    let r = extract_bytes(Kind::Odt, &zip_with(&entries, false), &Limits::default());
    assert!(matches!(r, Err(ExtractError::Encrypted)), "{r:?}");
}

// ---------------------------------------------------------------- epub

#[test]
fn epub_spine_order_and_metadata() {
    let bytes = epub(&[
        ("الفصل الأول", &[AR, "&"]),
        ("Chapter 2", &[EN]),
        ("الفصل الثالث", &[AR2]),
    ]);
    let e = ok(Kind::Epub, &bytes);
    assert_eq!(
        e.text,
        format!("الفصل الأول\n{AR}\n&\nChapter 2\n{EN}\nالفصل الثالث\n{AR2}")
    );
    assert_eq!(e.metadata.title.as_deref(), Some("كتاب الاختبار"));
    assert_eq!(e.metadata.author.as_deref(), Some("المؤلف"));
    assert_eq!(e.metadata.created.as_deref(), Some("2020-01-01"));
    assert_eq!(sniff(&bytes), Some(Kind::Epub));
}

// ---------------------------------------------------------------- eml

#[test]
fn eml_headers_rfc2047_arabic_and_qp_body() {
    // "مرحبا بالعالم" in UTF-8, base64, split mid-character over two words.
    let subject = AR2.as_bytes();
    let (a, b) = subject.split_at(5);
    let mail = format!(
        "From: =?UTF-8?B?{}?= <sami@example.com>\r\nTo: \"Ann\" <ann@example.com>\r\nSubject: =?utf-8?B?{}?=\r\n =?utf-8?B?{}?= (Re)\r\nDate: Mon, 1 Jan 2024 10:00:00 +0300\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=\"utf-8\"\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n{}=\r\n{}\r\nسطر\r\n",
        b64("سامي".as_bytes()),
        b64(a),
        b64(b),
        "=D9=86=D8=B5 =D8=B7=D9=88=D9=8A=D9=84 ",
        "joined"
    );
    let e = ok(Kind::Eml, mail.as_bytes());
    assert_eq!(
        e.metadata.title.as_deref(),
        Some(format!("{AR2} (Re)").as_str())
    );
    assert_eq!(
        e.metadata.author.as_deref(),
        Some("سامي <sami@example.com>")
    );
    assert_eq!(
        e.metadata.created.as_deref(),
        Some("Mon, 1 Jan 2024 10:00:00 +0300")
    );
    assert_eq!(
        e.text,
        format!(
            "{AR2} (Re)\nسامي <sami@example.com>\n\"Ann\" <ann@example.com>\nنص طويل joined\nسطر"
        )
    );
}

#[test]
fn eml_multipart_alternative_prefers_plain_and_skips_attachments() {
    let (cp, _, _) = WINDOWS_1256.encode("نص عادي بترميز ويندوز");
    let mail = format!(
        "Subject: =?windows-1256?Q?{}?=\nContent-Type: multipart/mixed; boundary=\"outer\"\n\npreamble\n--outer\nContent-Type: multipart/alternative; boundary=inner\n\n--inner\nContent-Type: text/plain; charset=windows-1256\nContent-Transfer-Encoding: base64\n\n{}\n--inner\nContent-Type: text/html; charset=utf-8\n\n<p>HTML version</p>\n--inner--\n--outer\nContent-Type: text/plain\nContent-Disposition: attachment; filename=\"a.txt\"\n\nATTACHED\n--outer\nContent-Type: message/rfc822\n\nSubject: forwarded\nContent-Type: text/html\n\n<p>fw &amp; body</p>\n--outer--\nepilogue\n",
        WINDOWS_1256
            .encode("عنوان")
            .0
            .iter()
            .map(|b| format!("={b:02X}"))
            .collect::<String>(),
        b64(&cp)
    );
    let e = ok(Kind::Eml, mail.as_bytes());
    assert_eq!(e.text, "عنوان\nنص عادي بترميز ويندوز\nforwarded\nfw & body");
}

#[test]
fn eml_html_only_and_8bit_cp1256_header() {
    let mut mail = b"Subject: ".to_vec();
    mail.extend_from_slice(
        &WINDOWS_1256
            .encode("قال المدير إن الاجتماع سيعقد يوم الأحد")
            .0,
    );
    mail.extend_from_slice(
        b"\nContent-Type: text/html; charset=utf-8\nContent-Transfer-Encoding: base64\n\n",
    );
    mail.extend_from_slice(b64(format!("<div>{AR}</div><div>{EN}</div>").as_bytes()).as_bytes());
    let e = ok(Kind::Eml, &mail);
    assert_eq!(
        e.text,
        format!("قال المدير إن الاجتماع سيعقد يوم الأحد\n{AR}\n{EN}")
    );
}

#[test]
fn eml_without_headers_is_corrupt() {
    assert!(matches!(
        extract_bytes(Kind::Eml, b"\n\nbody only", &Limits::default()),
        Err(ExtractError::Corrupt(_))
    ));
}

// ---------------------------------------------------------------- detection

#[test]
fn kind_from_path_and_sniff() {
    use std::path::Path;
    assert_eq!(Kind::from_path(Path::new("a/b/README")), Some(Kind::Text));
    assert_eq!(Kind::from_path(Path::new("x.RS")), Some(Kind::Text));
    assert_eq!(Kind::from_path(Path::new("x.Docx")), Some(Kind::Docx));
    assert_eq!(Kind::from_path(Path::new("x.unknown")), None);
    assert_eq!(sniff(b"%PDF-1.7\n"), Some(Kind::Pdf));
    assert_eq!(sniff(b"\xEF\xBB\xBF  <!DOCTYPE HTML>"), Some(Kind::Html));
    assert_eq!(sniff(&docx(&[EN], false)), Some(Kind::Docx));
    assert_eq!(sniff(b"just text"), None);
}

#[test]
fn pdf_without_feature_is_unsupported() {
    if !cfg!(feature = "pdf") {
        assert!(matches!(
            extract_bytes(Kind::Pdf, b"%PDF-1.4", &Limits::default()),
            Err(ExtractError::Unsupported(_))
        ));
    }
}

#[test]
fn extract_from_files() {
    let dir = std::env::temp_dir().join(format!("athrix-extract-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let txt = dir.join("note.txt");
    std::fs::write(&txt, WINDOWS_1256.encode("ملاحظة مهمة جدا عن المشروع").0).unwrap();
    assert_eq!(
        extract(&txt, &Limits::default()).unwrap().text,
        "ملاحظة مهمة جدا عن المشروع"
    );

    // Unknown extension: sniffed.
    let odd = dir.join("document.bin");
    std::fs::write(&odd, docx(&[AR2], false)).unwrap();
    let e = extract(&odd, &Limits::default()).unwrap();
    assert_eq!((e.kind, e.text.as_str()), (Kind::Docx, AR2));

    // Text over the input limit: read partially.
    let limits = Limits {
        max_input_bytes: 4,
        ..Limits::default()
    };
    let e = extract(&txt, &limits).unwrap();
    assert!(e.truncated);

    // Other formats over the input limit: rejected.
    let r = extract(dir.join("document.bin"), &limits);
    assert!(matches!(
        r,
        Err(ExtractError::Unsupported(_)) | Err(ExtractError::TooLarge(_))
    ));

    let missing = extract(dir.join("missing.txt"), &Limits::default());
    assert!(matches!(missing, Err(ExtractError::Io(_))));
    std::fs::remove_dir_all(&dir).ok();
}

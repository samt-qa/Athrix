//! Real-world queries as people type them in the search bar (Arabic, English, mixed), with
//! the plan they must give. Table-driven so a new report becomes one line.

use athrix_query::{Expr, ExprKind, PlanOptions, SearchPlan, parse, plan};

fn sexp(e: &Expr) -> String {
    let list = |op: &str, items: &[Expr]| {
        let inner: Vec<String> = items.iter().map(sexp).collect();
        format!("({op} {})", inner.join(" "))
    };
    match &e.kind {
        ExprKind::Term(t) if t.quoted => format!("{:?}", t.text),
        ExprKind::Term(t) => t.text.clone(),
        ExprKind::Filter(f) => format!("{}:{:?}", f.name, f.value),
        ExprKind::Not(x) => format!("(not {})", sexp(x)),
        ExprKind::And(v) => list("and", v),
        ExprKind::Or(v) => list("or", v),
        ExprKind::Group(x) => format!("<{}>", sexp(x)),
    }
}

/// `(query, expected parse tree)`.
const PARSES: &[(&str, &str)] = &[
    ("تقرير", "تقرير"),
    ("فاتورة ٢٠٢٦", "(and فاتورة ٢٠٢٦)"),
    ("\"عقد الإيجار\"", "\"عقد الإيجار\""),
    ("محضر اجتماع -مسودة", "(and محضر اجتماع (not مسودة))"),
    ("تقرير | report", "(or تقرير report)"),
    (
        "امتداد:pdf؛docx فاتورة",
        "(and ext:Extensions([\"pdf\", \"docx\"]) فاتورة)",
    ),
    ("إمتداد:PDF", "ext:Extensions([\"pdf\"])"),
    ("نوع:صور", "type:Kinds([Image])"),
    ("نوع:مستندات;فيديو", "type:Kinds([Document, Video])"),
    ("حجم:>١٠ميجا", "size:Size(Gt(10485760))"),
    ("حجم:١٫٥جيجا", "size:Size(Eq(1610612736))"),
    ("تاريخ:اليوم", "dm:Date(Eq(Named(Today)))"),
    ("تاريخ:أمس", "dm:Date(Eq(Named(Yesterday)))"),
    ("تاريخ:هذا_الأسبوع", "dm:Date(Eq(Named(ThisWeek)))"),
    (
        "تاريخ:٢٠٢٦-٠٣",
        "dm:Date(Eq(Calendar(CalendarDate { year: 2026, month: Some(3), day: None })))",
    ),
    ("مسار:\"المستندات/العمل\"", "path:Text(\"المستندات/العمل\")"),
    ("مجلد:العقود", "infolder:Text(\"العقود\")"),
    ("محتوى:الإيجار", "content:Text(\"الإيجار\")"),
    ("مُحْتَوَى:الإيجار", "content:Text(\"الإيجار\")"),
    ("محتوي:الإيجار", "content:Text(\"الإيجار\")"),
    ("وسم:مهم", "tag:Tags([\"مهم\"])"),
    ("fuzzy:فاتوره", "fuzzy:Text(\"فاتوره\")"),
    (r"C:\المستندات\تقرير", r"C:\المستندات\تقرير"),
    ("تقرير_2026.pdf", "تقرير_2026.pdf"),
    (
        "<تقرير | فاتورة> امتداد:pdf",
        "(and <(or تقرير فاتورة)> ext:Extensions([\"pdf\"]))",
    ),
];

#[test]
fn realistic_queries_parse() {
    let mut failures = Vec::new();
    for &(q, want) in PARSES {
        match parse(q) {
            Ok(ast) => {
                let got = ast.root.as_ref().map(sexp).unwrap_or_default();
                if got != want {
                    failures.push(format!("{q:?}\n   want {want}\n    got {got}"));
                }
            }
            Err(e) => failures.push(format!("{q:?}: {e}")),
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

/// `(query, plan kind)` with plain words searching names only.
const PLANS: &[(&str, &str)] = &[
    ("عقد الإيجار", "name"),
    ("محتوى:الإيجار امتداد:docx", "name+content"),
    ("عقد محتوى:\"الطرف الأول\"", "name+content"),
    ("امتداد:pdf -محتوى:مسودة", "name+content"),
    ("محتوى:عقد | فاتورة", "error ContentInOr"),
    ("-<محتوى:عقد فاتورة>", "error NegatedContentGroup"),
    ("-محتوى:مسودة", "error OnlyExcludedContent"),
];

#[test]
fn realistic_queries_plan() {
    let opts = PlanOptions::default();
    for &(q, want) in PLANS {
        let got = match plan(&parse(q).unwrap(), &opts) {
            Ok(SearchPlan::Name(_)) => "name".to_string(),
            Ok(SearchPlan::NameAndContent { .. }) => "name+content".to_string(),
            Ok(p) => format!("{p:?}"),
            Err(e) => format!("error {:?}", e.kind),
        };
        assert_eq!(got, want, "{q}");
    }
}

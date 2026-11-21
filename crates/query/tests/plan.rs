//! Planner tests: splitting queries between the name index and the content index.

use athrix_query::{
    Expr, ExprKind, PlanErrorKind, PlanOptions, Query, SearchPlan, Span, parse, plan,
};

const NAME_ONLY: PlanOptions = PlanOptions {
    plain_words_search_content: false,
};
const BOTH: PlanOptions = PlanOptions {
    plain_words_search_content: true,
};

fn sexp(e: &Expr) -> String {
    let list = |op: &str, items: &[Expr]| {
        let inner: Vec<String> = items.iter().map(sexp).collect();
        format!("({op} {})", inner.join(" "))
    };
    match &e.kind {
        ExprKind::Term(t) if t.quoted => format!("\"{}\"", t.text),
        ExprKind::Term(t) => t.text.clone(),
        ExprKind::Filter(f) => format!("{}:{:?}", f.name, f.value),
        ExprKind::Not(x) => format!("(not {})", sexp(x)),
        ExprKind::And(v) => list("and", v),
        ExprKind::Or(v) => list("or", v),
        ExprKind::Group(x) => format!("<{}>", sexp(x)),
    }
}

fn q_sexp(q: &Option<Query>) -> Option<String> {
    q.as_ref().and_then(|q| q.root.as_ref()).map(sexp)
}

fn planned(q: &str, opts: PlanOptions) -> SearchPlan {
    let ast = parse(q).unwrap_or_else(|e| panic!("`{q}`: {e}"));
    plan(&ast, &opts).unwrap_or_else(|e| panic!("`{q}`: {e}"))
}

/// (name part, content text) of a NameAndContent plan.
fn and_parts(q: &str) -> (Option<String>, String) {
    match planned(q, NAME_ONLY) {
        SearchPlan::NameAndContent { name, content } => (q_sexp(&name), content.to_query_text()),
        other => panic!("`{q}`: expected NameAndContent, got {other:?}"),
    }
}

fn plan_err(q: &str) -> (PlanErrorKind, String, String) {
    let ast = parse(q).unwrap();
    match plan(&ast, &NAME_ONLY) {
        Ok(p) => panic!("`{q}` should be rejected, got {p:?}"),
        Err(e) => (
            e.kind,
            e.span.slice(q).to_string(),
            e.group_span.slice(q).to_string(),
        ),
    }
}

#[test]
fn empty_and_name_only() {
    assert_eq!(planned("", NAME_ONLY), SearchPlan::Empty);
    assert_eq!(planned("   ", BOTH), SearchPlan::Empty);
    let ast = parse("تقرير ext:pdf | x").unwrap();
    assert_eq!(
        plan(&ast, &NAME_ONLY).unwrap(),
        SearchPlan::Name(ast.clone())
    );
    // `sem:` etc. are left to the name index (which reports them as unsupported).
    assert!(matches!(planned("~عقد", NAME_ONLY), SearchPlan::Name(_)));
}

#[test]
fn name_and_content_arabic() {
    assert_eq!(
        and_parts("تقرير content:ميزانية"),
        (Some("تقرير".into()), r#""ميزانية""#.into())
    );
    assert_eq!(
        and_parts("تقرير محتوى:ميزانية"),
        (Some("تقرير".into()), r#""ميزانية""#.into())
    );
    // Hamza / alef-maqsura tolerant filter names: محتوي, مُحتوى.
    assert_eq!(and_parts("محتوي:عقد").1, r#""عقد""#);
    assert_eq!(and_parts("مُحتوى:عقد").1, r#""عقد""#);
    assert_eq!(
        and_parts(r#"content:"خطة العمل" ext:docx"#),
        (
            Some(r#"ext:Extensions(["docx"])"#.into()),
            r#""خطة العمل""#.into()
        )
    );
    assert_eq!(
        and_parts(r#"امتداد:docx محتوى:"خطة العمل" محتوى:إيجار"#),
        (
            Some(r#"ext:Extensions(["docx"])"#.into()),
            r#""خطة العمل" "إيجار""#.into()
        )
    );
}

#[test]
fn name_and_content_english() {
    assert_eq!(
        and_parts("report content:budget size:>1mb"),
        (
            Some("(and report size:Size(Gt(1048576)))".into()),
            r#""budget""#.into()
        )
    );
    assert_eq!(and_parts("content:budget"), (None, r#""budget""#.into()));
    assert_eq!(
        and_parts("CONTENT:budget Content:plan"),
        (None, r#""budget" "plan""#.into())
    );
}

#[test]
fn groups_are_flattened_and_or_is_kept_on_the_name_side() {
    assert_eq!(
        and_parts("<report content:x> ext:pdf"),
        (
            Some(r#"(and report ext:Extensions(["pdf"]))"#.into()),
            r#""x""#.into()
        )
    );
    // An OR without content stays whole on the name side.
    assert_eq!(
        and_parts("<a | b> content:x"),
        (Some("(or a b)".into()), r#""x""#.into())
    );
    assert_eq!(and_parts("!!content:x"), (None, r#""x""#.into()));
    assert_eq!(
        and_parts("!!<a content:x>"),
        (Some("a".into()), r#""x""#.into())
    );
}

#[test]
fn negated_content() {
    assert_eq!(
        and_parts("report -content:draft"),
        (Some("report".into()), r#"-"draft""#.into())
    );
    assert_eq!(
        and_parts("تقرير !محتوى:مسودة content:ميزانية"),
        (Some("تقرير".into()), r#"-"مسودة" "ميزانية""#.into())
    );
    assert_eq!(
        and_parts(r#"ext:docx -<content:"نسخة قديمة">"#),
        (
            Some(r#"ext:Extensions(["docx"])"#.into()),
            r#"-"نسخة قديمة""#.into()
        )
    );
    let SearchPlan::NameAndContent { content, .. } = planned("x -content:a", NAME_ONLY) else {
        panic!()
    };
    assert!(!content.has_positive());
    assert_eq!(content.excluded().count(), 1);
    assert_eq!(content.clauses[0].to_positive_text(), r#""a""#);
}

#[test]
fn rejected_combinations() {
    assert_eq!(
        plan_err("a | content:b"),
        (
            PlanErrorKind::ContentInOr,
            "content:b".into(),
            "a | content:b".into()
        )
    );
    assert_eq!(
        plan_err("x <تقرير | محتوى:ميزانية>").0,
        PlanErrorKind::ContentInOr
    );
    assert_eq!(
        plan_err("content:a | content:b").0,
        PlanErrorKind::ContentInOr
    );
    assert_eq!(plan_err("x !<a | content:b>").0, PlanErrorKind::ContentInOr);
    assert_eq!(
        plan_err("!<content:a ext:pdf>"),
        (
            PlanErrorKind::NegatedContentGroup,
            "content:a".into(),
            "!<content:a ext:pdf>".into()
        )
    );
    assert_eq!(
        plan_err("x -<content:a content:b>").0,
        PlanErrorKind::NegatedContentGroup
    );
    assert_eq!(
        plan_err("-content:draft"),
        (
            PlanErrorKind::OnlyExcludedContent,
            "-content:draft".into(),
            "-content:draft".into()
        )
    );
    assert_eq!(
        plan_err("!محتوى:مسودة").0,
        PlanErrorKind::OnlyExcludedContent
    );
    // Errors convert to the app-wide error type.
    let e = plan(&parse("a | content:b").unwrap(), &NAME_ONLY).unwrap_err();
    let core: athrix_core::Error = e.into();
    assert!(core.to_string().contains("position 4"), "{core}");
}

#[test]
fn spans_point_into_the_query() {
    let q = "تقرير محتوى:\"خطة العمل\"";
    let SearchPlan::NameAndContent { content, .. } = planned(q, NAME_ONLY) else {
        panic!()
    };
    assert_eq!(content.clauses[0].span.slice(q), "محتوى:\"خطة العمل\"");
    assert_eq!(content.clauses[0].span, Span::new(11, q.len()));
}

#[test]
fn plain_words_mode() {
    let SearchPlan::NameOrContent {
        name,
        filters,
        content,
    } = planned("تقرير ext:pdf", BOTH)
    else {
        panic!()
    };
    assert_eq!(
        q_sexp(&Some(name)).unwrap(),
        r#"(and تقرير ext:Extensions(["pdf"]))"#
    );
    assert_eq!(q_sexp(&filters).unwrap(), r#"ext:Extensions(["pdf"])"#);
    assert_eq!(content.to_query_text(), r#""تقرير""#);

    let SearchPlan::NameOrContent {
        filters, content, ..
    } = planned(r#"report "annual plan" -draft"#, BOTH)
    else {
        panic!()
    };
    assert_eq!(filters, None);
    assert_eq!(
        content.to_query_text(),
        r#""report" "annual plan" -"draft""#
    );

    // Does not qualify: falls back to name only.
    for q in [
        "rep*",
        "^report",
        "a | b",
        "ext:pdf",
        "-draft",
        "<a | b> c",
        "ext:pdf -<a | b> c",
    ] {
        assert!(
            matches!(planned(q, BOTH), SearchPlan::Name(_)),
            "`{q}` should stay name-only"
        );
    }
    // Explicit content: wins over the mode.
    assert!(matches!(
        planned("تقرير محتوى:ميزانية", BOTH),
        SearchPlan::NameAndContent { .. }
    ));
    // Off by default.
    assert!(matches!(
        planned("report", PlanOptions::default()),
        SearchPlan::Name(_)
    ));
}

#[test]
fn name_words_for_boosts() {
    assert_eq!(planned("تقرير ext:pdf", NAME_ONLY).name_words(), ["تقرير"]);
    assert_eq!(
        planned(r#"report "q1" content:x"#, NAME_ONLY).name_words(),
        ["report", "q1"]
    );
    assert!(planned("content:x", NAME_ONLY).name_words().is_empty());
    assert!(planned("", NAME_ONLY).name_words().is_empty());
    assert!(planned("report content:x", NAME_ONLY).uses_content());
    assert!(!planned("report", NAME_ONLY).uses_content());
}

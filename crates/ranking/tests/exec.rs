//! Executor tests with a small in-memory "name index" and "content index".

use std::cell::Cell;

use athrix_query::{Expr, ExprKind, FilterName, FilterValue, PlanOptions, Query, parse, plan};
use athrix_ranking::{
    Boosts, ContentHit, ExecOptions, FrecencyConfig, FrecencyStore, Matched, MemoryFrecencyStore,
    NameHit, NameResults, SearchResults, Sources, execute,
};
use athrix_text_ar::normalize_name;

const NOW: i64 = 1_800_000_000;

struct File {
    path: String,
    content: String,
    mtime: i64,
}

fn file(path: &str, content: &str) -> File {
    File {
        path: path.into(),
        content: content.into(),
        mtime: NOW - 400 * 86_400,
    }
}

/// Fake indexes. Names: substring on the normalized file name, `ext:`, AND/OR/NOT.
/// Content: `"clause"`s ANDed, `-` excludes; hits ordered by occurrences, then path.
struct Fake {
    files: Vec<File>,
    /// What the content index reports for paths (to test path identity).
    content_path: fn(&str) -> String,
    verifier: bool,
    name_calls: Cell<usize>,
    content_calls: Cell<usize>,
    fail: bool,
}

impl Fake {
    fn new(files: Vec<File>) -> Self {
        Fake {
            files,
            content_path: |p| p.to_string(),
            verifier: false,
            name_calls: Cell::new(0),
            content_calls: Cell::new(0),
            fail: false,
        }
    }

    fn name_of(path: &str) -> &str {
        path.rsplit('\\').next().unwrap()
    }

    fn eval(e: &Expr, f: &File) -> bool {
        let name = normalize_name(Self::name_of(&f.path));
        match &e.kind {
            ExprKind::Term(t) => name.contains(&normalize_name(&t.text)),
            ExprKind::Filter(fl) => match (&fl.name, &fl.value) {
                (FilterName::Ext, FilterValue::Extensions(x)) => x
                    .iter()
                    .any(|x| f.path.to_lowercase().ends_with(&format!(".{x}"))),
                other => panic!("fake index: unsupported {other:?}"),
            },
            ExprKind::Not(x) => !Self::eval(x, f),
            ExprKind::Group(x) => Self::eval(x, f),
            ExprKind::And(v) => v.iter().all(|x| Self::eval(x, f)),
            ExprKind::Or(v) => v.iter().any(|x| Self::eval(x, f)),
        }
    }

    fn content_matches(text: &str, f: &File) -> Option<usize> {
        let mut score = 0;
        let mut rest = text.trim();
        while !rest.is_empty() {
            let neg = rest.starts_with('-');
            let body = rest.trim_start_matches('-').strip_prefix('"').unwrap();
            let end = body.find('"').unwrap();
            let clause = &body[..end];
            rest = body[end + 1..].trim_start();
            let n = f.content.matches(clause).count();
            match (neg, n) {
                (true, 0) => {}
                (true, _) | (false, 0) => return None,
                (false, n) => score += n,
            }
        }
        Some(score)
    }
}

#[derive(Debug, PartialEq)]
struct Boom;

impl Sources for Fake {
    type Snippet = String;
    type Error = Boom;

    fn search_names(&mut self, q: &Query, limit: usize) -> Result<NameResults, Boom> {
        self.name_calls.set(self.name_calls.get() + 1);
        if self.fail {
            return Err(Boom);
        }
        let hits = self
            .files
            .iter()
            .filter(|f| q.root.as_ref().is_none_or(|r| Self::eval(r, f)))
            .take(limit)
            .map(|f| NameHit {
                path: f.path.clone(),
                mtime: Some(f.mtime),
            })
            .collect();
        Ok(NameResults {
            hits,
            ranked: false,
        })
    }

    fn search_content(
        &mut self,
        text: &str,
        limit: usize,
    ) -> Result<Vec<ContentHit<String>>, Boom> {
        self.content_calls.set(self.content_calls.get() + 1);
        let mut hits: Vec<(usize, &File)> = self
            .files
            .iter()
            .filter_map(|f| Self::content_matches(text, f).map(|s| (s, f)))
            .collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.path.cmp(&b.1.path)));
        Ok(hits
            .into_iter()
            .take(limit)
            .map(|(s, f)| ContentHit {
                path: (self.content_path)(&f.path),
                score: s as f32,
                mtime: None,
                snippet: format!("…{}…", f.content),
            })
            .collect())
    }

    fn name_matches(&mut self, q: &Query, path: &str) -> Result<Option<bool>, Boom> {
        if !self.verifier {
            return Ok(None);
        }
        let f = self.files.iter().find(|f| f.path == path).unwrap();
        Ok(Some(q.root.as_ref().is_none_or(|r| Self::eval(r, f))))
    }
}

fn opts() -> ExecOptions {
    ExecOptions {
        boosts: Boosts::none(),
        case_insensitive_paths: true,
        ..ExecOptions::new(NOW)
    }
}

fn run(fake: &mut Fake, q: &str, o: &ExecOptions, plain: bool) -> SearchResults<String> {
    let p = plan(
        &parse(q).unwrap(),
        &PlanOptions {
            plain_words_search_content: plain,
        },
    )
    .unwrap();
    execute(&p, o, None, fake).unwrap()
}

fn paths(r: &SearchResults<String>) -> Vec<(&str, Matched)> {
    r.hits
        .iter()
        .map(|h| (Fake::name_of(&h.path), h.matched))
        .collect()
}

fn corpus() -> Vec<File> {
    vec![
        file(r"C:\د\تقرير 2026.docx", "ميزانية ميزانية خطة العمل"),
        file(r"C:\د\تقرير قديم.pdf", "لا شيء هنا"),
        file(r"C:\د\ميزانية.xlsx", "ميزانية"),
        file(r"C:\د\أرشيف التقارير.docx", "خطة العمل مسودة"),
        file(r"C:\e\report draft.txt", "budget draft"),
        file(r"C:\e\report final.txt", "budget"),
        file(r"C:\e\notes.txt", "budget meeting notes"),
    ]
}

#[test]
fn name_and_content_arabic() {
    let mut f = Fake::new(corpus());
    let r = run(&mut f, "تقرير content:ميزانية", &opts(), false);
    assert_eq!(paths(&r), [("تقرير 2026.docx", Matched::Both)]);
    assert_eq!(
        r.hits[0].snippet.as_deref(),
        Some("…ميزانية ميزانية خطة العمل…")
    );
    assert_eq!(r.hits[0].content_score, Some(2.0));
    assert_eq!(r.hits[0].mtime, Some(NOW - 400 * 86_400)); // from the name side
    assert!(r.truncated.is_complete());
    assert_eq!((f.name_calls.get(), f.content_calls.get()), (1, 1));

    let r = run(&mut f, r#"محتوى:"خطة العمل" امتداد:docx"#, &opts(), false);
    assert_eq!(
        paths(&r),
        [
            ("أرشيف التقارير.docx", Matched::Both),
            ("تقرير 2026.docx", Matched::Both)
        ]
    );
}

#[test]
fn content_only_and_english() {
    let mut f = Fake::new(corpus());
    let r = run(&mut f, "content:budget", &opts(), false);
    assert_eq!(
        paths(&r),
        [
            ("notes.txt", Matched::Content),
            ("report draft.txt", Matched::Content),
            ("report final.txt", Matched::Content),
        ]
    );
    assert_eq!(f.name_calls.get(), 0);
    let r = run(
        &mut f,
        "report content:budget -content:draft",
        &opts(),
        false,
    );
    assert_eq!(paths(&r), [("report final.txt", Matched::Both)]);
}

#[test]
fn excluded_content_only() {
    let mut f = Fake::new(corpus());
    let r = run(&mut f, "ext:docx -محتوى:مسودة", &opts(), false);
    assert_eq!(paths(&r), [("تقرير 2026.docx", Matched::Name)]);
    assert!(r.hits[0].snippet.is_none());
    let r = run(&mut f, "report -content:draft", &opts(), false);
    assert_eq!(paths(&r), [("report final.txt", Matched::Name)]);
}

#[test]
fn plain_words_search_both() {
    let mut f = Fake::new(corpus());
    let r = run(&mut f, "ميزانية", &opts(), true);
    // ميزانية.xlsx: name rank 1 + content rank 2 → first; تقرير 2026: content rank 1.
    assert_eq!(
        paths(&r),
        [
            ("ميزانية.xlsx", Matched::Both),
            ("تقرير 2026.docx", Matched::Content)
        ]
    );
    let r = run(&mut f, "report ext:txt", &opts(), true);
    assert_eq!(
        paths(&r),
        [
            ("report draft.txt", Matched::Name),
            ("report final.txt", Matched::Name)
        ]
    );
    // Content branch respects the filters: budget in .txt only.
    let r = run(&mut f, "budget ext:txt", &opts(), true);
    assert_eq!(r.hits.len(), 3);
    assert!(r.hits.iter().all(|h| h.matched == Matched::Content));
    // Filters alone never make a "name" match of the words.
    let r = run(&mut f, "meeting ext:txt", &opts(), true);
    assert_eq!(paths(&r), [("notes.txt", Matched::Content)]);
}

#[test]
fn path_identity_is_case_insensitive() {
    let mut f = Fake::new(corpus());
    f.content_path = |p| p.to_uppercase().replace('\\', "/");
    let r = run(&mut f, "report content:budget", &opts(), false);
    assert_eq!(r.hits.len(), 2);
    let mut exact = opts();
    exact.case_insensitive_paths = false;
    let r = run(&mut f, "report content:budget", &exact, false);
    assert!(r.hits.is_empty());
}

fn many(n: usize) -> Vec<File> {
    // Every file contains «عقد»; the files named «مطلوب» have it the least, so they come last
    // in content order.
    (0..n)
        .map(|i| {
            if i % 100 == 0 {
                file(&format!(r"C:\x\مطلوب {i}.txt"), "عقد")
            } else {
                file(&format!(r"C:\x\other {i:05}.txt"), "عقد عقد")
            }
        })
        .collect()
}

#[test]
fn content_deepening() {
    let mut f = Fake::new(many(5000));
    let o = ExecOptions {
        limit: 10,
        ..opts()
    };
    let r = run(&mut f, "مطلوب content:عقد", &o, false);
    assert_eq!(r.hits.len(), 10);
    assert!(r.truncated.is_complete());
    // Fetches 200 → 800 → 3200 → 10 000 (the last returns all 5000: exhausted).
    assert_eq!(f.content_calls.get(), 4);

    let mut f = Fake::new(many(5000));
    let o = ExecOptions {
        limit: 10,
        content_cap: 1000,
        ..opts()
    };
    let r = run(&mut f, "مطلوب content:عقد", &o, false);
    assert!(r.hits.is_empty());
    assert!(r.truncated.content);
}

#[test]
fn name_cap_overflow_uses_the_verifier() {
    let o = ExecOptions {
        limit: 5,
        name_cap: 3,
        ..opts()
    };
    let mut f = Fake::new(many(1000));
    let r = run(&mut f, "txt content:عقد", &o, false);
    // Without a verifier only the 3 capped name hits can be confirmed.
    assert_eq!(r.hits.len(), 3);
    assert!(r.truncated.names);

    let mut f = Fake::new(many(1000));
    f.verifier = true;
    let r = run(&mut f, "txt content:عقد", &o, false);
    assert_eq!(r.hits.len(), 5);
    assert!(r.truncated.is_complete());
}

#[test]
fn boosts_reorder() {
    let mut f = Fake::new(vec![
        file(r"C:\a\report 2025.txt", "x"),
        file(r"C:\a\report.txt", "x"),
        file(r"C:\a\old report.txt", "x"),
    ]);
    let r = run(&mut f, "report", &opts(), false);
    assert_eq!(Fake::name_of(&r.hits[0].path), "report 2025.txt");
    let o = ExecOptions {
        boosts: Boosts::default(),
        ..opts()
    };
    let r = run(&mut f, "report", &o, false);
    assert_eq!(Fake::name_of(&r.hits[0].path), "report.txt"); // exact name

    let mut store = MemoryFrecencyStore::new(FrecencyConfig {
        case_insensitive_paths: true,
        ..FrecencyConfig::default()
    });
    for d in 0..5 {
        store.record_open(r"c:\A\OLD REPORT.TXT", NOW - d * 3600);
    }
    let o = ExecOptions {
        boosts: Boosts {
            exact_name: 0.0,
            ..Boosts::default()
        },
        ..opts()
    };
    let p = plan(&parse("report").unwrap(), &PlanOptions::default()).unwrap();
    let r = execute(&p, &o, Some(&store as &dyn FrecencyStore), &mut f).unwrap();
    assert_eq!(Fake::name_of(&r.hits[0].path), "old report.txt");
}

#[test]
fn limit_and_errors() {
    let mut f = Fake::new(many(300));
    let o = ExecOptions { limit: 7, ..opts() };
    assert_eq!(run(&mut f, "content:عقد", &o, false).hits.len(), 7);
    assert_eq!(run(&mut f, "txt", &o, false).hits.len(), 7);
    assert!(run(&mut f, "", &o, false).hits.is_empty());
    f.fail = true;
    let p = plan(&parse("x content:y").unwrap(), &PlanOptions::default()).unwrap();
    assert_eq!(execute(&p, &o, None, &mut f), Err(Boom));
}

#[test]
fn closures_adapter() {
    use athrix_ranking::Closures;
    let mut names = |_: &Query, _: usize| -> Result<NameResults, ()> {
        Ok(NameResults {
            hits: vec![
                NameHit {
                    path: "a".into(),
                    mtime: None,
                },
                NameHit {
                    path: "b".into(),
                    mtime: None,
                },
            ],
            ranked: true,
        })
    };
    let mut content = |_: &str, _: usize| -> Result<Vec<ContentHit<&'static str>>, ()> {
        Ok(vec![ContentHit {
            path: "b".into(),
            score: 1.0,
            mtime: None,
            snippet: "snip",
        }])
    };
    let mut src = Closures {
        names: &mut names,
        content: &mut content,
        verify: None,
    };
    let p = plan(&parse("x content:y").unwrap(), &PlanOptions::default()).unwrap();
    let r = execute(&p, &opts(), None, &mut src).unwrap();
    assert_eq!(r.hits.len(), 1);
    assert_eq!(r.hits[0].path, "b");
    assert_eq!(r.hits[0].snippet, Some("snip"));
    assert_eq!(r.hits[0].matched, Matched::Both);
}

#[test]
fn executes_two_thousand_hits_fast() {
    use athrix_ranking::Closures;
    use std::time::{Duration, Instant};
    let path = |i: usize| format!(r"C:\Users\User\Documents\مشاريع\2026\تقرير الميزانية {i}.docx");
    let name_hits: Vec<NameHit> = (0..1000)
        .map(|i| NameHit {
            path: path(i),
            mtime: Some(NOW - i as i64 * 3600),
        })
        .collect();
    let content_hits: Vec<ContentHit<()>> = (500..1500)
        .map(|i| ContentHit {
            path: path(i),
            score: 1.0,
            mtime: None,
            snippet: (),
        })
        .collect();
    let mut names = |_: &Query, _: usize| -> Result<NameResults, ()> {
        Ok(NameResults {
            hits: name_hits.clone(),
            ranked: false,
        })
    };
    let mut content =
        |_: &str, _: usize| -> Result<Vec<ContentHit<()>>, ()> { Ok(content_hits.clone()) };
    let mut src = Closures {
        names: &mut names,
        content: &mut content,
        verify: None,
    };
    let p = plan(
        &parse("تقرير").unwrap(),
        &PlanOptions {
            plain_words_search_content: true,
        },
    )
    .unwrap();
    let o = ExecOptions {
        limit: 1000,
        boosts: Boosts::default(),
        ..ExecOptions::new(NOW)
    };
    let store = MemoryFrecencyStore::new(FrecencyConfig::default());
    let mut best = Duration::MAX;
    for _ in 0..10 {
        let t = Instant::now();
        let r = execute(&p, &o, Some(&store as &dyn FrecencyStore), &mut src).unwrap();
        best = best.min(t.elapsed());
        assert_eq!(r.hits.len(), 1000);
    }
    // Includes cloning the 2000 input hits in the closures.
    eprintln!("execute (union, 2×1000 hits, boosts): {best:?}");
    let bound = if cfg!(debug_assertions) { 100 } else { 5 };
    assert!(best < Duration::from_millis(bound), "{best:?}");
}

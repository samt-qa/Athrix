// Self-checks for the search-bar lexer and autocomplete (no test runner in the desktop app).
// Run: `npm run check:query` (Node >= 22.6, type stripping). Not imported by the app.
import { applyCompletion, complete, type Completion } from "./autocomplete.ts";
import { filterName, kindOf, namedDate, normalizeWord, tokenize, type Token } from "./queryLang.ts";
import { findMatches, queryTerms } from "./textMatch.ts";
import { highlightSyntax } from "./syntax.ts";

declare const process: { exit(code: number): never };

let failures = 0;
function check(name: string, ok: boolean, detail?: unknown) {
  if (ok) return;
  failures++;
  console.error(`FAIL ${name}`, detail ?? "");
}
function eq<T>(name: string, got: T, want: T) {
  const g = JSON.stringify(got);
  const w = JSON.stringify(want);
  check(name, g === w, `\n  got:  ${g}\n  want: ${w}`);
}

/** Tokens as "kind:text" strings (whitespace omitted). */
function lex(src: string): string[] {
  const toks = tokenize(src);
  // Spans must cover the string contiguously.
  let at = 0;
  for (const t of toks) {
    check(`contiguous ${JSON.stringify(src)} @${at}`, t.start === at && t.end > t.start, t);
    at = t.end;
  }
  check(`covers ${JSON.stringify(src)}`, at === src.length, { at, len: src.length });
  return toks.filter((t: Token) => t.kind !== "ws").map((t) => `${t.kind}:${src.slice(t.start, t.end)}`);
}

// ---- word lists ----
eq("hamza ext", [filterName("إمتداد"), filterName("أمتداد"), filterName("امتداد"), filterName("EXT")], ["ext", "ext", "ext", "ext"]);
eq("alef maqsura", filterName("محتوي"), "content");
eq("kind archive", kindOf("ارشيف"), "archive");
eq("named date underscores", [namedDate("this_week"), namedDate("هذا_الاسبوع"), namedDate("امس")], ["thisweek", "thisweek", "yesterday"]);
eq("normalize harakat/tatweel", normalizeWord("مُحـاضَرة"), "محاضرة");

// ---- lexer ----
eq("plain", lex("report final"), ["text:report", "text:final"]);
eq("filter+value", lex("report ext:pdf;docx"), ["text:report", "filter:ext:", "value:pdf", "op:;", "value:docx"]);
eq("arabic filter", lex("تقرير امتداد:pdf"), ["text:تقرير", "filter:امتداد:", "value:pdf"]);
eq("hamza filter", lex("إمتداد:pdf"), ["filter:إمتداد:", "value:pdf"]);
eq("arabic list sep", lex("نوع:صور؛فيديو"), ["filter:نوع:", "value:صور", "op:؛", "value:فيديو"]);
eq("size compare", lex("size:>=10mb"), ["filter:size:", "op:>=", "value:10mb"]);
eq("size range", lex("size:10mb..50mb"), ["filter:size:", "value:10mb", "op:..", "value:50mb"]);
eq("date", lex("dm:2026-01..2026-03"), ["filter:dm:", "value:2026-01", "op:..", "value:2026-03"]);
eq("or + not", lex("a | !b -c"), ["text:a", "op:|", "op:!", "text:b", "op:-", "text:c"]);
eq("lone minus literal", lex("Artist - Song"), ["text:Artist", "text:-", "text:Song"]);
eq("word with hyphen", lex("meeting-notes"), ["text:meeting-notes"]);
eq("group", lex("<a|b> c"), ["op:<", "text:a", "op:|", "text:b", "op:>", "text:c"]);
eq("not group", lex("!<a b>"), ["op:!", "op:<", "text:a", "text:b", "op:>"]);
eq("phrase", lex('"rental contract" x'), ["op:\"", "phrase:rental contract", "op:\"", "text:x"]);
eq("unclosed quote", lex('"abc def'), ["op:\"", "phrase:abc def"]);
eq("quoted value", lex('path:"My Docs"'), ["filter:path:", 'value:"My Docs"']);
eq("content value", lex("content:ميزانية x"), ["filter:content:", "value:ميزانية", "text:x"]);
eq("quoted content value is one token", lex('محتوى:"عقد الإيجار" ext:pdf'),
  ["filter:محتوى:", 'value:"عقد الإيجار"', "filter:ext:", "value:pdf"]);
eq("unclosed quoted value", lex('content:"عقد ال'), ["filter:content:", 'value:"عقد ال']);
eq("negated content", lex("ext:docx -content:مسودة"), ["filter:ext:", "value:docx", "op:-", "filter:content:", "value:مسودة"]);
eq("content in or", lex("a | content:b"), ["text:a", "op:|", "filter:content:", "value:b"]);
eq("wildcards", lex("^IMG*?.jpg$"), ["wildcard:^", "text:IMG", "wildcard:*?", "text:.jpg", "wildcard:$"]);
eq("caret mid-word is text", lex("a^b"), ["text:a^b"]);
eq("semantic ~", lex("~عقد الإيجار ext:pdf"), ["op:~", "value:عقد", "value:الإيجار", "filter:ext:", "value:pdf"]);
eq("sem: stops at |", lex("sem:rental contract | x"), ["filter:sem:", "value:rental", "value:contract", "op:|", "text:x"]);
eq("unknown filter is text", lex("szie:10"), ["text:szie:10"]);
eq("drive letter", lex("C:\\Users"), ["text:C:\\Users"]);
eq("switch filter", lex("case: Report"), ["filter:case:", "text:Report"]);
eq("negated filter", lex("!ext:tmp"), ["op:!", "filter:ext:", "value:tmp"]);
eq("filter value ends at >", lex("<ext:pdf>"), ["op:<", "filter:ext:", "value:pdf", "op:>"]);
eq("emoji (surrogates) stay whole", lex("😀a b"), ["text:😀a", "text:b"]);
for (const s of ["", " ", "|||", "<<<>>>", '"', "!", "-", "~", "ext:", "size:>", "a:b:c", "\u200f\u200e", "؛;", "x".repeat(2000)]) lex(s);

// ---- autocomplete ----
const at = (src: string, lang: "ar" | "en" = "en"): Completion | null => {
  const caret = src.indexOf("|");
  return complete(src.replace("|", ""), caret < 0 ? src.length : caret, lang);
};
const labels = (c: Completion | null) => c?.items.map((s) => s.label) ?? [];

eq("ex → ext", labels(at("report ex")).slice(0, 2), ["ext:", "extension:"]);
eq("one letter: nothing", at("e"), null);
eq("امت → امتداد", labels(at("امت")), ["امتداد:"]);
eq("hamza prefix إمت", labels(at("إمت")), ["امتداد:"]);
eq("نو → نوع", labels(at("نو")), ["نوع:"]);
eq("تا → dates", labels(at("تا")), ["تاريخ:", "تاريخ_التعديل:", "تاريخ_الإنشاء:", "تاريخ_الوصول:"]);
eq("plain word: nothing", at("xyz"), null);
eq("mid-word not a term start", at("foo-ex"), null);
eq("negated prefix", labels(at("!ty")), ["type:"]);
eq("inside phrase: nothing", at('"ex|t"'), null);
eq("already a filter: nothing", at("ex|t:pdf"), null);
eq("type: kinds (en first)", labels(at("type:")).slice(0, 3), ["doc", "image", "video"]);
eq("نوع: kinds (ar first)", labels(at("نوع:", "ar")).slice(0, 3), ["مستندات", "صور", "فيديو"]);
eq("type:pho", labels(at("type:pho")), ["photo"]);
eq("type:صو", labels(at("type:صو")), ["صور", "صوت"]);
eq("type list item", labels(at("type:image;vi")), ["video"]);
eq("type exact: nothing", at("type:image"), null);
eq("dm: dates", labels(at("dm:")).slice(0, 2), ["today", "yesterday"]);
eq("تاريخ: dates ar", labels(at("تاريخ:", "ar")).slice(0, 2), ["اليوم", "أمس"]);
eq("dm:>= keeps op", labels(at("dm:>=th")), [">=thisweek", ">=thismonth", ">=thisyear"]);
eq("هذا prefix", labels(at("dm:هذا")), ["هذا_الأسبوع", "هذا_الشهر", "هذا_العام"]);
eq("size: examples", labels(at("size:")).slice(0, 2), [">100mb", ">1gb"]);
eq("size:>10 units", labels(at("size:>10")).slice(0, 4), [">10kb", ">10mb", ">10gb", ">10tb"]);
eq("size:10m", labels(at("size:10m")), ["10mb"]);
eq("size arabic digits", labels(at("حجم:>١٠", "ar")).slice(0, 2), [">١٠كيلو", ">١٠ميجا"]);
eq("ext:p", labels(at("ext:p")), ["pdf", "pptx", "png", "py"]);
eq("path: no suggestions", at("path:"), null);
eq("content: no value suggestions", at("content:"), null);
eq("محتوى: no value suggestions", at("محتوى:", "ar"), null);
eq("content: typed value: nothing", at("content:تقر"), null);
eq("content: quoted value: nothing", at('content:"عقد ال|'), null);
eq("مح → محتوى", labels(at("مح")), ["محتوى:"]);

// Accepting
{
  const text = "report ex";
  const c = complete(text, text.length, "en")!;
  eq("apply filter", applyCompletion(text, c, c.items[0]), { text: "report ext:", caret: 11 });
  const t2 = "type:im ext:pdf";
  const c2 = complete(t2, 7, "en")!;
  eq("apply kind mid-text", applyCompletion(t2, c2, c2.items[0]), { text: "type:image ext:pdf", caret: 10 });
  const t3 = "size:>10";
  const c3 = complete(t3, t3.length, "en")!;
  eq("apply size unit at end adds space", applyCompletion(t3, c3, c3.items[1]), { text: "size:>10mb ", caret: 11 });
  const t4 = "ty|pe:x";
  eq("caret in filter name: nothing", at(t4), null);
  const t5 = "ex report";
  const c5 = complete(t5, 2, "en")!;
  eq("replace whole word", applyCompletion(t5, c5, c5.items[0]).text, "ext: report");
}

// ---- preview: query words and Arabic-tolerant matches (task 4.9) ----
{
  eq("terms: words, phrase, content:, not negated or filters", queryTerms('تقرير "Annual Plan" -draft ext:pdf content:ميزانية !x'),
    ["annual plan", "ميزانية", "تقرير"]);
  eq("terms: wildcard splits", queryTerms("rep*rt"), ["rep", "rt"]);
  eq("terms: negated content: skipped", queryTerms("ext:docx -content:مسودة !محتوى:قديم تقرير"), ["تقرير"]);
  eq("terms: quoted content: is a phrase", queryTerms('content:"عقد الإيجار" محتوى:ميزانية'), ["عقد الايجار", "ميزانية"]);
  eq("terms: negated word then content:", queryTerms("-x content:y"), ["y"]);
  eq("terms: normalized (hamza, harakat)", queryTerms("أُصول"), ["اصول"]);
  const text = "مقدمة: التَّقْرِيرُ السنوي\nReport and REPORT";
  const m = findMatches(text, queryTerms("تقرير report"));
  eq("matches cover diacritics", m.map(([s, e]) => text.slice(s, e)), ["تَّقْرِيرُ", "Report", "REPORT"]);
  eq("matches: digits", findMatches("رقم ١٢٣", queryTerms("123")).length, 1);
  eq("matches: none without terms", findMatches(text, []), []);
  eq("matches: limit", findMatches("aaaa", ["a"], 2).length, 2);
}

// ---- preview: syntax coloring ----
{
  const src = 'fn main() { let s = "x // y"; } // done\n/* c */ 42';
  const spans = highlightSyntax(src, "rust").map((x) => `${x.cls}:${src.slice(x.start, x.end)}`);
  eq("rust spans", spans, ["kw:fn", "kw:let", 'str:"x // y"', "com:// done", "com:/* c */", "num:42"]);
  const py = 'def f():\n    """doc\n    more"""\n    return 0x1F # hex';
  eq("python spans", highlightSyntax(py, "python").map((x) => `${x.cls}:${py.slice(x.start, x.end)}`),
    ["kw:def", 'str:"""doc\n    more"""', "kw:return", "num:0x1F", "com:# hex"]);
  eq("sql is caseless", highlightSyntax("SELECT 1", "sql").map((x) => x.cls), ["kw", "num"]);
  eq("identifiers with digits are not numbers", highlightSyntax("x1 = 2", "c").map((x) => x.cls), ["num"]);
}

if (failures) {
  console.error(`${failures} check(s) failed`);
  process.exit(1);
}
console.log("queryLang self-check: all passed");

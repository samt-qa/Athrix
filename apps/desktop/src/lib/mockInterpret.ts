// `interpret_query` for the in-browser mock backend: a deliberately TINY subset of the Rust rules
// (crates/query/src/natural.rs), enough to see and test the suggestion line under the field
// without the desktop shell. It knows a few kinds, named dates after «من»/"from" (or after a
// creation / access word: «أُنشئت أمس» → `dc:`, "opened last week" → `da:`), «أكبر/أصغر من N
// ميجا» / "bigger/smaller than N mb", and size words next to a kind or «ملفات»/"files" («ملفات
// كبيرة» → `size:>100mb`, same thresholds as Rust). The real rules live only in Rust.
import type { Interpretation, InterpretPart } from "./api";

const KINDS: Record<string, string> = {
  pdf: "ext:pdf",
  وورد: "ext:docx;doc",
  word: "ext:docx;doc",
  اكسل: "ext:xlsx;xls",
  إكسل: "ext:xlsx;xls",
  excel: "ext:xlsx;xls",
  صور: "type:image",
  photos: "type:image",
  images: "type:image",
  فيديو: "type:video",
  videos: "type:video",
  مستندات: "type:doc",
  documents: "type:doc",
};

const DATES: [string, string][] = [
  ["الأسبوع الماضي", "lastweek"],
  ["هذا الأسبوع", "thisweek"],
  ["الشهر الماضي", "lastmonth"],
  ["هذا الشهر", "thismonth"],
  ["السنة الماضية", "lastyear"],
  ["اليوم", "today"],
  ["أمس", "yesterday"],
  ["last week", "lastweek"],
  ["this week", "thisweek"],
  ["last month", "lastmonth"],
  ["this month", "thismonth"],
  ["last year", "lastyear"],
  ["today", "today"],
  ["yesterday", "yesterday"],
];

const FILES = new Set(["ملفات", "الملفات", "files"]);
const FILLERS = new Set([...FILES, "ابحث", "عن", "show", "me", "find", "the"]);
const PREPS = new Set(["من", "في", "from", "in"]);
/**
 * Whether `dc:` / `da:` may be produced (Rust: `InterpretOptions::date_kinds`). The app's name
 * index stores modification times only, so the app keeps this off and creation / access words
 * still give `dm:`.
 */
const DATE_KINDS = false;
/** Creation / access words before a date (Rust: `DATE_FIELDS`); the default is `dm:`. */
const DATE_FIELDS: Record<string, string> = {
  أنشئت: "dc",
  انشئت: "dc",
  أنشأتها: "dc",
  المنشأة: "dc",
  created: "dc",
  فتحت: "da",
  فتحتها: "da",
  استخدمت: "da",
  opened: "da",
  accessed: "da",
  modified: "dm",
  عدلت: "dm",
};
/** Size words without a number (Rust: `SIZE_WORDS`), only next to a kind or a "files" word. */
const SIZE_WORDS: [string, string][] = [
  ["كبيرة جدا", ">1gb"],
  ["صغيرة جدا", "tiny"],
  ["very large", ">1gb"],
  ["كبيرة", ">100mb"],
  ["الكبيرة", ">100mb"],
  ["large", ">100mb"],
  ["big", ">100mb"],
  ["ضخمة", ">1gb"],
  ["الضخمة", ">1gb"],
  ["huge", ">1gb"],
  ["صغيرة", "<100kb"],
  ["small", "<100kb"],
  ["tiny", "tiny"],
  ["فارغة", "empty"],
  ["empty", "empty"],
];
/** Lower case without harakat («أُنشئت» = «أنشئت»). */
const fold = (w: string) => w.toLowerCase().replace(/[\u064B-\u0652]/g, "");
const UNITS: Record<string, string> = { ميجا: "mb", mb: "mb", جيجا: "gb", gb: "gb", كيلو: "kb", kb: "kb" };

export function mockInterpret(text: string): Interpretation | null {
  // Anything that already looks like query syntax is left alone, like the Rust side.
  if (/[:|<>"~]/.test(text)) return null;
  const words: { w: string; start: number; end: number }[] = [];
  for (const m of text.matchAll(/\S+/g)) words.push({ w: m[0], start: m.index, end: m.index + m[0].length });
  const parts: InterpretPart[] = [];
  const out: string[] = [];
  let cues = 0;
  const isKindOrFiles = (j: number) => j >= 0 && j < words.length && (FILES.has(fold(words[j].w)) || !!KINDS[fold(words[j].w)]);
  const dateAt = (j: number) => {
    const after = words.slice(j).map((x) => fold(x.w)).join(" ");
    return DATES.find(([p]) => after === p || after.startsWith(p + " "));
  };
  for (let i = 0; i < words.length; ) {
    const lower = fold(words[i].w);
    const rest = words.slice(i).map((x) => fold(x.w)).join(" ");
    const push = (n: number, role: string, output: string) => {
      parts.push({ start: words[i].start, end: words[i + n - 1].end, role, output });
      if (output && role !== "ignored") out.push(output);
      i += n;
    };
    // [creation/access word] [من/from] named date
    const field = DATE_FIELDS[lower];
    const skip = field ? 1 : 0;
    const prep = i + skip < words.length && PREPS.has(fold(words[i + skip].w)) ? 1 : 0;
    if (field || prep) {
      const d = dateAt(i + skip + prep);
      if (d) {
        cues += 2;
        push(skip + prep + d[0].split(" ").length, "date", `${DATE_KINDS && field ? field : "dm"}:${d[1]}`);
        continue;
      }
    }
    const word = SIZE_WORDS.find(([p]) => rest === p || rest.startsWith(p + " "));
    if (word) {
      const n = word[0].split(" ").length;
      if (isKindOrFiles(i - 1) || isKindOrFiles(i + n)) {
        cues += 2;
        push(n, "size", `size:${word[1]}`);
        continue;
      }
    }
    const size = /^(أكبر من|أصغر من|bigger than|smaller than) (\d+(?:\.\d+)?) ?(\S+)/.exec(rest);
    if (size && UNITS[size[3]]) {
      cues += 2;
      const op = size[1].startsWith("أكبر") || size[1].startsWith("bigger") ? ">" : "<";
      push(size[0].split(" ").length, "size", `size:${op}${size[2]}${UNITS[size[3]]}`);
      continue;
    }
    if (KINDS[lower]) {
      // Like Rust: a kind next to «ملفات»/"files" is a strong cue, a bare kind a weak one.
      cues += FILES.has(fold(words[i - 1]?.w ?? "")) || FILES.has(fold(words[i + 1]?.w ?? "")) ? 2 : 1;
      push(1, "kind", KINDS[lower]);
      continue;
    }
    if (FILLERS.has(lower)) {
      push(1, "ignored", "");
      continue;
    }
    push(1, "term", words[i].w);
  }
  // Rust: one strong cue (counted 2 here) or two weak ones.
  if (cues < 2) return null;
  return { query: out.join(" "), parts, confidence: Math.min(1, cues * 0.4) };
}

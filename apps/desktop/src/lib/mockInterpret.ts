// `interpret_query` for the in-browser mock backend: a deliberately TINY subset of the Rust rules
// (crates/query/src/natural.rs), enough to see and test the suggestion line under the field
// without the desktop shell. It knows a few kinds, named dates after «من»/"from", and
// «أكبر/أصغر من N ميجا» / "bigger/smaller than N mb". The real rules live only in Rust.
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

const FILLERS = new Set(["ملفات", "files", "ابحث", "عن", "show", "me", "find", "the"]);
const UNITS: Record<string, string> = { ميجا: "mb", mb: "mb", جيجا: "gb", gb: "gb", كيلو: "kb", kb: "kb" };

export function mockInterpret(text: string): Interpretation | null {
  // Anything that already looks like query syntax is left alone, like the Rust side.
  if (/[:|<>"~]/.test(text)) return null;
  const words: { w: string; start: number; end: number }[] = [];
  for (const m of text.matchAll(/\S+/g)) words.push({ w: m[0], start: m.index, end: m.index + m[0].length });
  const parts: InterpretPart[] = [];
  const out: string[] = [];
  let cues = 0;
  for (let i = 0; i < words.length; ) {
    const lower = words[i].w.toLowerCase();
    const rest = words.slice(i).map((x) => x.w.toLowerCase()).join(" ");
    const push = (n: number, role: string, output: string) => {
      parts.push({ start: words[i].start, end: words[i + n - 1].end, role, output });
      if (output && role !== "ignored") out.push(output);
      i += n;
    };
    if ((lower === "من" || lower === "from") && i + 1 < words.length) {
      const after = words.slice(i + 1).map((x) => x.w.toLowerCase()).join(" ");
      const d = DATES.find(([p]) => after === p || after.startsWith(p + " "));
      if (d) {
        cues++;
        push(1 + d[0].split(" ").length, "date", `dm:${d[1]}`);
        continue;
      }
    }
    const size = /^(أكبر من|أصغر من|bigger than|smaller than) (\d+(?:\.\d+)?) ?(\S+)/.exec(rest);
    if (size && UNITS[size[3]]) {
      cues++;
      const op = size[1].startsWith("أكبر") || size[1].startsWith("bigger") ? ">" : "<";
      push(size[0].split(" ").length, "size", `size:${op}${size[2]}${UNITS[size[3]]}`);
      continue;
    }
    if (KINDS[lower]) {
      cues++;
      push(1, "kind", KINDS[lower]);
      continue;
    }
    if (FILLERS.has(lower)) {
      push(1, "ignored", "");
      continue;
    }
    push(1, "term", words[i].w);
  }
  if (cues < 2) return null;
  return { query: out.join(" "), parts, confidence: Math.min(1, cues * 0.4) };
}

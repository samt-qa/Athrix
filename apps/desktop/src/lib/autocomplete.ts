// Autocomplete for the search bar: from the text and the caret, works out what is being typed
// (a filter name, or the value of type:/ext:/size:/dm:…) and lists completions.
// Pure functions over queryLang.ts; descriptions are i18n keys resolved by the component.
import {
  COMPARE_FILTERS, FILTER_NAMES, KINDS, NAMED_DATES, NAMED_SIZES, SIZE_UNITS, asciiDigits, isArabic,
  isDelim, normalizeWord, tokenize, type FilterId, type Token,
} from "./queryLang.ts"; // explicit extension: also run directly by Node (npm run check:query)

export type SuggestionKind = "filter" | "kind" | "date" | "size" | "ext";

export interface Suggestion {
  kind: SuggestionKind;
  /** Text that replaces [from, to). */
  insert: string;
  /** Shown in the list (usually = insert). */
  label: string;
  /** i18n key of the description, with its params. */
  detail: string;
  params?: Record<string, string>;
  /** Add a space after accepting when the completion ends the text. */
  space: boolean;
}

export interface Completion {
  from: number;
  to: number;
  items: Suggestion[];
}

export type Lang = "ar" | "en";

/** Minimum typed characters before filter names are suggested (so plain words stay quiet). */
const MIN_FILTER_PREFIX = 2;
const MAX_ITEMS = 12;

/** Common extensions for `ext:`. */
const COMMON_EXTS = [
  "pdf", "docx", "xlsx", "pptx", "txt", "md", "csv", "jpg", "png", "heic", "mp4", "mov", "mp3", "m4a", "zip", "7z",
  "epub", "html", "rs", "ts", "py", "json",
];

const SIZE_EXAMPLES: readonly [string, string][] = [
  [">100mb", "ac.size.gt100mb"],
  [">1gb", "ac.size.gt1gb"],
  ["<1mb", "ac.size.lt1mb"],
  ["10mb..50mb", "ac.size.range"],
];

const NAME_CHAR = /[\p{Alphabetic}\p{N}_]/u;
const SEPARATORS = new Set([";", "؛"]);

/** Orders `entries` with the UI language's names first (stable otherwise). */
function byLang<T extends readonly [string, unknown]>(entries: readonly T[], lang: Lang): T[] {
  const mine = (name: string) => isArabic(name) === (lang === "ar");
  return [...entries.filter((e) => mine(e[0])), ...entries.filter((e) => !mine(e[0]))];
}

/** Entries whose normalized name starts with `prefix`, one per value, primary names first. */
function matchPrefix<T extends string>(
  entries: readonly (readonly [string, T])[],
  prefix: string,
  key: (s: string) => string,
  lang: Lang,
  onePerValue: boolean,
): (readonly [string, T])[] {
  const p = key(prefix);
  const seen = new Set<T>();
  const out: (readonly [string, T])[] = [];
  for (const e of p ? entries : byLang(entries, lang)) {
    if (!key(e[0]).startsWith(p)) continue;
    if (onePerValue) {
      if (seen.has(e[1])) continue;
      seen.add(e[1]);
    }
    out.push(e);
  }
  return out;
}

const tokenAt = (tokens: Token[], i: number) => tokens.find((t) => t.start <= i && i < t.end);

/**
 * Completions for the caret at `caret` (a collapsed selection) in `text`, or null when there is
 * nothing useful to offer. Suggestions that would only repeat what is already typed are dropped.
 */
export function complete(text: string, caret: number, lang: Lang): Completion | null {
  if (caret < 0 || caret > text.length) return null;
  const tokens = tokenize(text);
  const before = caret > 0 ? tokenAt(tokens, caret - 1) : undefined;

  // Value of a filter: right after its colon, or inside its value.
  if (before?.filter && before.valueStart !== undefined && (before.kind !== "filter" || before.end === caret)) {
    return completeValue(text, caret, before.filter, before.valueStart, lang);
  }

  // A filter name being typed at the start of a term.
  if (before && before.kind === "text") {
    let start = caret;
    while (start > 0 && NAME_CHAR.test(text[start - 1])) start--;
    if (start === caret) return null;
    let end = caret;
    while (end < text.length && NAME_CHAR.test(text[end])) end++;
    if (text[end] === ":") return null;
    // The word must begin a term: preceded by nothing, whitespace, or an operator (`!ext`, `<ext`).
    const prev = start > 0 ? tokenAt(tokens, start - 1) : undefined;
    if (prev && prev.kind !== "ws" && prev.kind !== "op") return null;
    const typed = text.slice(start, caret);
    if ([...typed].length < MIN_FILTER_PREFIX) return null;
    const items = matchPrefix(FILTER_NAMES, typed, normalizeWord, lang, false).map(
      ([name, id]): Suggestion => ({
        kind: "filter", insert: `${name}:`, label: `${name}:`, detail: `ac.filter.${id}`, space: false,
      }),
    );
    return result(start, end, items, text.slice(start, end));
  }
  return null;
}

function result(from: number, to: number, items: Suggestion[], current: string): Completion | null {
  const kept = items.filter((s) => s.insert !== current).slice(0, MAX_ITEMS);
  // A single suggestion equal to what is typed (ignoring case/hamza) adds nothing.
  return kept.length ? { from, to, items: kept } : null;
}

function completeValue(text: string, caret: number, f: FilterId, valueStart: number, lang: Lang): Completion | null {
  if (text[valueStart] === '"') return null;
  // End of the raw value (leading comparison operators belong to it).
  let valueEnd = valueStart;
  if (COMPARE_FILTERS.has(f)) while (valueEnd - valueStart < 2 && /[<>=]/.test(text[valueEnd] ?? "")) valueEnd++;
  while (!isDelim(text[valueEnd])) valueEnd++;
  if (caret > valueEnd) return null;

  if (f === "type" || f === "ext") {
    // The current `;`-separated item.
    let from = caret;
    while (from > valueStart && !SEPARATORS.has(text[from - 1])) from--;
    let to = caret;
    while (to < valueEnd && !SEPARATORS.has(text[to])) to++;
    const typed = text.slice(from, caret).trim();
    const current = text.slice(from, to);
    if (f === "type") {
      // One name per kind: its primary name, or the first alias that matches what is typed.
      const items = matchPrefix(KINDS, typed, normalizeWord, lang, true).map(
        ([name, id]): Suggestion => ({ kind: "kind", insert: name, label: name, detail: `ac.kind.${id}`, space: true }),
      );
      return result(from, to, items, current);
    }
    const p = typed.replace(/^\*?\./, "").toLowerCase();
    const items = COMMON_EXTS.filter((e) => e.startsWith(p)).map(
      (e): Suggestion => ({ kind: "ext", insert: e, label: e, detail: "ac.ext", params: { ext: e.toUpperCase() }, space: false }),
    );
    return result(from, to, items, current);
  }

  if (f === "size" || f === "dm" || f === "dc" || f === "da") {
    const whole = text.slice(valueStart, valueEnd);
    const typed = text.slice(valueStart, caret);
    const op = /^[<>=]{0,2}/.exec(typed)![0];
    const rest = typed.slice(op.length);
    const items = f === "size" ? sizeItems(op, rest, lang) : dateItems(op, rest, lang);
    return result(valueStart, valueEnd, items, whole);
  }
  return null;
}

function dateItems(op: string, rest: string, lang: Lang): Suggestion[] {
  const items = matchPrefix(NAMED_DATES, rest, (s) => normalizeWord(s).replace(/_/g, ""), lang, true).map(
    ([name, id]): Suggestion => ({ kind: "date", insert: op + name, label: op + name, detail: `ac.date.${id}`, space: true }),
  );
  const now = new Date();
  const year = String(now.getFullYear());
  const month = `${year}-${String(now.getMonth() + 1).padStart(2, "0")}`;
  const digits = asciiDigits(rest);
  if (!rest || (/^\d+(-\d*)?$/.test(digits) && (year.startsWith(digits) || month.startsWith(digits)))) {
    if (year.startsWith(digits) && digits !== year) {
      items.push({ kind: "date", insert: op + year, label: op + year, detail: "ac.date.year", params: { year }, space: true });
    }
    items.push({ kind: "date", insert: op + month, label: op + month, detail: "ac.date.month", space: true });
  }
  return items;
}

function sizeItems(op: string, rest: string, lang: Lang): Suggestion[] {
  const digits = asciiDigits(rest);
  // A number typed: offer units (`>10` → `>10mb`), or finish a partly typed unit (`10m` → `10mb`).
  const num = /^(\d+(?:\.\d+)?)(\S*)$/.exec(digits);
  if (num) {
    const [, n, unit] = num;
    const shownNum = rest.slice(0, rest.length - unit.length); // keep the user's digits (Arabic-Indic stays)
    // Arabic unit names only for an Arabic UI or Arabic-Indic digits (they read oddly in English).
    const arabicOk = lang === "ar" || rest !== digits || isArabic(unit);
    return byLang(SIZE_UNITS, lang)
      .filter(([u]) => arabicOk || !isArabic(u))
      .filter(([u]) => normalizeWord(u).startsWith(normalizeWord(unit)))
      .map(([u, id]): Suggestion => ({
        kind: "size", insert: `${op}${shownNum}${u}`, label: `${op}${shownNum}${u}`, detail: `ac.unit.${id}`, params: { n }, space: true,
      }));
  }
  const typed = (op + rest).toLowerCase();
  const items: Suggestion[] = SIZE_EXAMPLES.filter(([ex]) => ex.startsWith(typed)).map(([ex, detail]) => ({
    kind: "size", insert: ex, label: ex, detail, space: true,
  }));
  if (!op) {
    const seen = new Set<string>();
    for (const [name, id] of byLang(NAMED_SIZES, lang)) {
      if (!seen.has(id) && name.startsWith(rest.toLowerCase())) {
        seen.add(id);
        items.push({ kind: "size", insert: name, label: name, detail: `ac.size.${id}`, space: true });
      }
    }
  }
  return items;
}

/** Applies `s` to `text`; returns the new text and the caret after the insertion. */
export function applyCompletion(text: string, c: Completion, s: Suggestion): { text: string; caret: number } {
  const after = text.slice(c.to);
  const space = s.space && after === "" ? " " : "";
  const next = text.slice(0, c.from) + s.insert + space + after;
  return { text: next, caret: c.from + s.insert.length + space.length };
}

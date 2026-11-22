// The query language as the search bar sees it: word lists (filter names, kinds, named dates,
// size words) and a small error-tolerant lexer for syntax coloring and autocomplete.
//
// Mirrors crates/query/src/{lexicon,parser,values}.rs. Keep the lists here in sync with
// lexicon.rs; this is the only TS module that knows them. The Rust parser stays the authority
// on errors (QueryError spans); this lexer never reports errors, it only classifies text.
//
// All offsets are UTF-16 code units, like JS strings and QueryError.start/end.

export type FilterId =
  | "ext" | "type" | "size" | "dm" | "dc" | "da" | "path" | "parent" | "infolder" | "content"
  | "sem" | "regex" | "case" | "ww" | "fuzzy" | "dupe" | "similar" | "tag";

/** Filter names and aliases (lexicon.rs FILTER_NAMES), in suggestion order per language. */
export const FILTER_NAMES: readonly (readonly [string, FilterId])[] = [
  ["ext", "ext"], ["extension", "ext"],
  ["type", "type"],
  ["size", "size"],
  ["dm", "dm"], ["datemodified", "dm"],
  ["dc", "dc"], ["datecreated", "dc"],
  ["da", "da"], ["dateaccessed", "da"],
  ["path", "path"], ["parent", "parent"], ["infolder", "infolder"],
  ["content", "content"],
  ["sem", "sem"], ["semantic", "sem"],
  ["regex", "regex"], ["case", "case"],
  ["ww", "ww"], ["wholeword", "ww"],
  ["fuzzy", "fuzzy"], ["dupe", "dupe"], ["similar", "similar"], ["tag", "tag"],
  // Arabic synonyms
  ["امتداد", "ext"], ["نوع", "type"], ["حجم", "size"],
  ["تاريخ", "dm"], ["تاريخ_التعديل", "dm"], ["تاريخ_الإنشاء", "dc"], ["تاريخ_الوصول", "da"],
  ["مسار", "path"], ["مجلد", "infolder"], ["محتوى", "content"], ["دلالي", "sem"],
  ["مكرر", "dupe"], ["مشابه", "similar"], ["وسم", "tag"],
];

export type KindId = "doc" | "image" | "video" | "audio" | "code" | "archive" | "folder";

/** `type:` values (lexicon.rs KINDS). The first entry of each kind per language is its primary name. */
export const KINDS: readonly (readonly [string, KindId])[] = [
  ["doc", "doc"], ["docs", "doc"], ["document", "doc"], ["documents", "doc"],
  ["image", "image"], ["images", "image"], ["img", "image"], ["picture", "image"], ["pictures", "image"],
  ["photo", "image"], ["photos", "image"],
  ["video", "video"], ["videos", "video"],
  ["audio", "audio"], ["music", "audio"],
  ["code", "code"], ["source", "code"],
  ["archive", "archive"], ["archives", "archive"],
  ["folder", "folder"], ["folders", "folder"], ["dir", "folder"],
  ["مستندات", "doc"], ["مستند", "doc"], ["وثيقة", "doc"], ["وثائق", "doc"],
  ["صور", "image"], ["صورة", "image"],
  ["فيديو", "video"], ["فيديوهات", "video"],
  ["صوت", "audio"], ["صوتيات", "audio"],
  ["كود", "code"], ["برمجة", "code"],
  ["أرشيف", "archive"], ["أرشيفات", "archive"], ["مضغوط", "archive"],
  ["مجلدات", "folder"], ["مجلد", "folder"],
];

export type DateId =
  | "today" | "yesterday" | "thisweek" | "lastweek" | "thismonth" | "lastmonth" | "thisyear" | "lastyear";

/** Named dates for dm:/dc:/da: (lexicon.rs NAMED_DATES). Underscores are ignored when matching. */
export const NAMED_DATES: readonly (readonly [string, DateId])[] = [
  ["today", "today"], ["yesterday", "yesterday"], ["thisweek", "thisweek"], ["lastweek", "lastweek"],
  ["thismonth", "thismonth"], ["lastmonth", "lastmonth"], ["thisyear", "thisyear"], ["lastyear", "lastyear"],
  ["اليوم", "today"], ["أمس", "yesterday"], ["البارحة", "yesterday"],
  ["هذا_الأسبوع", "thisweek"], ["الأسبوع_الماضي", "lastweek"],
  ["هذا_الشهر", "thismonth"], ["الشهر_الماضي", "lastmonth"],
  ["هذا_العام", "thisyear"], ["هذه_السنة", "thisyear"], ["العام_الماضي", "lastyear"], ["السنة_الماضية", "lastyear"],
];

export type NamedSizeId = "empty" | "tiny" | "small" | "medium" | "large" | "huge" | "gigantic";

/** Named sizes (values.rs named_size). Matched exactly (lower case), not Arabic-normalized. */
export const NAMED_SIZES: readonly (readonly [string, NamedSizeId])[] = [
  ["empty", "empty"], ["فارغ", "empty"], ["tiny", "tiny"], ["small", "small"], ["medium", "medium"],
  ["large", "large"], ["huge", "huge"], ["gigantic", "gigantic"],
];

export type UnitId = "kb" | "mb" | "gb" | "tb";

/** Size units offered by autocomplete (values.rs unit_multiplier accepts more spellings). */
export const SIZE_UNITS: readonly (readonly [string, UnitId])[] = [
  ["kb", "kb"], ["mb", "mb"], ["gb", "gb"], ["tb", "tb"],
  ["كيلو", "kb"], ["ميجا", "mb"], ["جيجا", "gb"], ["تيرا", "tb"],
];

/** Filters that may stand alone without a value (`case:`, `dupe:` …). */
export const SWITCH_FILTERS: ReadonlySet<FilterId> = new Set(["case", "ww", "fuzzy", "dupe", "similar"]);
/** Filters whose value may start with a comparison operator (`size:>1mb`, `dm:>=2026`). */
export const COMPARE_FILTERS: ReadonlySet<FilterId> = new Set(["size", "dm", "dc", "da"]);

// ---------- normalization + lookup ----------

const ARABIC_SCRIPT = /[؀-ۿ]/;
export const isArabic = (s: string) => ARABIC_SCRIPT.test(s);

/**
 * lexicon.rs normalize_word: lower case, hamza forms of alef (أ إ آ ٱ) → ا, ى → ي, drop tatweel
 * and harakat. So `إمتداد`, `امتداد`, `أمتداد` all match `امتداد`.
 */
export function normalizeWord(s: string): string {
  return s
    .toLowerCase()
    .replace(/[أإآٱ]/g, "ا")
    .replace(/ى/g, "ي")
    .replace(/[ـً-ْٰ]/g, "");
}

const dateKey = (s: string) => normalizeWord(s).replace(/_/g, "");

function table<T>(entries: readonly (readonly [string, T])[], key: (s: string) => string): Map<string, T> {
  const m = new Map<string, T>();
  for (const [name, v] of entries) if (!m.has(key(name))) m.set(key(name), v);
  return m;
}
const FILTER_MAP = table(FILTER_NAMES, normalizeWord);
const KIND_MAP = table(KINDS, normalizeWord);
const DATE_MAP = table(NAMED_DATES, dateKey);

export const filterName = (word: string): FilterId | undefined => FILTER_MAP.get(normalizeWord(word));
export const kindOf = (word: string): KindId | undefined => KIND_MAP.get(normalizeWord(word));
export const namedDate = (word: string): DateId | undefined => DATE_MAP.get(dateKey(word));

/** Arabic-Indic (٠-٩) and Extended Arabic-Indic (۰-۹) digits → ASCII, `٫` → `.` (values.rs ascii_digits). */
export function asciiDigits(s: string): string {
  return s.replace(/[٠-٩۰-۹٫]/g, (c) => {
    if (c === "٫") return ".";
    const code = c.charCodeAt(0);
    return String(code >= 0x06f0 ? code - 0x06f0 : code - 0x0660);
  });
}

// ---------- lexer ----------

export type TokenKind =
  /** Plain search text. */
  | "text"
  | "ws"
  /** A known filter name with its colon: `ext:`, `امتداد:`. */
  | "filter"
  /** A filter's value (also `~` / `sem:` free text). */
  | "value"
  /** `|` `<` `>` `!` `-` `~` quote marks, comparison operators, `;` `..` inside values. */
  | "op"
  /** The inside of a quoted phrase. */
  | "phrase"
  /** `*` `?` anywhere in a word, `^` at its start, `$` at its end. */
  | "wildcard";

export interface Token {
  kind: TokenKind;
  start: number;
  end: number;
  /** For "filter" tokens and the "value"/"op" tokens of that filter's value. */
  filter?: FilterId;
  /** For a filter and its value tokens: offset right after the filter's colon. */
  valueStart?: number;
}

const isWs = (c: string | undefined) => c !== undefined && /\s/.test(c);
/** parser.rs is_delim: ends a plain word or a raw filter value. */
export const isDelim = (c: string | undefined) =>
  c === undefined || isWs(c) || c === "|" || c === "<" || c === ">" || c === '"';

/** `name:` at the current position (parser.rs filter_name_candidate). Sticky, Unicode-aware. */
const NAME_CANDIDATE = /[\p{Alphabetic}\p{N}_]+(?=:)/uy;

/**
 * Splits `src` into contiguous tokens (their spans cover the whole string, in order). Follows
 * the Rust parser's context rules: `!`/`-` only negate at the start of a term, `>` after
 * `size:`/`dm:` is a comparison, `~` and `sem:` swallow free text up to the next filter, `|`,
 * bracket or `!`/`-` word, and a filter value ends at whitespace, `|`, `<`, `>` or `"`.
 * Malformed input (unclosed quote, unknown filter) still yields tokens.
 */
export function tokenize(src: string): Token[] {
  const n = src.length;
  const out: Token[] = [];
  let pos = 0;

  const push = (kind: TokenKind, start: number, end: number, filter?: FilterId, valueStart?: number) => {
    if (end <= start) return;
    const last = out[out.length - 1];
    if (last && last.kind === kind && last.end === start && last.filter === filter && kind !== "op" && kind !== "filter") {
      last.end = end;
      return;
    }
    out.push(filter ? { kind, start, end, filter, valueStart } : { kind, start, end });
  };

  const skipWs = () => {
    const s = pos;
    while (pos < n && isWs(src[pos])) pos++;
    push("ws", s, pos);
  };

  const candidate = (at: number): { name: string; colon: number } | null => {
    NAME_CANDIDATE.lastIndex = at;
    const m = NAME_CANDIDATE.exec(src);
    return m ? { name: m[0], colon: at + m[0].length } : null;
  };
  const atKnownFilter = () => {
    const c = candidate(pos);
    return !!c && filterName(c.name) !== undefined;
  };

  /**
   * `"…"`: a phrase's quote marks are operators around the `phrase`. A quoted filter value
   * (`content:"عقد الإيجار"`, `path:"My Docs"`) is one `value` token, quotes included, so it
   * colors as a single value. An unclosed quote runs to the end.
   */
  const quoted = (inner: TokenKind, filter?: FilterId, valueStart?: number) => {
    const close = src.indexOf('"', pos + 1);
    const end = close < 0 ? n : close;
    if (inner === "value") {
      push("value", pos, close < 0 ? n : close + 1, filter, valueStart);
      pos = close < 0 ? n : close + 1;
      return;
    }
    push("op", pos, pos + 1, filter, valueStart);
    pos++;
    push(inner, pos, end, filter, valueStart);
    pos = end;
    if (close >= 0) {
      push("op", pos, pos + 1, filter, valueStart);
      pos++;
    }
  };

  const plainWord = () => {
    const s = pos;
    while (pos < n && !isDelim(src[pos])) pos++;
    if (pos === s) {
      push("text", pos, ++pos); // defensive: always make progress
      return;
    }
    for (let i = s; i < pos; i++) {
      const c = src[i];
      const wild = c === "*" || c === "?" || (c === "^" && i === s) || (c === "$" && i === pos - 1);
      push(wild ? "wildcard" : "text", i, i + 1);
    }
  };

  /** parser.rs read_semantic: free text up to the end, `|`, a bracket, `!`/`-`/`~`, or a known filter. */
  const semantic = (valueStart: number) => {
    for (;;) {
      skipWs();
      const c = src[pos];
      if (c === undefined || c === "|" || c === "<" || c === ">" || c === "!" || c === "-" || c === "~") return;
      if (c === '"') {
        quoted("value", "sem", valueStart);
        continue;
      }
      if (atKnownFilter()) return;
      const s = pos;
      while (pos < n && !isDelim(src[pos])) pos++;
      push("value", s, pos, "sem", valueStart);
    }
  };

  /** A raw value: `;`/`؛` list separators and `..` ranges as operators, the rest as value text. */
  const rawValue = (s: number, e: number, f: FilterId, valueStart: number) => {
    const ranged = COMPARE_FILTERS.has(f);
    let i = s;
    while (i < e) {
      const c = src[i];
      if (c === ";" || c === "؛") {
        push("op", i, i + 1, f, valueStart);
        i++;
      } else if (ranged && c === "." && src[i + 1] === ".") {
        push("op", i, i + 2, f, valueStart);
        i += 2;
      } else {
        push("value", i, i + 1, f, valueStart);
        i++;
      }
    }
  };

  const filterValue = (f: FilterId, valueStart: number) => {
    if (f === "sem") return semantic(valueStart);
    if (src[pos] === '"') return quoted("value", f, valueStart);
    if (COMPARE_FILTERS.has(f)) {
      const s = pos;
      while (pos - s < 2 && (src[pos] === "<" || src[pos] === ">" || src[pos] === "=")) pos++;
      push("op", s, pos, f, valueStart);
    }
    const s = pos;
    while (pos < n && !isDelim(src[pos])) pos++;
    rawValue(s, pos, f, valueStart);
  };

  const wordOrFilter = () => {
    const c = candidate(pos);
    const f = c ? filterName(c.name) : undefined;
    if (c && f) {
      const valueStart = c.colon + 1;
      push("filter", pos, valueStart, f, valueStart);
      pos = valueStart;
      filterValue(f, valueStart);
      return;
    }
    // Unknown `name:` is the backend's to report (QueryError); here it is plain text.
    plainWord();
  };

  const primary = () => {
    const c = src[pos];
    if (c === '"') return quoted("phrase");
    if (c === "~") {
      push("op", pos, pos + 1);
      pos++;
      return semantic(pos);
    }
    if (c === "<" || c === ">" || c === "|") {
      push("op", pos, pos + 1);
      pos++;
      return;
    }
    wordOrFilter();
  };

  /** `!`/`-` negate only when something follows directly; a lone one is a literal term. */
  const unary = () => {
    while (src[pos] === "!" || src[pos] === "-") {
      const next = src[pos + 1];
      if (next === undefined || isWs(next) || next === "|" || next === ">") {
        push("text", pos, pos + 1);
        pos++;
        return;
      }
      push("op", pos, pos + 1);
      pos++;
    }
    primary();
  };

  while (pos < n) {
    const c = src[pos];
    if (isWs(c)) skipWs();
    else if (c === "|" || c === "<" || c === ">") {
      push("op", pos, pos + 1);
      pos++;
    } else unary();
  }
  return out;
}

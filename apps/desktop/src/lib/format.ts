// Locale-aware formatting for result rows (design §11: Western digits by default, Intl for
// plurals and relative dates) and helpers for highlights and paths.
import type { Locale } from "../i18n";
import type { Range16 } from "./api";

/** BCP 47 tag for Intl: Arabic with Western digits (design §11 default). */
export const intlLocale = (l: Locale) => (l === "ar" ? "ar-u-nu-latn" : "en");

// Intl formatters are expensive to build; rows call these thousands of times.
const cache = new Map<string, Intl.NumberFormat | Intl.RelativeTimeFormat | Intl.DateTimeFormat>();
function cached<T extends Intl.NumberFormat | Intl.RelativeTimeFormat | Intl.DateTimeFormat>(key: string, make: () => T): T {
  let f = cache.get(key) as T | undefined;
  if (!f) cache.set(key, (f = make()));
  return f;
}

export function formatCount(n: number, l: Locale): string {
  return cached(`n:${l}`, () => new Intl.NumberFormat(intlLocale(l))).format(n);
}

const SIZE_UNITS = ["byte", "kilobyte", "megabyte", "gigabyte", "terabyte"] as const;

/** Human size, 1024-based like the query language's `size:` units. */
export function formatSize(bytes: number, l: Locale): string {
  let v = bytes;
  let u = 0;
  while (v >= 1024 && u < SIZE_UNITS.length - 1) {
    v /= 1024;
    u++;
  }
  const digits = u === 0 || v >= 10 ? 0 : 1;
  return cached(`s:${l}:${u}:${digits}`, () => new Intl.NumberFormat(intlLocale(l), {
    style: "unit", unit: SIZE_UNITS[u], unitDisplay: "short", maximumFractionDigits: digits,
  })).format(v);
}

const STEPS: [Intl.RelativeTimeFormatUnit, number][] = [
  ["second", 60], ["minute", 60], ["hour", 24], ["day", 7], ["week", 4.35], ["month", 12], ["year", Infinity],
];

/** "3 hours ago" / "قبل 3 ساعات" (Intl handles Arabic dual and plural forms). */
export function formatRelative(unixSec: number, l: Locale, nowMs = Date.now()): string {
  let v = unixSec - nowMs / 1000;
  for (const [unit, size] of STEPS) {
    if (Math.abs(v) < size) {
      return cached(`r:${l}`, () => new Intl.RelativeTimeFormat(intlLocale(l), { numeric: "auto" }))
        .format(Math.round(v), unit);
    }
    v /= size;
  }
  return "";
}

export function formatDateTime(unixSec: number, l: Locale): string {
  return cached(`d:${l}`, () => new Intl.DateTimeFormat(intlLocale(l), { dateStyle: "medium", timeStyle: "short" }))
    .format(new Date(unixSec * 1000));
}

export interface TextPart {
  text: string;
  match: boolean;
}

/** Splits `text.slice(from, to)` into plain and matched parts using UTF-16 `ranges` of `text`. */
export function highlightParts(text: string, ranges: Range16[], from = 0, to = text.length): TextPart[] {
  const parts: TextPart[] = [];
  let pos = from;
  for (const [rs, re] of ranges) {
    const s = Math.max(rs, from);
    const e = Math.min(re, to);
    if (e <= s || s < pos) continue; // outside the slice, or overlapping (ignored defensively)
    if (s > pos) parts.push({ text: text.slice(pos, s), match: false });
    parts.push({ text: text.slice(s, e), match: true });
    pos = e;
  }
  if (pos < to) parts.push({ text: text.slice(pos, to), match: false });
  return parts;
}

/** Splits a file name into stem and ".ext" so the extension can be isolated as LTR text. */
export function splitName(name: string, ext: string, isDir: boolean): [stem: string, dotExt: string] {
  if (isDir || !ext) return [name, ""];
  const at = name.length - ext.length - 1;
  if (at <= 0 || name[at] !== ".") return [name, ""];
  return [name.slice(0, at), name.slice(at)];
}

export interface PathSegment {
  text: string;
  /** The separator that follows this segment ("" for the last one). */
  sep: string;
}

/** Splits a path into segments, keeping the original separators (`\` or `/`). */
export function splitPath(path: string): PathSegment[] {
  const out: PathSegment[] = [];
  const re = /[\\/]+/g;
  let last = 0;
  let m: RegExpExecArray | null;
  while ((m = re.exec(path))) {
    out.push({ text: path.slice(last, m.index), sep: m[0] });
    last = m.index + m[0].length;
  }
  if (last < path.length) out.push({ text: path.slice(last), sep: "" });
  return out;
}

/** Context kept before the first match of a snippet when its start is trimmed. */
const SNIPPET_LEAD = 28;

/**
 * A content snippet laid out for one line: whitespace runs (line breaks, tabs) become single
 * spaces, and when the first match sits far into the text the start is cut at a word boundary
 * with a leading "…", so the match is visible before the line's own ellipsis. Highlight ranges
 * are UTF-16 offsets of `text` and stay valid through both steps (each is remapped).
 */
export function snippetParts(text: string, highlights: Range16[], lead = SNIPPET_LEAD): TextPart[] {
  let from = 0;
  const first = highlights[0]?.[0] ?? 0;
  if (first > lead * 1.5) {
    from = first - lead;
    // Back to the start of a word (never inside a surrogate pair or a word).
    const space = text.lastIndexOf(" ", from);
    from = space >= 0 && first - space < lead * 2 ? space + 1 : from;
    const code = text.charCodeAt(from);
    if (code >= 0xdc00 && code <= 0xdfff) from--;
  }
  const parts = highlightParts(text, highlights, from).map((p) => ({ ...p, text: p.text.replace(/\s+/g, " ") }));
  if (from > 0 && !text.slice(0, from).trim().endsWith("…")) parts.unshift({ text: "…", match: false });
  return parts;
}

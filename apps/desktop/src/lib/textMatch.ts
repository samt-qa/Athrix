// Finding the query's words inside preview text (task 4.9), Arabic-tolerant.
//
// Done in the UI rather than by the backend: the text is already here, the words come from the
// field as the user types (no extra IPC round trip per keystroke), and the result is UTF-16
// ranges into the very string being rendered. The normalization approximates nass's name
// normalization (case, harakat, tatweel, hamza forms of alef, alef maqsura, Arabic-Indic
// digits, and the name folds ة → ه, ؤ → و, ئ → ي that content search also applies); it is the
// same one the mock backend uses for names. Every fold is one UTF-16 unit for one, so the map
// stays exact. Offsets map back to the original
// text, so a match covers its diacritics too.
import type { Range16 } from "./api";
import { tokenize, type Token } from "./queryLang.ts";

const HAMZA: Record<string, string> = {
  "أ": "ا", "إ": "ا", "آ": "ا", "ٱ": "ا", "ى": "ي",
  // Name search folds (athrix_text_ar::normalize_name_into); the standalone hamza ء is kept.
  "ة": "ه", "ؤ": "و", "ئ": "ي",
};

/** Harakat, Quranic marks, superscript alef and tatweel: dropped by the normalization. */
export const isMark = (code: number) => (code >= 0x064b && code <= 0x065f) || code === 0x0670 || code === 0x0640;

/**
 * Normalized `s` plus, for each of its UTF-16 units, the index of the unit of `s` it came from.
 */
export function normalizeWithMap(s: string): { norm: string; map: number[] } {
  let norm = "";
  const map: number[] = [];
  for (let i = 0; i < s.length; i++) {
    const c = s[i];
    const code = c.charCodeAt(0);
    if (isMark(code)) continue;
    let n = HAMZA[c] ?? c;
    if (code >= 0x0660 && code <= 0x0669) n = String(code - 0x0660);
    else if (code >= 0x06f0 && code <= 0x06f9) n = String(code - 0x06f0);
    n = n.toLowerCase();
    for (let k = 0; k < n.length; k++) {
      norm += n[k];
      map.push(i);
    }
  }
  return { norm, map };
}

export const normalizeText = (s: string) => normalizeWithMap(s).norm;

/**
 * Maps [start, end) of a normalized string back to UTF-16 units of the original, stretching the
 * end over trailing marks so diacritics stay with their letter.
 */
export function toOriginalRange(original: string, map: number[], start: number, end: number): Range16 {
  const from = map[start];
  let to = map[end - 1] + 1;
  while (to < original.length && isMark(original.charCodeAt(to))) to++;
  return [from, to];
}

/** Filters whose value is text to look for inside the content. */
const CONTENT_FILTERS = new Set(["content"]);

/**
 * The words of query `src` worth highlighting in a preview, normalized: plain words, quoted
 * phrases and `content:` / `محتوى:` values (a quoted value as one phrase), minus negated ones
 * (`!word`, `-word`, `-content:word`). Wildcards split a word (`rep*rt` → `rep`, `rt`); filter
 * names and other filter values are skipped. Longest first.
 */
export function queryTerms(src: string): string[] {
  const tokens = tokenize(src);
  const out = new Set<string>();
  let word: string[] = [];
  let negated = false;
  const flush = () => {
    if (!negated) for (const w of word) {
      const n = normalizeText(w).trim();
      if (n) out.add(n);
    }
    word = [];
    negated = false;
  };
  for (let i = 0; i < tokens.length; i++) {
    const tok: Token = tokens[i];
    const text = src.slice(tok.start, tok.end);
    switch (tok.kind) {
      case "text":
        if (!word.length) negated = isNegation(tokens[i - 1], src, tok.start);
        word.push(text);
        break;
      case "wildcard":
        break; // splits the word: the pieces around it are pushed separately
      case "phrase": {
        flush();
        negated = isNegation(tokens[i - 2], src, tokens[i - 1]?.start ?? tok.start);
        word = [text];
        flush();
        break;
      }
      case "value":
        flush();
        if (tok.filter && CONTENT_FILTERS.has(tok.filter)) {
          // The filter token (`content:`) and whether a `!` / `-` stands right before it.
          let f = i - 1;
          while (f >= 0 && tokens[f].kind !== "filter") f--;
          negated = f >= 0 && isNegation(tokens[f - 1], src, tokens[f].start);
          // A quoted value (one token, quotes included) is a phrase: `content:"عقد الإيجار"`.
          word = text.startsWith('"') ? [text.replace(/^"|"$/g, "")] : text.split(/\s+/);
          flush();
        }
        break;
      default:
        flush();
    }
  }
  flush();
  return [...out].sort((a, b) => b.length - a.length);
}

/** True if `prev` is a `!` / `-` operator right before position `at`. */
function isNegation(prev: Token | undefined, src: string, at: number): boolean {
  if (!prev || prev.kind !== "op" || prev.end !== at) return false;
  const op = src.slice(prev.start, prev.end);
  return op === "!" || op === "-";
}

/**
 * Ranges of `text` (UTF-16, sorted, non-overlapping) where any of `terms` (already normalized,
 * see {@link queryTerms}) occurs, at most `limit` of them.
 */
export function findMatches(text: string, terms: string[], limit = 5000): Range16[] {
  if (!terms.length || !text) return [];
  const { norm, map } = normalizeWithMap(text);
  const found: [number, number][] = [];
  for (const term of terms) {
    let at = norm.indexOf(term);
    while (at >= 0 && found.length < limit * 2) {
      found.push([at, at + term.length]);
      at = norm.indexOf(term, at + term.length);
    }
  }
  found.sort((a, b) => a[0] - b[0] || b[1] - a[1]);
  const merged: [number, number][] = [];
  for (const r of found) {
    const last = merged[merged.length - 1];
    if (last && r[0] < last[1]) last[1] = Math.max(last[1], r[1]);
    else merged.push([r[0], r[1]]);
    if (merged.length > limit) break;
  }
  return merged.slice(0, limit).map(([s, e]) => toOriginalRange(text, map, s, e));
}

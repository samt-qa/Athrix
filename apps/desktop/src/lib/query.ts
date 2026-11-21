// Builds the query string sent to `search` from what the user typed plus the active filter chip.
// The input keeps the user's text untouched; the chip only changes what is sent.

export const CHIP_KINDS = ["all", "doc", "image", "code", "folder"] as const;
export type ChipKind = (typeof CHIP_KINDS)[number];

const TYPE_FILTER = /^(type|نوع):/i;

/**
 * With a chip other than "all": blanks out the user's own top-level `type:` tokens (replacing them
 * with spaces of the same length, so UTF-16 offsets in QueryError spans still point into the
 * user's text) and appends ` type:<kind>`. Quoted text is left alone.
 */
export function composeQuery(text: string, chip: ChipKind): string {
  if (chip === "all" || !text.trim()) return text;
  let out = "";
  let i = 0;
  while (i < text.length) {
    if (/\s/.test(text[i])) {
      out += text[i++];
      continue;
    }
    if (text[i] === '"') {
      const close = text.indexOf('"', i + 1);
      const end = close < 0 ? text.length : close + 1;
      out += text.slice(i, end);
      i = end;
      continue;
    }
    const start = i;
    while (i < text.length && !/\s/.test(text[i])) i++;
    const word = text.slice(start, i);
    out += TYPE_FILTER.test(word) ? " ".repeat(word.length) : word;
  }
  return `${out} type:${chip}`;
}

/**
 * The text to put in the search field for a "did you mean" query: the backend rewrote the query as
 * sent, so the chip's ` type:<kind>` suffix (see `composeQuery`) is dropped again.
 */
export function suggestionInput(suggested: string, chip: ChipKind): string {
  const suffix = ` type:${chip}`;
  const text = chip !== "all" && suggested.endsWith(suffix) ? suggested.slice(0, -suffix.length) : suggested;
  return text.trimEnd();
}

/** Replaces the error span `[start, end)` of `text` with `suggestion` (keeping a filter's ':'). */
export function applySuggestion(text: string, start: number, end: number, suggestion: string): string {
  const span = text.slice(start, end);
  const replacement = span.endsWith(":") && !suggestion.endsWith(":") ? `${suggestion}:` : suggestion;
  return text.slice(0, start) + replacement + text.slice(end);
}

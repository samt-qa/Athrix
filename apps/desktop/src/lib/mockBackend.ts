// In-browser stand-in for the Rust backend (docs/IPC-CONTRACT.md), used whenever the UI runs
// outside Tauri. It is deliberately small but honest about the contract: UTF-16 highlight ranges,
// requestId echo + cancellation of older requests, `items: []` until the index is ready,
// QueryError with kind/start/end/suggestion, and `index://status` events.
//
// The fake index is deterministic (seeded PRNG): the same names, folders and sizes on every load.
// Test switches (URL query string): `?mockEntries=N` (default 4000), `?mockIndexError`,
// `?mockStale` (the first open / reveal / copy acts as if a re-index ran after the search:
// it rejects with the stale-id error and the ids change, like the Rust side).
import type {
  CommandArgs, CommandName, CommandResult, IndexRoots, IndexStatus, PreviewBody, PreviewResponse, QueryError, Range16,
  ResultItem, SearchResponse, SearchSuggestion, Volume,
} from "./api";
import { categoryOf, type FileCategory } from "./fileKinds";
import { normalizeWithMap, toOriginalRange as toOriginal } from "./textMatch";

const params = new URLSearchParams(window.location.search);
const ENTRY_COUNT = clampInt(Number(params.get("mockEntries") ?? 4000), 10, 200_000);
const INDEX_DELAY_MS = 1200;
const HOME = "C:\\Users\\Sara";
/** Some folders live on a second drive so the search scope can be tried out. */
const DATA = "D:\\Data";
const ON_D = /^(Archive|Videos|Projects)\b/;
const baseOf = (folder: string) => (ON_D.test(folder) ? DATA : HOME);
const VOLUMES: Volume[] = [
  { path: "C:\\", name: "C:", label: "Windows", kind: "fixed" },
  { path: "D:\\", name: "D:", label: "Data", kind: "fixed" },
  { path: "E:\\", name: "E:", label: "USB", kind: "removable" },
];
const WHOLE_COMPUTER = VOLUMES.filter((v) => v.kind === "fixed").map((v) => v.path);

function clampInt(n: number, min: number, max: number) {
  return Number.isFinite(n) ? Math.min(max, Math.max(min, Math.round(n))) : min;
}

// ---------- deterministic fake index ----------

function mulberry32(seed: number) {
  return () => {
    seed |= 0;
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const FOLDERS = [
  "Documents", "Documents\\العمل\\التقارير", "Documents\\العمل\\العقود", "Documents\\Finance\\2026",
  "Documents\\الجامعة\\مُحَاضَرَاتُ الفِقْهِ", "Documents\\الجامعة\\رسالة الماجستير", "Desktop", "Downloads",
  "Pictures\\رحلة إسطنبول 2025", "Pictures\\Screenshots", "Music\\تِلَاوَات", "Videos\\Family",
  "Projects\\athrix\\src", "Projects\\athrix\\crates\\query\\src", "Projects\\website", "Archive\\Backups",
  "Documents\\كُتُب\\العقيدة", "Documents\\Personal\\Receipts",
];

/** [base name, allowed extensions]. Arabic bases carry diacritics on purpose. */
const BASES: [string, string[]][] = [
  ["تقرير المبيعات", ["pdf", "xlsx", "docx"]],
  ["تقرير_الربع_الثالث", ["xlsx", "pdf"]],
  ["ملاحظات حول التقرير", ["md", "txt"]],
  ["مُحَاضَرَةُ الفِقْهِ", ["pdf", "mp3", "docx"]],
  ["مُذَكِّرَةُ أُصُولِ الفِقْهِ", ["pdf", "docx"]],
  ["كِتَابُ التَّوْحِيدِ", ["pdf", "epub"]],
  ["تِلَاوَةُ سُورَةِ الكَهْفِ", ["mp3", "m4a"]],
  ["الميزانية السنوية", ["xlsx", "csv"]],
  ["خطة المشروع", ["docx", "pptx", "pdf"]],
  ["عقد الإيجار", ["pdf", "docx"]],
  ["إيصال الدفع", ["pdf", "png", "jpg"]],
  ["فاتورة الكهرباء", ["pdf"]],
  ["سيرة ذاتية", ["pdf", "docx"]],
  ["صورة العائلة", ["jpg", "png", "heic"]],
  ["رسالة الماجستير", ["docx", "pdf"]],
  ["عرض تقديمي للعملاء", ["pptx", "pdf"]],
  ["جدول الحصص", ["xlsx", "pdf"]],
  ["دليل المستخدم", ["pdf", "md"]],
  ["استراتيجية_التحول_الرقمي_للمؤسسات_الحكومية_النسخة_النهائية_المعتمدة", ["docx"]],
  ["Quarterly Report", ["pdf", "xlsx"]],
  ["invoice", ["pdf"]],
  ["meeting-notes", ["md", "docx", "txt"]],
  ["Budget 2026", ["xlsx", "csv"]],
  ["holiday photo", ["jpg", "png"]],
  ["Screenshot", ["png"]],
  ["IMG", ["jpg", "heic"]],
  ["project plan", ["pptx", "docx"]],
  ["resume", ["pdf", "docx"]],
  ["contract", ["pdf"]],
  ["design-system", ["md", "pdf", "fig"]],
  ["main", ["rs", "ts", "py", "go"]],
  ["parser", ["rs", "ts"]],
  ["index", ["ts", "tsx", "html", "js"]],
  ["README", ["md"]],
  ["backup", ["zip", "7z"]],
  ["lecture", ["mp4", "pdf"]],
  ["family trip", ["mp4", "mov"]],
];

const ARABIC_DIGITS = "٠١٢٣٤٥٦٧٨٩";

interface MockEntry {
  item: Omit<ResultItem, "highlights">;
  /** Normalized name, and for each of its UTF-16 units the index in the original name. */
  norm: string;
  map: number[];
}

/** Typical byte size per category, so sizes look plausible. */
const SIZE_SCALE: Record<FileCategory, number> = {
  folder: 0, doc: 2e6, sheet: 6e5, slides: 8e6, image: 4e6, media: 3e8, code: 2e4, archive: 9e8, other: 1e6,
};

function buildIndex(count: number): MockEntry[] {
  const rnd = mulberry32(0x5eed);
  const pick = <T,>(a: T[]) => a[Math.floor(rnd() * a.length)];
  const nowSec = Math.floor(Date.now() / 1000);
  const entries: MockEntry[] = [];
  let id = 1;

  const push = (item: Omit<ResultItem, "highlights" | "id">) => {
    const { norm, map } = normalizeWithMap(item.name);
    entries.push({ item: { ...item, id: id++ }, norm, map });
  };

  // The folders themselves (and their ancestors) are entries too.
  const seen = new Set<string>();
  for (const f of FOLDERS) {
    const parts = f.split("\\");
    for (let i = 0; i < parts.length; i++) {
      const rel = parts.slice(0, i + 1).join("\\");
      if (seen.has(rel)) continue;
      seen.add(rel);
      push({
        name: parts[i], parent: [baseOf(f), ...parts.slice(0, i)].join("\\"), isDir: true, size: null,
        modified: nowSec - Math.floor(rnd() * 400 * 86400), ext: "",
      });
    }
  }

  while (entries.length < count) {
    const [base, exts] = pick(BASES);
    const ext = pick(exts);
    const r = rnd();
    const arabic = /[\u0600-\u06FF]/.test(base);
    let stem = base;
    if (r < 0.25) stem += ` ${1 + Math.floor(rnd() * 12)}`;
    else if (r < 0.35 && arabic) stem += ` ${ARABIC_DIGITS[1 + Math.floor(rnd() * 9)]}`;
    else if (r < 0.45) stem += `_v${2 + Math.floor(rnd() * 4)}`;
    else if (r < 0.52) stem += arabic ? " (نسخة)" : " - Copy";
    else if (r < 0.65) stem += ` 2026-0${1 + Math.floor(rnd() * 9)}-1${Math.floor(rnd() * 9)}`;
    else if (r < 0.75) stem += `_${1000 + Math.floor(rnd() * 9000)}`;
    const cat = categoryOf(ext, false);
    // Spread modification times from minutes to years ago (skewed towards recent).
    const age = Math.floor(Math.pow(rnd(), 3) * 3 * 365 * 86400) + 30;
    push({
      name: `${stem}.${ext}`, parent: ((f) => `${baseOf(f)}\\${f}`)(pick(FOLDERS)), isDir: false,
      size: Math.max(1, Math.floor(SIZE_SCALE[cat] * Math.pow(rnd() * 2, 3))),
      modified: nowSec - age, ext,
    });
  }
  return entries;
}

// ---------- normalization (approximation of nass: case, harakat, tatweel, hamza, ya, digits) ----------
// Shared with the preview's match highlighting (lib/textMatch.ts).

/** Maps a [start, end) range of the normalized name back to UTF-16 units of the original,
 *  stretching the end over trailing diacritics so marks stay with their letter. */
function toOriginalRange(e: MockEntry, start: number, end: number): Range16 {
  return toOriginal(e.item.name, e.map, start, end);
}

// ---------- a tiny subset of the query language ----------

const KNOWN_FILTERS = [
  "ext", "type", "size", "dm", "dc", "da", "path", "parent", "infolder", "content", "sem", "regex",
  "case", "ww", "fuzzy", "dupe", "similar", "tag",
];
const ARABIC_FILTERS: Record<string, string> = {
  "امتداد": "ext", "نوع": "type", "حجم": "size", "تاريخ": "dm", "مسار": "path", "مجلد": "infolder",
  "محتوى": "content", "وسم": "tag",
};
const VALUE_FILTERS = new Set(["ext", "type", "size", "dm", "dc", "da", "path", "parent", "infolder", "tag"]);
/** Valid filters the name index cannot answer yet (contract: QueryError.kind "Unsupported"). */
const UNSUPPORTED = new Set(["content", "sem", "tag", "regex", "dupe", "similar", "dc", "da"]);
const KINDS: Record<string, string> = {
  doc: "doc", docs: "doc", document: "doc", documents: "doc", "مستند": "doc", "مستندات": "doc",
  image: "image", images: "image", "صورة": "image", "صور": "image",
  video: "video", "فيديو": "video", audio: "audio", "صوت": "audio",
  code: "code", "كود": "code", archive: "archive", "أرشيف": "archive", "ارشيف": "archive",
  folder: "folder", folders: "folder", "مجلد": "folder", "مجلدات": "folder",
};
const KIND_NAMES = ["doc", "image", "video", "audio", "code", "archive", "folder"];

interface Parsed {
  terms: string[];
  negated: string[];
  kinds: string[] | null;
  exts: string[] | null;
  size: { op: string; bytes: number } | null;
  /** `fuzzy:word` terms. */
  fuzzy: string[];
  /** The valueless `fuzzy:` switch: every plain word is fuzzy. */
  fuzzyAll: boolean;
}

class MockParseError extends Error {
  constructor(public err: QueryError) {
    super(err.message);
  }
}

function fail(kind: string, start: number, end: number, message: string, suggestion: string | null = null): never {
  throw new MockParseError({ kind, start, end, message, suggestion });
}

function levenshtein(a: string, b: string): number {
  const d = Array.from({ length: b.length + 1 }, (_, j) => j);
  for (let i = 1; i <= a.length; i++) {
    let prev = d[0];
    d[0] = i;
    for (let j = 1; j <= b.length; j++) {
      const tmp = d[j];
      d[j] = Math.min(d[j] + 1, d[j - 1] + 1, prev + (a[i - 1] === b[j - 1] ? 0 : 1));
      prev = tmp;
    }
  }
  return d[b.length];
}

function closest(word: string, list: string[]): string | null {
  let best: string | null = null;
  let bestD = 3;
  for (const w of list) {
    const dist = levenshtein(word.toLowerCase(), w);
    if (dist < bestD) [best, bestD] = [w, dist];
  }
  return best;
}

const SIZE_UNITS: Record<string, number> = { "": 1, b: 1, kb: 1024, mb: 1024 ** 2, gb: 1024 ** 3 };

function parse(q: string): Parsed {
  const out: Parsed = { terms: [], negated: [], kinds: null, exts: null, size: null, fuzzy: [], fuzzyAll: false };
  let i = 0;
  while (i < q.length) {
    if (/\s/.test(q[i])) { i++; continue; }
    if (q[i] === '"') {
      const close = q.indexOf('"', i + 1);
      if (close < 0) fail("UnclosedQuote", i, q.length, "unclosed `\"`");
      if (close === i + 1) fail("EmptyPhrase", i, close + 1, "empty phrase");
      out.terms.push(q.slice(i + 1, close));
      i = close + 1;
      continue;
    }
    const start = i;
    while (i < q.length && !/\s/.test(q[i])) i++;
    const word = q.slice(start, i);
    const colon = word.indexOf(":");
    if (colon > 0) {
      const rawName = word.slice(0, colon);
      const value = word.slice(colon + 1);
      const name = ARABIC_FILTERS[rawName] ?? rawName.toLowerCase();
      const plain = rawName.length === 1 || /\d/.test(rawName) || value.startsWith("//");
      if (KNOWN_FILTERS.includes(name)) {
        applyFilter(out, name, value, start, start + colon + 1);
        continue;
      }
      if (!plain) {
        const s = closest(rawName, KNOWN_FILTERS);
        fail("UnknownFilter", start, start + colon + 1,
          s ? `unknown filter \`${rawName}:\`; did you mean \`${s}:\`?` : `unknown filter \`${rawName}:\``, s);
      }
    }
    if (word[0] === "~") fail("Unsupported", start, q.length, "semantic search is not supported by name search yet");
    if ((word[0] === "!" || word[0] === "-") && word.length > 1) out.negated.push(word.slice(1));
    else if (word === "!" || word === "|") {
      if (word === "!") fail("DanglingOperator", start, i, "`!` has nothing to apply to");
    } else out.terms.push(word);
  }
  return out;
}

function applyFilter(out: Parsed, name: string, value: string, start: number, valueStart: number) {
  if (!value && VALUE_FILTERS.has(name)) fail("MissingValue", start, valueStart, `\`${name}:\` needs a value`);
  if (name === "type") {
    out.kinds ??= [];
    let pos = valueStart;
    for (const item of value.split(/[;؛]/)) {
      const k = KINDS[item.toLowerCase()];
      if (!k) {
        fail("InvalidKind", pos, pos + item.length, `unknown kind \`${item}\``, closest(item, KIND_NAMES));
      }
      out.kinds.push(k);
      pos += item.length + 1;
    }
  } else if (name === "ext") {
    out.exts = value.split(/[;؛]/).map((e) => e.replace(/^\*?\./, "").toLowerCase()).filter(Boolean);
  } else if (name === "size") {
    const m = /^(>=|<=|>|<|=)?(\d+(?:\.\d+)?)(b|kb|mb|gb)?$/i.exec(value);
    if (!m) fail("InvalidSize", valueStart, valueStart + value.length, `cannot read size \`${value}\``);
    out.size = { op: m[1] ?? "=", bytes: Number(m[2]) * SIZE_UNITS[(m[3] ?? "").toLowerCase()] };
  }
  else if (name === "fuzzy") {
    if (value) out.fuzzy.push(value);
    else out.fuzzyAll = true;
  }
  else if (UNSUPPORTED.has(name)) {
    fail("Unsupported", start, valueStart + value.length, `\`${name}:\` is not supported by name search yet`);
  }
  // Other known filters (dm, path, parent, infolder, case, ww) are accepted and ignored by the mock.
}

function kindMatches(kinds: string[], item: MockEntry["item"]): boolean {
  const cat = categoryOf(item.ext, item.isDir);
  return kinds.some((k) => {
    switch (k) {
      case "folder": return item.isDir;
      case "doc": return cat === "doc" || cat === "sheet" || cat === "slides";
      case "video": return cat === "media" && ["mp4", "mkv", "mov", "avi", "webm"].includes(item.ext);
      case "audio": return cat === "media" && !["mp4", "mkv", "mov", "avi", "webm"].includes(item.ext);
      default: return cat === k;
    }
  });
}

function sizeMatches(s: NonNullable<Parsed["size"]>, size: number | null): boolean {
  if (size === null) return false;
  switch (s.op) {
    case ">": return size > s.bytes;
    case ">=": return size >= s.bytes;
    case "<": return size < s.bytes;
    case "<=": return size <= s.bytes;
    default: return Math.abs(size - s.bytes) <= s.bytes * 0.05;
  }
}

// ---------- state + commands ----------

/** Must match STALE_ID_ERROR in src-tauri/src/commands.rs (and lib/api.ts). */
const STALE_ID_ERROR = "this result is out of date; search again";
/** Ids carry the index generation in their high bits, like Rust's `generation << 32 | item`. */
const GEN_UNIT = 2 ** 32;
let generation = 1;
let staleOnce = params.has("mockStale");

/** A new index generation: every id changes, so ids handed out before become stale. */
function bumpGeneration(entries: MockEntry[]) {
  generation++;
  for (const e of entries) e.item = { ...e.item, id: generation * GEN_UNIT + (e.item.id % GEN_UNIT) };
}

let index: MockEntry[] | null = null;
let status: IndexStatus = { state: "idle", entries: 0, roots: [], elapsedMs: null, error: null };
const listeners = new Set<(s: IndexStatus) => void>();
let latestRequest = 0;

function setStatus(next: IndexStatus) {
  status = next;
  for (const l of listeners) l({ ...status });
}

export function onIndexStatus(handler: (s: IndexStatus) => void): () => void {
  listeners.add(handler);
  return () => listeners.delete(handler);
}

/** The indexed-roots setting (`null` = whole computer), kept for the page's lifetime. */
let chosenRoots: string[] | null = null;
const resolvedRoots = () => chosenRoots ?? WHOLE_COMPUTER;

/** Comparable path: "/" separators, no trailing "/", lower case. */
const pathKey = (p: string) => p.replace(/\\/g, "/").replace(/\/+$/, "").toLowerCase();
const under = (path: string, root: string) => {
  const k = pathKey(path);
  const r = pathKey(root);
  return k === r || k.startsWith(`${r}/`);
};

function startIndexing(roots?: string[]) {
  if (status.state === "indexing") return;
  const r = roots?.length ? roots : resolvedRoots();
  setStatus({ state: "indexing", entries: 0, roots: r, elapsedMs: null, error: null });
  const t0 = performance.now();
  setTimeout(() => {
    if (params.has("mockIndexError")) {
      setStatus({ ...status, state: "error", error: "access denied: C:\\Users\\Sara (mock)" });
      return;
    }
    index = buildIndex(ENTRY_COUNT).filter((e) => r.some((root) => under(e.item.parent, root)));
    bumpGeneration(index);
    setStatus({
      state: "ready", entries: index.length, roots: r, elapsedMs: Math.round(performance.now() - t0), error: null,
    });
  }, INDEX_DELAY_MS);
}

// ---------- fuzzy matching and "did you mean" (rough stand-ins for fs-index / text-ar) ----------

/** Greedy subsequence match of `term` in `norm`: matched positions, or null. */
function subsequence(norm: string, term: string): number[] | null {
  const at: number[] = [];
  let from = 0;
  for (const c of term) {
    const i = norm.indexOf(c, from);
    if (i < 0) return null;
    at.push(i);
    from = i + 1;
  }
  return at;
}

/** US QWERTY ⇄ Arabic 101, unshifted keys only (text-ar `swap_layout` handles more). */
const LAYOUT_KEYS: [string, string][] = [
  ["`", "ذ"], ["q", "ض"], ["w", "ص"], ["e", "ث"], ["r", "ق"], ["t", "ف"], ["y", "غ"], ["u", "ع"], ["i", "ه"],
  ["o", "خ"], ["p", "ح"], ["[", "ج"], ["]", "د"], ["a", "ش"], ["s", "س"], ["d", "ي"], ["f", "ب"], ["g", "ل"],
  ["h", "ا"], ["j", "ت"], ["k", "ن"], ["l", "م"], [";", "ك"], ["'", "ط"], ["z", "ئ"], ["x", "ء"], ["c", "ؤ"],
  ["v", "ر"], ["b", "لا"], ["n", "ى"], ["m", "ة"], [",", "و"], [".", "ز"], ["/", "ظ"],
];
const TO_ARABIC = new Map(LAYOUT_KEYS);
const TO_LATIN = new Map(LAYOUT_KEYS.map(([en, ar]) => [ar, en] as [string, string]));

function swapLayout(word: string): string | null {
  const latin = /[a-z]/i.test(word);
  const arabic = /[\u0600-\u06ff]/.test(word);
  if (latin === arabic) return null;
  let out = "";
  if (latin) {
    for (const c of word.toLowerCase()) out += TO_ARABIC.get(c) ?? c;
  } else {
    for (let i = 0; i < word.length; i++) {
      if (word.startsWith("لا", i)) { out += "b"; i++; continue; }
      out += TO_LATIN.get(word[i]) ?? word[i];
    }
  }
  return out !== word ? out : null;
}

/** The query with its plain words (not filters) retyped on the other layout, or null. */
function layoutSwapped(query: string): string | null {
  let changed = false;
  const out = query.replace(/[^\s"]+/g, (word) => {
    if (word.includes(":")) return word;
    const neg = /^[-!]/.test(word) ? word[0] : "";
    const swapped = swapLayout(word.slice(neg.length));
    if (!swapped) return word;
    changed = true;
    return neg + swapped;
  });
  return changed ? out : null;
}

/** "Did you mean": the other keyboard layout, else the `fuzzy:` switch, if it finds results. */
function suggest(query: string, p: Parsed, limit: number, scope: string | null): SearchSuggestion | null {
  const candidates: [string | null, SearchSuggestion["kind"]][] = [
    [layoutSwapped(query), "layout"],
    [p.fuzzyAll || p.fuzzy.length || !p.terms.length ? null : `fuzzy: ${query.trimStart()}`, "fuzzy"],
  ];
  for (const [candidate, kind] of candidates) {
    if (!candidate) continue;
    const r = search(candidate, limit, 0, scope, false);
    if (!r.error && r.items.length) return { query: candidate, kind, count: r.items.length };
  }
  return null;
}

function search(query: string, limit: number, requestId: number, scope: string | null, withSuggestion = true): SearchResponse {
  const t0 = performance.now();
  let p: Parsed | null = null;
  const done = (items: ResultItem[], truncated: boolean, error: QueryError | null = null): SearchResponse => ({
    requestId, items, truncated, error, cancelled: false,
    suggestion: withSuggestion && p && !error && !items.length && limit > 0 ? suggest(query, p, limit, scope) : null,
    elapsedMs: Math.round((performance.now() - t0) * 10) / 10,
  });
  if (!query.trim()) return done([], false);
  try {
    p = parse(query);
  } catch (e) {
    if (e instanceof MockParseError) return done([], false, e.err);
    throw e;
  }
  if (!index) {
    p = null; // no suggestion before the index is ready
    return done([], false);
  }

  const norm = (t: string) => normalizeWithMap(t).norm;
  const terms = (p.fuzzyAll ? [] : p.terms).map(norm).filter(Boolean);
  const fuzzy = [...p.fuzzy, ...(p.fuzzyAll ? p.terms : [])].map(norm).filter(Boolean);
  const negated = p.negated.map((t) => normalizeWithMap(t).norm).filter(Boolean);
  const hits: { e: MockEntry; score: number; highlights: Range16[] }[] = [];
  for (const e of index) {
    const it = e.item;
    if (scope && !under(`${it.parent}\\${it.name}`, scope)) continue;
    if (p.kinds && !kindMatches(p.kinds, it)) continue;
    if (p.exts && !p.exts.includes(it.ext)) continue;
    if (p.size && !sizeMatches(p.size, it.size)) continue;
    if (negated.some((n) => e.norm.includes(n))) continue;
    const highlights: Range16[] = [];
    let ok = true;
    let score = 0;
    for (const t of terms) {
      const at = e.norm.indexOf(t);
      if (at < 0) { ok = false; break; }
      if (at === 0) score -= 1000;
      highlights.push(toOriginalRange(e, at, at + t.length));
    }
    if (!ok) continue;
    // Fuzzy terms: a subsequence; tighter matches rank first.
    for (const f of fuzzy) {
      const at = subsequence(e.norm, f);
      if (!at) { ok = false; break; }
      score += (at[at.length - 1] - at[0]) * 100 - (at[0] === 0 ? 1000 : 0);
      for (const i of at) highlights.push(toOriginalRange(e, i, i + 1));
    }
    if (!ok) continue;
    hits.push({ e, score: score + e.norm.length + (it.isDir ? -5 : 0), highlights: mergeRanges(highlights) });
  }
  hits.sort((a, b) => a.score - b.score || a.e.item.name.localeCompare(b.e.item.name));
  const items = hits.slice(0, limit).map((h) => ({ ...h.e.item, highlights: h.highlights }));
  return done(items, hits.length > limit);
}

function mergeRanges(r: Range16[]): Range16[] {
  r.sort((a, b) => a[0] - b[0]);
  const out: Range16[] = [];
  for (const x of r) {
    const last = out[out.length - 1];
    if (last && x[0] <= last[1]) last[1] = Math.max(last[1], x[1]);
    else out.push([x[0], x[1]]);
  }
  return out;
}

function findEntry(id: number) {
  if (staleOnce && index) {
    staleOnce = false;
    bumpGeneration(index);
  }
  const e = index?.find((x) => x.item.id === id);
  if (!e) throw Math.floor(id / GEN_UNIT) !== generation ? STALE_ID_ERROR : `no entry with id ${id}`;
  return e.item;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

// ---------- previews ----------

const CODE_LANGUAGES: Record<string, string> = {
  rs: "rust", ts: "typescript", tsx: "typescript", js: "javascript", py: "python", go: "go", json: "json",
};
const DOCUMENTS = new Set(["docx", "xlsx", "pptx", "odt", "epub", "html"]);
const IMAGES: Record<string, string> = {
  png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg", gif: "image/gif", svg: "image/svg+xml",
};
let latestPreview = 0;

const SAMPLE_CODE: Record<string, (stem: string) => string[]> = {
  rust: (stem) => [
    `//! ${stem}: وحدة تجريبية (demo module)`,
    "use std::collections::HashMap;",
    "",
    "/// يحسب عدد الكلمات في التقرير.",
    "pub fn count_words(text: &str) -> HashMap<String, usize> {",
    "    let mut counts = HashMap::new();",
    "    for word in text.split_whitespace() {",
    "        *counts.entry(word.to_lowercase()).or_insert(0) += 1;",
    "    }",
    "    counts",
    "}",
    "",
    "fn main() {",
    '    let report = "التقرير السنوي 2026";',
    "    let n = count_words(report).len();",
    '    println!("{} words, ratio {}", n, 0.75);',
    "    /* block",
    "       comment */",
    "}",
  ],
  typescript: (stem) => [
    `// ${stem} — مثال`,
    'import { createSignal } from "solid-js";',
    "",
    "export interface Report {",
    "  title: string;",
    "  pages: number;",
    "}",
    "",
    "/** التقرير الافتراضي */",
    'const fallback: Report = { title: "تقرير المبيعات", pages: 12 };',
    "",
    "export function useReport(initial = fallback) {",
    "  const [report, setReport] = createSignal(initial);",
    "  return { report, rename: (title: string) => setReport({ ...report(), title }) };",
    "}",
  ],
  javascript: (stem) => [
    `// ${stem}`,
    "const items = [1, 2, 3];",
    "for (const x of items) {",
    "  if (x > 1) console.log(`item ${x}`);",
    "}",
  ],
  python: (stem) => [
    `# ${stem}: ملخص التقرير`,
    "import json",
    "",
    "def summarize(path: str) -> dict:",
    '    """Return word counts for the report."""',
    '    with open(path, encoding="utf-8") as f:',
    "        words = f.read().split()",
    '    return {"words": len(words), "ratio": 0.5}',
    "",
    'if __name__ == "__main__":',
    '    print(summarize("التقرير.txt"))',
  ],
  go: (stem) => [
    `// ${stem}`,
    "package main",
    "",
    'import "fmt"',
    "",
    "func main() {",
    "\tfor i := 0; i < 3; i++ {",
    '\t\tfmt.Println("التقرير", i)',
    "\t}",
    "}",
  ],
  json: () => [
    "{",
    '  "title": "تقرير المبيعات",',
    '  "year": 2026,',
    '  "final": true,',
    '  "tags": ["مبيعات", "report"]',
    "}",
  ],
};

function sampleText(stem: string, lines: number): string {
  const out = [
    `# ${stem}`,
    "",
    "هذا ملف تجريبي يعرض لوحة المعاينة. يحتوي على نص عربي مُشَكَّل مثل: التَّقْرِيرُ السَّنَوِيُّ لِلْمَبِيعَاتِ.",
    "This line is English: the quarterly report is attached below.",
    "سطر مختلط: Report رقم ١٢٣ للعام 2026 — أُعِدَّ التقرير في الرياض.",
    "",
  ];
  for (let i = 1; out.length < lines; i++) {
    out.push(i % 3 === 0
      ? `Line ${i}: notes about ${stem} and the report totals.`
      : `السطر ${i}: ملاحظات حول ${stem} وأرقام التقرير والمحاضرة.`);
  }
  return out.join("\n") + "\n";
}

/** A 1600×1000 SVG stand-in for an image (the real app serves the file via athrix-preview). */
function placeholderImage(name: string): string {
  const svg = '<svg xmlns="http://www.w3.org/2000/svg" width="1600" height="1000" viewBox="0 0 1600 1000">' +
    '<defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#93293A"/>' +
    '<stop offset="1" stop-color="#E0D2BE"/></linearGradient></defs><rect width="1600" height="1000" fill="url(#g)"/>' +
    '<circle cx="1180" cy="330" r="150" fill="#F5EFE6" opacity=".7"/>' +
    '<path d="M0 1000 520 470l330 300 250-200 500 430Z" fill="#4A0F1A" opacity=".55"/>' +
    `<text x="80" y="140" font-family="sans-serif" font-size="64" fill="#FBF8F3">${name.replace(/[<&>]/g, "")}</text></svg>`;
  return `data:image/svg+xml;charset=utf-8,${encodeURIComponent(svg)}`;
}

function previewBody(it: Omit<ResultItem, "highlights">, maxChars: number): PreviewBody {
  const path = `${it.parent}\\${it.name}`;
  if (it.isDir) {
    const children = (index ?? []).filter((e) => e.item.parent === path).map((e) => e.item);
    children.sort((a, b) => Number(b.isDir) - Number(a.isDir) || a.name.localeCompare(b.name));
    return {
      kind: "folder", childCount: children.length,
      children: children.slice(0, 200).map((c) => ({ name: c.name, isDir: c.isDir })),
    };
  }
  const stem = it.ext ? it.name.slice(0, -(it.ext.length + 1)) : it.name;
  const cut = (text: string) => ({ text: text.slice(0, maxChars), truncated: text.length > maxChars });
  if (IMAGES[it.ext]) return { kind: "image", src: placeholderImage(it.name), mime: IMAGES[it.ext] };
  if (it.ext === "pdf") return { kind: "unsupported", reason: "pdf", message: "PDF preview is not available yet" };
  const language = CODE_LANGUAGES[it.ext];
  if (language) {
    return { kind: "code", ...cut(SAMPLE_CODE[language](stem).join("\n") + "\n"), encoding: "UTF-8", language };
  }
  if (DOCUMENTS.has(it.ext)) {
    const body = [
      stem,
      "مقدمة: يتناول هذا المستند نتائج التَّقْرِير الربعي ويعرض الأرقام الرئيسة.",
      "The document summarizes the quarterly report and its main figures.",
      ...Array.from({ length: 40 }, (_, i) => `الفقرة ${i + 1}: تفاصيل إضافية حول ${stem} — paragraph ${i + 1}.`),
    ].join("\n");
    return { kind: "document", ...cut(body), format: it.ext, title: stem, author: "سارة أحمد" };
  }
  if (["md", "txt", "csv"].includes(it.ext)) {
    const big = it.ext === "txt";
    return { kind: "text", ...cut(sampleText(stem, big ? 400 : 60)), encoding: big ? "windows-1256" : "UTF-8" };
  }
  return { kind: "unsupported", reason: "format", message: `no preview for .${it.ext || "?"} files` };
}

/** Dispatches one command. Rejections carry an English string, like Tauri commands. */
export async function handle<C extends CommandName>(cmd: C, rawArgs: CommandArgs<C> | undefined): Promise<CommandResult<C>> {
  const args = (rawArgs ?? {}) as Record<string, unknown>;
  let result: unknown;
  switch (cmd) {
    case "app_info":
      result = { name: "Athrix (mock)", version: "0.0.1-mock" };
      break;
    case "index_status":
      result = { ...status };
      break;
    case "start_indexing":
      startIndexing(args.roots as string[] | undefined);
      break;
    case "list_volumes":
      await sleep(80);
      result = VOLUMES.map((v) => ({ ...v }));
      break;
    case "get_index_roots":
      result = { wholeComputer: chosenRoots === null, roots: resolvedRoots() } satisfies IndexRoots;
      break;
    case "set_index_roots": {
      const roots = args.roots as string[] | null;
      if (roots !== null && roots.length === 0) throw "choose at least one drive or folder to index";
      chosenRoots = roots;
      const next = resolvedRoots();
      const same = next.length === status.roots.length && next.every((x, i) => pathKey(x) === pathKey(status.roots[i]));
      if (!same) {
        // Like the Rust queue (latest wins), simplified: restart once the running walk ends.
        if (status.state === "indexing") setTimeout(() => startIndexing(resolvedRoots()), INDEX_DELAY_MS + 50);
        else startIndexing(next);
      }
      result = { wholeComputer: roots === null, roots: next } satisfies IndexRoots;
      break;
    }
    case "search": {
      const requestId = args.requestId as number;
      latestRequest = Math.max(latestRequest, requestId);
      const query = args.query as string;
      // Variable, query-dependent latency so responses can arrive out of order (tests stale-drop).
      let h = 0;
      for (let i = 0; i < query.length; i++) h = (h * 31 + query.charCodeAt(i)) | 0;
      await sleep(2 + (Math.abs(h) % 25));
      if (requestId < latestRequest) {
        result = { requestId, items: [], truncated: false, elapsedMs: 0, error: null, cancelled: true };
      } else {
        const scope = ((args.scope as string | null | undefined) ?? "").trim();
        if (scope && !/^([a-z]:([\\/].*)?|[\\/].*)$/i.test(scope)) {
          throw `invalid search scope: "${scope}" is not an absolute path or a drive like D:`;
        }
        result = search(query, args.limit as number, requestId, scope || null);
      }
      break;
    }
    case "open_entry":
      console.info("[mock] open_entry", findEntry(args.id as number));
      break;
    case "reveal_entry":
      console.info("[mock] reveal_entry", findEntry(args.id as number));
      break;
    case "preview_entry": {
      const requestId = args.requestId as number;
      latestPreview = Math.max(latestPreview, requestId);
      const it = findEntry(args.id as number);
      await sleep(20 + (it.id % 7) * 15);
      if (requestId < latestPreview) {
        result = { requestId, cancelled: true, preview: null } satisfies PreviewResponse;
        break;
      }
      const { name, isDir, size, modified, ext } = it;
      result = {
        requestId, cancelled: false,
        preview: {
          entry: { name, path: `${it.parent}\\${name}`, isDir, size, modified, ext },
          body: previewBody(it, (args.maxChars as number | undefined) ?? 65536),
        },
      } satisfies PreviewResponse;
      break;
    }
    case "entry_path": {
      const it = findEntry(args.id as number);
      result = `${it.parent}\\${it.name}`;
      break;
    }
    default:
      throw `unknown command ${String(cmd)}`;
  }
  return result as CommandResult<C>;
}

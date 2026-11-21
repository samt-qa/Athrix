// In-browser stand-in for the Rust backend (docs/IPC-CONTRACT.md), used whenever the UI runs
// outside Tauri. It is deliberately small but honest about the contract: UTF-16 highlight ranges,
// requestId echo + cancellation of older requests, `items: []` until the index is ready,
// QueryError with kind/start/end/suggestion, and `index://status` events.
//
// The fake index is deterministic (seeded PRNG): the same names, folders and sizes on every load.
// Test switches (URL query string): `?mockEntries=N` (default 4000), `?mockIndexError`.
import type {
  CommandArgs, CommandName, CommandResult, IndexStatus, QueryError, Range16, ResultItem, SearchResponse,
} from "./api";
import { categoryOf, type FileCategory } from "./fileKinds";

const params = new URLSearchParams(window.location.search);
const ENTRY_COUNT = clampInt(Number(params.get("mockEntries") ?? 4000), 10, 200_000);
const INDEX_DELAY_MS = 1200;
const HOME = "C:\\Users\\Sara";

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
        name: parts[i], parent: [HOME, ...parts.slice(0, i)].join("\\"), isDir: true, size: null,
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
      name: `${stem}.${ext}`, parent: `${HOME}\\${pick(FOLDERS)}`, isDir: false,
      size: Math.max(1, Math.floor(SIZE_SCALE[cat] * Math.pow(rnd() * 2, 3))),
      modified: nowSec - age, ext,
    });
  }
  return entries;
}

// ---------- normalization (approximation of nass: case, harakat, tatweel, hamza, ya, digits) ----------

const HAMZA: Record<string, string> = { "أ": "ا", "إ": "ا", "آ": "ا", "ٱ": "ا", "ى": "ي" };

function normalizeWithMap(s: string): { norm: string; map: number[] } {
  let norm = "";
  const map: number[] = [];
  for (let i = 0; i < s.length; i++) {
    const c = s[i];
    const code = c.charCodeAt(0);
    // Harakat, superscript alef, Quranic marks and tatweel vanish.
    if ((code >= 0x064b && code <= 0x065f) || code === 0x0670 || code === 0x0640) continue;
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

/** Maps a [start, end) range of the normalized name back to UTF-16 units of the original,
 *  stretching the end over trailing diacritics so marks stay with their letter. */
function toOriginalRange(e: MockEntry, start: number, end: number): Range16 {
  const name = e.item.name;
  const from = e.map[start];
  let to = e.map[end - 1] + 1;
  while (to < name.length) {
    const code = name.charCodeAt(to);
    if ((code >= 0x064b && code <= 0x065f) || code === 0x0670 || code === 0x0640) to++;
    else break;
  }
  return [from, to];
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
const UNSUPPORTED = new Set(["content", "sem", "tag", "regex", "fuzzy", "dupe", "similar", "dc", "da"]);
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
  const out: Parsed = { terms: [], negated: [], kinds: null, exts: null, size: null };
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

function startIndexing(roots?: string[]) {
  if (status.state === "indexing") return;
  const r = roots?.length ? roots : [HOME];
  setStatus({ state: "indexing", entries: 0, roots: r, elapsedMs: null, error: null });
  const t0 = performance.now();
  setTimeout(() => {
    if (params.has("mockIndexError")) {
      setStatus({ ...status, state: "error", error: "access denied: C:\\Users\\Sara (mock)" });
      return;
    }
    index = buildIndex(ENTRY_COUNT);
    setStatus({
      state: "ready", entries: index.length, roots: r, elapsedMs: Math.round(performance.now() - t0), error: null,
    });
  }, INDEX_DELAY_MS);
}

function search(query: string, limit: number, requestId: number): SearchResponse {
  const t0 = performance.now();
  const done = (items: ResultItem[], truncated: boolean, error: QueryError | null = null): SearchResponse => ({
    requestId, items, truncated, error, cancelled: false, elapsedMs: Math.round((performance.now() - t0) * 10) / 10,
  });
  if (!query.trim()) return done([], false);
  let p: Parsed;
  try {
    p = parse(query);
  } catch (e) {
    if (e instanceof MockParseError) return done([], false, e.err);
    throw e;
  }
  if (!index) return done([], false);

  const terms = p.terms.map((t) => normalizeWithMap(t).norm).filter(Boolean);
  const negated = p.negated.map((t) => normalizeWithMap(t).norm).filter(Boolean);
  const hits: { e: MockEntry; score: number; highlights: Range16[] }[] = [];
  for (const e of index) {
    const it = e.item;
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
  const e = index?.find((x) => x.item.id === id);
  if (!e) throw `no entry with id ${id}`;
  return e.item;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

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
        result = search(query, args.limit as number, requestId);
      }
      break;
    }
    case "open_entry":
      console.info("[mock] open_entry", findEntry(args.id as number));
      break;
    case "reveal_entry":
      console.info("[mock] reveal_entry", findEntry(args.id as number));
      break;
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

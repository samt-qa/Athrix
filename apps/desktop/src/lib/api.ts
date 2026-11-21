// Typed client for the Rust commands in docs/IPC-CONTRACT.md (v1).
// Outside Tauri (a plain browser: `npx vite`, reviewers, UI work) every call goes to an in-memory
// mock backend instead, so the whole UI can be exercised without the desktop shell.
import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export type IndexState = "idle" | "indexing" | "ready" | "error";

export interface IndexStatus {
  state: IndexState;
  /** Indexed entries (0 during the first indexing). */
  entries: number;
  roots: string[];
  /** Duration of the last completed indexing. */
  elapsedMs: number | null;
  /** English message when state = "error". */
  error: string | null;
}

/** A UTF-16 [start, end) range, ready for `String.prototype.slice`. */
export type Range16 = [number, number];

export interface ResultItem {
  /** EntryId: the only handle the UI sends back; paths never travel UI -> Rust. */
  id: number;
  name: string;
  /** Full path of the parent folder. */
  parent: string;
  isDir: boolean;
  /** Bytes; null for folders. */
  size: number | null;
  /** Unix seconds. */
  modified: number | null;
  /** Lower case, no dot, "" when none. */
  ext: string;
  /** Match ranges inside `name`. */
  highlights: Range16[];
  /** What matched (task 4.7): always "name" for name-only searches. Optional for older backends. */
  matched?: MatchSource;
  /** An excerpt of the content when the content matched, else null. */
  snippet?: ContentSnippet | null;
}

export type MatchSource = "name" | "content" | "both";

/** A short excerpt of a file's text around the words that matched in its content. */
export interface ContentSnippet {
  /** Original text (with its diacritics). */
  text: string;
  /** Matched words inside `text`, UTF-16, sorted and non-overlapping. */
  highlights: Range16[];
}

export type ContentState = "disabled" | "idle" | "running" | "paused" | "stopped" | "error";
export type ContentPauseReason = "user" | "battery" | "busy";

/** Background content indexing (task 4.5): `content_status` and the `content://status` event. */
export interface ContentStatus {
  state: ContentState;
  /** Why it is paused, when state = "paused". */
  pausedBy: ContentPauseReason[];
  /** Queue items handled (indexed + updated + failed). */
  processed: number;
  /** processed + what is left in the queue. */
  total: number;
  /** Documents in the content index. */
  indexed: number;
  failed: number;
  docsPerSecond: number;
  /** English message when state = "error". */
  error: string | null;
}

export interface QueryError {
  /** English fallback; the UI localizes by `kind`. */
  message: string;
  /** Stable ParseErrorKind code, e.g. "UnknownFilter". */
  kind: string;
  /** Error span in the query, UTF-16 units. */
  start: number;
  end: number;
  suggestion: string | null;
}

export interface SearchResponse {
  requestId: number;
  items: ResultItem[];
  truncated: boolean;
  elapsedMs: number;
  error: QueryError | null;
  cancelled: boolean;
  /** "Did you mean": only when a valid query found nothing and a rewrite of it finds something. */
  suggestion?: SearchSuggestion | null;
}

/** A query to offer instead of one that found nothing. */
export interface SearchSuggestion {
  /** The full replacement query (as sent, i.e. including the chip's `type:` filter). */
  query: string;
  /** "layout": words retyped on the other keyboard layout; "fuzzy": `fuzzy:` put in front. */
  kind: "layout" | "fuzzy";
  /** Results it finds (1..limit). */
  count: number;
}

export type VolumeKind = "fixed" | "removable" | "network" | "unknown";

/** A drive of this computer (`list_volumes`). */
export interface Volume {
  /** Root folder, e.g. "C:\\" or "/media/usb". */
  path: string;
  /** Short name: "C:" on Windows, the path elsewhere. */
  name: string;
  /** Volume label ("Data"), if any. */
  label: string | null;
  kind: VolumeKind;
}

/** The indexed-roots setting (`get_index_roots` / `set_index_roots`). */
export interface IndexRoots {
  /** "Whole computer": every fixed drive, re-resolved at each start. */
  wholeComputer: boolean;
  /** The folders indexed under that choice now. */
  roots: string[];
}

/** Header of a preview: name, full path and metadata of the entry. */
export interface PreviewEntry {
  name: string;
  path: string;
  isDir: boolean;
  size: number | null;
  modified: number | null;
  ext: string;
}

export type UnsupportedReason = "pdf" | "format" | "tooLarge" | "encrypted" | "corrupt" | "missing" | "unreadable";

/** What the preview pane shows, by `kind`. Text is cut at `maxChars` (`truncated`). */
export type PreviewBody =
  | { kind: "text"; text: string; truncated: boolean; encoding: string | null }
  | { kind: "code"; text: string; truncated: boolean; encoding: string | null; language: string }
  | { kind: "document"; text: string; truncated: boolean; format: string; title: string | null; author: string | null }
  /** `src`: an `athrix-preview` URL valid for this preview only (never a file path). */
  | { kind: "image"; src: string; mime: string }
  | { kind: "folder"; childCount: number; children: { name: string; isDir: boolean }[] }
  | { kind: "unsupported"; reason: UnsupportedReason; message: string };

export interface Preview {
  entry: PreviewEntry;
  body: PreviewBody;
}

export interface PreviewResponse {
  requestId: number;
  /** A newer preview request cancelled this one (`preview` is then null). */
  cancelled: boolean;
  preview: Preview | null;
}

export interface AppInfo {
  name: string;
  version: string;
}

/** The command table of the contract: name -> [args, result]. */
export interface Commands {
  app_info: [undefined, AppInfo];
  index_status: [undefined, IndexStatus];
  start_indexing: [{ roots?: string[] }, void];
  list_volumes: [undefined, Volume[]];
  get_index_roots: [undefined, IndexRoots];
  /** `roots: null` = whole computer. */
  set_index_roots: [{ roots: string[] | null }, IndexRoots];
  /** `scope`: a root from `IndexStatus.roots` (e.g. "D:\\") to search only there; null = everywhere. */
  /** `searchContent`: plain words also search file contents (a `content:` filter always does). */
  search: [{ query: string; limit: number; requestId: number; scope: string | null; searchContent?: boolean }, SearchResponse];
  open_entry: [{ id: number }, void];
  reveal_entry: [{ id: number }, void];
  entry_path: [{ id: number }, string];
  /** `maxChars`: text characters to return (default 65536). Each call cancels the previous one. */
  preview_entry: [{ id: number; requestId: number; maxChars?: number }, PreviewResponse];
  content_status: [undefined, ContentStatus];
  /** Pause by the user (other reasons, battery or a busy machine, come from the backend). */
  content_pause: [undefined, void];
  content_resume: [undefined, void];
  /** Persisted on the Rust side; disabled = state "disabled". */
  set_content_indexing: [{ enabled: boolean }, void];
}
export type CommandName = keyof Commands;
export type CommandArgs<C extends CommandName> = Commands[C][0];
export type CommandResult<C extends CommandName> = Commands[C][1];

export const INDEX_STATUS_EVENT = "index://status";
export const CONTENT_STATUS_EVENT = "content://status";

/** True when running against the in-browser mock instead of Rust. */
export const usingMock = !isTauri();

type MockModule = typeof import("./mockBackend");
let mockModule: Promise<MockModule> | null = null;
// Lazy import: the mock (and its fake index) is never loaded inside the desktop app.
const mock = () => (mockModule ??= import("./mockBackend"));

/**
 * Calls a contract command. Rejects with an English message string when the command fails
 * (the contract's error shape), so callers decide how to surface it.
 */
export async function call<C extends CommandName>(cmd: C, args?: CommandArgs<C>): Promise<CommandResult<C>> {
  if (usingMock) return (await mock()).handle(cmd, args);
  return invoke<CommandResult<C>>(cmd, args as Record<string, unknown> | undefined);
}

/** Subscribes to `index://status`; resolves to an unsubscribe function. */
export async function onIndexStatus(handler: (status: IndexStatus) => void): Promise<() => void> {
  if (usingMock) return (await mock()).onIndexStatus(handler);
  return listen<IndexStatus>(INDEX_STATUS_EVENT, (e) => handler(e.payload));
}

/** Subscribes to `content://status` (at most twice a second); resolves to an unsubscribe function. */
export async function onContentStatus(handler: (status: ContentStatus) => void): Promise<() => void> {
  if (usingMock) return (await mock()).onContentStatus(handler);
  return listen<ContentStatus>(CONTENT_STATUS_EVENT, (e) => handler(e.payload));
}

// Thin named wrappers, one per command.
export const api = {
  appInfo: () => call("app_info"),
  indexStatus: () => call("index_status"),
  startIndexing: (roots?: string[]) => call("start_indexing", roots ? { roots } : {}),
  listVolumes: () => call("list_volumes"),
  getIndexRoots: () => call("get_index_roots"),
  setIndexRoots: (roots: string[] | null) => call("set_index_roots", { roots }),
  search: (query: string, limit: number, requestId: number, scope: string | null = null, searchContent = false) =>
    call("search", { query, limit, requestId, scope, searchContent }),
  openEntry: (id: number) => call("open_entry", { id }),
  revealEntry: (id: number) => call("reveal_entry", { id }),
  entryPath: (id: number) => call("entry_path", { id }),
  previewEntry: (id: number, requestId: number, maxChars?: number) =>
    call("preview_entry", maxChars === undefined ? { id, requestId } : { id, requestId, maxChars }),
  contentStatus: () => call("content_status"),
  contentPause: () => call("content_pause"),
  contentResume: () => call("content_resume"),
  setContentIndexing: (enabled: boolean) => call("set_content_indexing", { enabled }),
};

/**
 * What `open_entry` / `reveal_entry` / `entry_path` / `preview_entry` reject with when the id comes from an older
 * index generation (a re-index ran since the search). The UI should search again.
 */
export const STALE_ID_ERROR = "this result is out of date; search again";

/** True for the stale-id rejection (matched loosely, in case the wording gains context). */
export function isStaleIdError(err: unknown): boolean {
  return errorMessage(err).toLowerCase().includes("out of date");
}

/** Normalizes whatever a rejected command carries into a message. */
export function errorMessage(err: unknown): string {
  if (typeof err === "string") return err;
  if (err instanceof Error) return err.message;
  return String(err);
}

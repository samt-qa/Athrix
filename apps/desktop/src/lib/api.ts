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
  search: [{ query: string; limit: number; requestId: number; scope: string | null }, SearchResponse];
  open_entry: [{ id: number }, void];
  reveal_entry: [{ id: number }, void];
  entry_path: [{ id: number }, string];
}
export type CommandName = keyof Commands;
export type CommandArgs<C extends CommandName> = Commands[C][0];
export type CommandResult<C extends CommandName> = Commands[C][1];

export const INDEX_STATUS_EVENT = "index://status";

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

// Thin named wrappers, one per command.
export const api = {
  appInfo: () => call("app_info"),
  indexStatus: () => call("index_status"),
  startIndexing: (roots?: string[]) => call("start_indexing", roots ? { roots } : {}),
  listVolumes: () => call("list_volumes"),
  getIndexRoots: () => call("get_index_roots"),
  setIndexRoots: (roots: string[] | null) => call("set_index_roots", { roots }),
  search: (query: string, limit: number, requestId: number, scope: string | null = null) =>
    call("search", { query, limit, requestId, scope }),
  openEntry: (id: number) => call("open_entry", { id }),
  revealEntry: (id: number) => call("reveal_entry", { id }),
  entryPath: (id: number) => call("entry_path", { id }),
};

/**
 * What `open_entry` / `reveal_entry` / `entry_path` reject with when the id comes from an older
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

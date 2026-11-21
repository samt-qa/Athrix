// History, saved searches and favorites (task 5.3): the lists the UI shows when the search box is
// empty, kept in sync with the backend. One copy per window; each window refreshes when its
// library becomes visible, so a search run in the popup shows up in the main window too.
import { batch, createSignal } from "solid-js";
import { api, type Favorite, type HistoryEntry, type SavedSearch } from "./api";
import { storedFlag } from "./storage";

/** Recent searches fetched (the panel shows fewer). */
const HISTORY_LIMIT = 20;

const [history, setHistory] = createSignal<HistoryEntry[]>([]);
const [saved, setSaved] = createSignal<SavedSearch[]>([]);
const [favorites, setFavorites] = createSignal<Favorite[]>([]);
/** "Keep a search history" (settings): when off nothing is recorded and recents are hidden. */
const [historyEnabled, setHistoryEnabledFlag] = storedFlag("historyEnabled", true);

export { favorites, history, historyEnabled, saved };

/** Reloads the three lists. A backend without these commands leaves them empty (logged once). */
export async function refreshLibrary(): Promise<void> {
  const [h, s, f] = await Promise.allSettled([api.historyList(HISTORY_LIMIT), api.savedList(), api.favoriteList()]);
  batch(() => {
    if (h.status === "fulfilled") setHistory(h.value);
    if (s.status === "fulfilled") setSaved(s.value);
    if (f.status === "fulfilled") setFavorites(f.value);
  });
  const failed = [h, s, f].find((r) => r.status === "rejected");
  if (failed && !warned) {
    warned = true;
    console.warn("library unavailable", failed.reason);
  }
}
let warned = false;

export function setHistoryEnabled(on: boolean): void {
  setHistoryEnabledFlag(on);
}

/** Records a search (Enter, or a result opened). No-op when history is off or the query is blank. */
export async function recordSearch(query: string, scope: string | null): Promise<void> {
  const q = query.trim();
  if (!q || !historyEnabled()) return;
  try {
    await api.historyAdd(q, scope || null);
  } catch (err) {
    console.warn("history_add failed", err);
  }
}

export async function removeHistory(id: number): Promise<void> {
  setHistory((list) => list.filter((h) => h.id !== id));
  try {
    await api.historyRemove(id);
  } finally {
    void refreshLibrary();
  }
}

export async function clearHistory(): Promise<void> {
  await api.historyClear();
  setHistory([]);
}

export async function saveSearch(name: string, query: string, scope: string | null): Promise<SavedSearch> {
  const s = await api.savedCreate(name, query, scope || null);
  setSaved((list) => [s, ...list.filter((x) => x.id !== s.id)]);
  return s;
}

export async function renameSaved(id: number, name: string): Promise<void> {
  const s = await api.savedRename(id, name);
  setSaved((list) => list.map((x) => (x.id === id ? s : x)));
}

export async function deleteSaved(id: number): Promise<void> {
  await api.savedDelete(id);
  setSaved((list) => list.filter((x) => x.id !== id));
}

/** Adds a result to the favorites; resolves to the favorite (also when it already was one). */
export async function addFavorite(resultId: number): Promise<{ favorite: Favorite; already: boolean }> {
  const f = await api.favoriteAdd(resultId);
  const already = favorites().some((x) => samePath(x.path, f.path));
  if (!already) setFavorites((list) => [f, ...list]);
  return { favorite: f, already };
}

export async function removeFavorite(path: string): Promise<void> {
  await api.favoriteRemove(path);
  setFavorites((list) => list.filter((x) => !samePath(x.path, path)));
}

const samePath = (a: string, b: string) =>
  a.replace(/\\/g, "/").replace(/\/+$/, "").toLowerCase() === b.replace(/\\/g, "/").replace(/\/+$/, "").toLowerCase();

/**
 * The query for "search in this folder": the folder filter alone, quoted (a path may hold
 * spaces; Windows paths cannot hold `"`), with a trailing space so the user can type words after it.
 */
export function folderQuery(path: string): string {
  return `infolder:"${path.replace(/"/g, "")}" `;
}

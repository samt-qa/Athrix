import { batch, createEffect, createMemo, createResource, createSignal, Match, on, onCleanup, onMount, Show, Switch } from "solid-js";
import { locale, localeDir, t } from "./i18n";
import {
  api, errorMessage, isStaleIdError, onIndexStatus, onSearchIn, onUpdateAvailable, usingMock, type UpdateInfo, type IndexStatus, type QueryError, type SearchResponse,
  type SearchSuggestion,
} from "./lib/api";
import { applySuggestion, composeQuery, suggestionInput, type ChipKind } from "./lib/query";
import SearchField from "./components/SearchField";
import FilterChips from "./components/FilterChips";
import QueryErrorBar from "./components/QueryErrorBar";
import ResultList, { optionId, type ResultListHandle } from "./components/ResultList";
import { EmptyState, NoResults, NotReady } from "./components/EmptyState";
import HintBar from "./components/HintBar";
import StatusBar from "./components/StatusBar";
import ContentStatusControl from "./components/ContentStatusControl";
import ScopeSelect from "./components/ScopeSelect";
import SettingsDialog, { type SettingsSection } from "./components/SettingsDialog";
import Onboarding, { welcomeDone } from "./components/Onboarding";
import LibraryPanel, { type LibraryHandle } from "./components/LibraryPanel";
import PromptDialog from "./components/PromptDialog";
import ContextMenu, { type MenuItem } from "./components/ContextMenu";
import PreviewPane, { type PreviewHandle } from "./components/PreviewPane";
import { readSetting, writeSetting } from "./lib/storage";
import { effectiveScope, rootKey, setScope, storedScope } from "./lib/scope";
import { addFavorite, folderQuery, recordSearch, saveSearch } from "./lib/library";
import { searchContent, setSearchContent } from "./lib/searchContent";
import { fitWindowTo, hideWindow, onWindowFocus, type WindowMode } from "./lib/window";
import "./styles/panels.css";

/** Results per query (contract `limit`). Dev-only override for scroll testing: `?limit=5000`. */
const LIMIT = (() => {
  const v = import.meta.env.DEV ? Number(new URLSearchParams(window.location.search).get("limit")) : NaN;
  return Number.isFinite(v) && v > 0 ? Math.floor(Math.min(v, 100_000)) : 200;
})();

/**
 * "Did you mean" is asked (`search_suggest`) this long after an empty result, if no newer
 * keystroke came: its probes (a ranked `fuzzy:` scan) cost more than the search itself, so
 * they run apart from it and only once typing pauses.
 */
const SUGGEST_DELAY_MS = 150;

const LIST_ID = "results";
const ERROR_ID = "query-error";
const PREVIEW_ID = "preview";

/** Preview pane width: remembered, clamped to what the window leaves (design §9.2: 360–520). */
const PREVIEW_MIN = 280;
const PREVIEW_MAX = 900;
const PREVIEW_DEFAULT = 440;
/** Room the search panel keeps next to the pane. */
const PANEL_MIN = 360;

/**
 * The search UI. `mode` "main": the full window (top bar, panel, status bar). `mode` "quick": the
 * quick-search popup (design §9.1): only the panel, inside a transparent frame that the window
 * fits to. In the popup, Esc on an empty field hides it (the first Esc clears the text), and
 * opening a result (Enter, Ctrl+Enter, double-click) hides it; it also hides when it loses focus
 * (Rust side). Each time it is shown the field is focused with its text selected.
 */
export default function App(props: { mode: WindowMode }) {
  const quick = props.mode === "quick";
  let input!: HTMLInputElement;
  let list: ResultListHandle | undefined;

  const [info] = createResource(() => api.appInfo().catch(() => null));

  // ---- index status: subscribe first, then read the current state ----
  const [index, setIndex] = createSignal<IndexStatus | null>(null);
  const loadIndex = async () => {
    let gotEvent = false;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    onCleanup(() => {
      disposed = true;
      unlisten?.();
    });
    try {
      unlisten = await onIndexStatus((s) => {
        gotEvent = true;
        setIndex(s);
      });
      if (disposed) unlisten();
      const s = await api.indexStatus();
      if (!gotEvent) setIndex(s);
      // The backend starts indexing at launch; kick it only if it is still idle.
      if (s.state === "idle") await api.startIndexing();
    } catch (err) {
      console.error("index status unavailable", err);
    }
  };
  onMount(loadIndex);
  const retryIndexing = () => api.startIndexing().catch((e) => showNotice(t("notice.actionFailed", { message: errorMessage(e) })));

  // ---- search ----
  const [text, setText] = createSignal("");
  const [chip, setChip] = createSignal<ChipKind>("all");
  const [response, setResponse] = createSignal<SearchResponse | null>(null);
  const [failure, setFailure] = createSignal<string | null>(null);
  const [selected, setSelected] = createSignal(0);
  const sent = createMemo(() => composeQuery(text(), chip()));
  // Where to search: "" = whole computer, else one indexed root. A remembered drive that is no
  // longer indexed falls back to the whole computer.
  // Roots on an unplugged drive are not offered, and a scope on one falls back to everywhere
  // (it comes back with the drive).
  const roots = () => {
    const offline = (index()?.offline ?? []).map(rootKey);
    return (index()?.roots ?? []).filter((r) => !offline.includes(rootKey(r)));
  };
  const scope = createMemo(() => effectiveScope(storedScope(), index() ? roots() : undefined));

  // Whole numbers: Rust reads requestId as u64. Seeded from the clock so ids keep increasing
  // across a webview reload (or HMR) while the backend stays alive.
  let lastRequest = Date.now();
  // A search is on its way: the list holds `content_snippets` requests meanwhile, since the
  // backend answers them with the query of the last search it finished.
  const [searchPending, setSearchPending] = createSignal(false);
  // "Did you mean" for the shown (empty) response, asked separately after a pause.
  const [suggestion, setSuggestion] = createSignal<SearchSuggestion | null>(null);
  let suggestTimer: ReturnType<typeof setTimeout> | undefined;
  onCleanup(() => clearTimeout(suggestTimer));
  async function askSuggestion(query: string, where: string, withContent: boolean, requestId: number) {
    if (requestId !== lastRequest) return;
    try {
      const s = await api.searchSuggest(query, LIMIT, requestId, where || null, withContent);
      // Same request id as the search it completes: dropped once anything newer was sent.
      if (requestId !== lastRequest || s.requestId !== requestId || s.cancelled) return;
      setSuggestion(s.suggestion);
    } catch (err) {
      console.warn("no suggestion", err);
    }
  }
  async function runSearch(query: string, where: string, withContent = searchContent()) {
    const requestId = ++lastRequest;
    clearTimeout(suggestTimer);
    if (!query.trim()) {
      batch(() => {
        setResponse(null);
        setSuggestion(null);
        setFailure(null);
        setSelected(0);
        setSearchPending(false);
      });
      return;
    }
    setSearchPending(true);
    try {
      const r = await api.search(query, LIMIT, requestId, where || null, withContent);
      // Drop anything older than the latest request, and cancelled ones.
      if (requestId !== lastRequest) return;
      if (r.requestId !== lastRequest || r.cancelled) {
        setSearchPending(false);
        return;
      }
      batch(() => {
        setResponse(r);
        setSuggestion(null);
        setFailure(null);
        setSelected(0);
        setSearchPending(false);
      });
      if (!r.items.length && !r.error) {
        suggestTimer = setTimeout(() => void askSuggestion(query, where, withContent, requestId), SUGGEST_DELAY_MS);
      }
    } catch (err) {
      if (requestId === lastRequest) {
        batch(() => {
          setSearchPending(false);
          setResponse(null);
          setFailure(t("queryError.searchFailed", { message: errorMessage(err) }));
        });
      }
    }
  }
  // No debounce: a search takes a few ms, and stale responses are dropped by requestId.
  // Re-runs when the index becomes ready, so a query typed during indexing fills in, and when
  // the scope or the "search contents too" setting changes.
  createEffect(on([sent, () => index()?.state, scope, searchContent], ([q, , where, withContent]) =>
    void runSearch(q, where, withContent)));

  const items = () => response()?.items ?? [];
  // "Did you mean": the backend rewrote the query as sent; the field gets it without the chip suffix.
  const didYouMean = () => {
    const s = suggestion();
    return s ? { text: suggestionInput(s.query, chip()), kind: s.kind } : null;
  };
  const queryError = () => response()?.error ?? null;
  const current = () => items()[selected()];

  // ---- preview pane (main window only, design §9.2): open state and width are remembered ----
  const [previewOpen, setPreviewOpen] = createSignal(!quick && readSetting("previewOpen") === "1");
  const storedWidth = Number(readSetting("previewWidth"));
  const [previewWidth, setPreviewWidth] = createSignal(
    Number.isFinite(storedWidth) && storedWidth >= PREVIEW_MIN ? Math.min(storedWidth, PREVIEW_MAX) : PREVIEW_DEFAULT,
  );
  let previewPane: PreviewHandle | undefined;
  let mainEl: HTMLElement | undefined;
  function setPreview(open: boolean) {
    setPreviewOpen(open);
    writeSetting("previewOpen", open ? "1" : "0");
  }
  function closePreview() {
    setPreview(false);
    input.focus();
  }
  /** The arrow key pointing at the pane: it sits at the inline end (right in LTR, left in RTL). */
  const towardPane = () => (localeDir(locale()) === "rtl" ? "ArrowLeft" : "ArrowRight");

  // Relative times ("3 minutes ago") refresh every minute.
  const [now, setNow] = createSignal(Date.now());
  const clock = setInterval(() => setNow(Date.now()), 60_000);
  onCleanup(() => clearInterval(clock));

  // ---- transient notices (copied, opening…) ----
  const [notice, setNotice] = createSignal<string | null>(null);
  let noticeTimer: ReturnType<typeof setTimeout> | undefined;
  function showNotice(msg: string) {
    setNotice(msg);
    clearTimeout(noticeTimer);
    noticeTimer = setTimeout(() => setNotice(null), 2500);
  }
  onCleanup(() => clearTimeout(noticeTimer));

  // ---- actions on the selected entry ----
  async function open(i = selected(), reveal = false) {
    const it = items()[i];
    if (!it) return;
    record();
    try {
      if (reveal) {
        await api.revealEntry(it.id);
        showNotice(t("notice.revealing", { name: it.name }));
      } else {
        await api.openEntry(it.id);
        showNotice(t("notice.opening", { name: it.name }));
      }
      // The popup's job is done; on failure it stays open to show the notice.
      if (quick) hideWindow();
    } catch (err) {
      if (isStaleIdError(err)) searchAgain();
      else showNotice(t("notice.actionFailed", { message: errorMessage(err) }));
    }
  }

  // ---- history, saved searches, favorites (task 5.3) ----
  let library: LibraryHandle | undefined;
  const currentQuery = () => sent().trim();
  /** Adds the current search to the history (Enter, or a result opened); never per keystroke. */
  function record() {
    void recordSearch(currentQuery(), scope() || null);
  }

  /** A past or saved search, run again: its text (chip reset) and the scope it ran in. */
  function runFromLibrary(query: string, where: string | null) {
    batch(() => {
      setChip("all");
      setScope(where ?? "");
    });
    setQuery(query);
  }

  const [savePrompt, setSavePrompt] = createSignal<{ query: string; scope: string | null } | null>(null);
  /** "Save this search" (Ctrl+S): asks for a name in the main window; the popup saves it as is. */
  function promptSave() {
    const query = currentQuery();
    if (!query) return;
    const where = scope() || null;
    if (quick) {
      void saveSearch(query, query, where)
        .then(() => showNotice(t("library.savedNotice", { name: query })))
        .catch((err) => showNotice(t("library.actionFailed", { message: errorMessage(err) })));
    } else {
      setSavePrompt({ query, scope: where });
    }
  }

  /** "Add to favorites" (Ctrl+D) on result `i`. */
  async function favorite(i = selected()) {
    const it = items()[i];
    if (!it) return;
    try {
      const { already } = await addFavorite(it.id);
      showNotice(t(already ? "library.alreadyFavorite" : "library.favorited", { name: it.name }));
    } catch (err) {
      if (isStaleIdError(err)) searchAgain();
      else showNotice(t("library.actionFailed", { message: errorMessage(err) }));
    }
  }

  // ---- context menu on a result (right click, Shift+F10, the Menu key) ----
  const [menu, setMenu] = createSignal<{ x: number; y: number; index: number } | null>(null);
  const menuItems = (i: number): MenuItem[] => [
    { label: t("hints.open"), shortcut: "Enter", onSelect: () => void open(i) },
    { label: t("hints.reveal"), shortcut: "Ctrl+Enter", onSelect: () => void open(i, true) },
    { label: t("hints.copy"), shortcut: "Ctrl+C", onSelect: () => void copyPath() },
    { label: t("library.addFavorite"), shortcut: "Ctrl+D", onSelect: () => void favorite(i) },
  ];
  function closeMenu() {
    setMenu(null);
    input.focus();
  }
  function onResultsContextMenu(e: MouseEvent) {
    const row = (e.target as HTMLElement).closest<HTMLElement>('[role="option"]');
    if (!row) return;
    const i = items().findIndex((it) => optionId(LIST_ID, it) === row.id);
    if (i < 0) return;
    e.preventDefault();
    setSelected(i);
    setMenu({ x: e.clientX, y: e.clientY, index: i });
  }
  /** From the keyboard: under the selected row (or under the field when it is not drawn). */
  function openMenuForSelected() {
    const it = current();
    if (!it) return;
    const row = document.getElementById(optionId(LIST_ID, it));
    const box = (row ?? input).getBoundingClientRect();
    const rtl = localeDir(locale()) === "rtl";
    setMenu({ x: rtl ? box.right - 220 : box.left + 24, y: box.bottom - 4, index: selected() });
  }

  /** The index was rebuilt since this search: its ids are void. Say so and refresh the results. */
  function searchAgain() {
    showNotice(t("notice.stale"));
    void runSearch(sent(), scope());
  }

  async function copyPath() {
    const it = current();
    if (!it) return;
    try {
      const path = await api.entryPath(it.id);
      try {
        await navigator.clipboard.writeText(path);
      } catch (err) {
        if (!legacyCopy(path)) throw err;
      }
      showNotice(t("notice.copied"));
    } catch (err) {
      if (isStaleIdError(err)) return searchAgain();
      showNotice(t("notice.copyFailed", { message: errorMessage(err) }));
    }
  }

  /** Fallback when the async Clipboard API is denied: copy through a hidden textarea. */
  function legacyCopy(value: string): boolean {
    const ta = document.createElement("textarea");
    ta.value = value;
    ta.setAttribute("readonly", "");
    ta.style.position = "fixed";
    ta.style.opacity = "0";
    document.body.append(ta);
    ta.select();
    let ok = false;
    try {
      ok = document.execCommand("copy");
    } catch {
      ok = false;
    }
    ta.remove();
    input.focus();
    return ok;
  }

  function move(to: number) {
    const n = items().length;
    if (n) setSelected(Math.max(0, Math.min(n - 1, to)));
  }

  function onKeyDown(e: KeyboardEvent) {
    if (e.isComposing) return;
    const n = items().length;
    const page = list?.pageSize() ?? 8;
    const ctrl = e.ctrlKey || e.metaKey;
    // Physical keys (KeyboardEvent.code) so shortcuts work under the Arabic layout too (design §11).
    // Shift+F10 / the Menu key: the selected result's context menu.
    if ((e.key === "F10" && e.shiftKey && !ctrl && !e.altKey) || e.key === "ContextMenu") {
      if (!current()) return;
      e.preventDefault();
      openMenuForSelected();
      return;
    }
    if (ctrl && !e.altKey && !e.shiftKey && (e.code === "KeyD" || e.code === "KeyS")) {
      // Ctrl+D: add the selected result to the favorites. Ctrl+S: save this search.
      e.preventDefault();
      if (e.code === "KeyD") void favorite();
      else promptSave();
      return;
    }
    switch (e.code === "NumpadEnter" ? "Enter" : e.key) {
      case "ArrowDown":
        // In the empty field, ArrowDown walks into the recent / saved searches.
        if (!text().trim() && !n) {
          if (!library?.focusFirst()) return;
          break;
        }
        move(selected() + 1);
        break;
      case "ArrowUp": move(selected() - 1); break;
      case "PageDown": move(selected() + page); break;
      case "PageUp": move(selected() - page); break;
      case "ArrowLeft":
      case "ArrowRight": {
        // The arrow toward the pane toggles it, only with the caret at the end of the text
        // (where the key would do nothing), so it never steals caret movement.
        const el = e.currentTarget as HTMLInputElement;
        if (quick || e.key !== towardPane() || e.shiftKey || ctrl || e.altKey) return;
        if (el.selectionStart !== el.selectionEnd || el.selectionEnd !== el.value.length || !current()) return;
        setPreview(!previewOpen());
        break;
      }
      case "Home":
      case "End":
        // Shift+Home/End keeps its text-selection meaning in the input.
        if (e.shiftKey || !n) return;
        move(e.key === "Home" ? 0 : n - 1);
        break;
      case "Enter":
        if (!n) {
          // Nothing to open, but the search counts as made.
          if (text().trim() && !e.repeat) record();
          return;
        }
        void open(selected(), ctrl);
        break;
      case "Escape":
        // First Esc clears the query; Esc on an empty field closes the popup.
        if (text()) setText("");
        else if (quick) hideWindow();
        else return;
        break;
      default:
        // (`key` only as a fallback for synthetic events that carry no `code`.)
        if (ctrl && !e.altKey && !e.shiftKey && (e.code === "KeyC" || (!e.code && e.key.toLowerCase() === "c"))) {
          const el = e.currentTarget as HTMLInputElement;
          // With text selected in the input, Ctrl+C copies that text as usual.
          if (el.selectionStart !== el.selectionEnd || !current()) return;
          void copyPath();
          break;
        }
        return;
    }
    e.preventDefault();
  }

  const activeDescendant = () => {
    const it = current();
    return it && list?.isRendered(selected()) ? optionId(LIST_ID, it) : undefined;
  };

  // Error span in the user's text (a span inside the appended chip token is not underlined).
  const errorSpan = () => {
    const err = queryError();
    return err && err.start < text().length ? { start: err.start, end: err.end } : null;
  };

  function useSuggestion(err: QueryError, suggestion: string) {
    if (err.start >= text().length) return;
    setText(applySuggestion(text(), err.start, err.end, suggestion));
    input.focus();
  }

  function setQuery(q: string) {
    setText(q);
    input.focus();
    input.setSelectionRange(q.length, q.length);
  }

  // F3 / Shift+F3: next / previous match in the preview. Space on the pane (not in a field or
  // on a button) closes it, like Quick Look. Main window only.
  if (!quick) {
    const onGlobalKey = (e: KeyboardEvent) => {
      if (e.isComposing || !previewOpen()) return;
      if (e.key === "F3") {
        e.preventDefault(); // also keeps WebView2's find bar closed
        if (e.shiftKey) previewPane?.prevMatch();
        else previewPane?.nextMatch();
        return;
      }
      const target = e.target as HTMLElement | null;
      const typing = target?.closest("input, textarea, select, button, [contenteditable]");
      if (e.code === "Space" && !e.ctrlKey && !e.altKey && !e.metaKey && !typing && target?.closest(`#${PREVIEW_ID}`)) {
        e.preventDefault();
        closePreview();
      }
    };
    document.addEventListener("keydown", onGlobalKey);
    onCleanup(() => document.removeEventListener("keydown", onGlobalKey));
  }

  // ---- settings dialog and welcome screen (main window only) ----
  const [settingsOpen, setSettingsOpen] = createSignal(false);
  const [settingsSection, setSettingsSection] = createSignal<SettingsSection>("general");
  /** An update found by the start-up check (`app://update-available`); dismissible. */
  const [update, setUpdate] = createSignal<UpdateInfo | null>(null);
  const [welcomeOpen, setWelcomeOpen] = createSignal(!quick && !welcomeDone());
  function openSettings(section: SettingsSection = "general") {
    setMenu(null);
    setSettingsSection(section);
    setSettingsOpen(true);
  }
  function closeSettings() {
    setSettingsOpen(false);
    queueMicrotask(() => input.focus());
  }
  if (!quick) {
    // Ctrl+, opens the settings from anywhere in the window.
    const onSettingsKey = (e: KeyboardEvent) => {
      if (e.isComposing || !(e.ctrlKey || e.metaKey) || e.altKey || e.shiftKey || (e.code !== "Comma" && e.key !== ",")) return;
      if (welcomeOpen() || savePrompt()) return;
      e.preventDefault();
      if (!settingsOpen()) openSettings();
    };
    document.addEventListener("keydown", onSettingsKey);
    onCleanup(() => document.removeEventListener("keydown", onSettingsKey));

    // Explorer's "search in this folder" (app://search-in): the folder filter alone in the field.
    const searchIn = (path: string) => {
      batch(() => {
        setSettingsOpen(false);
        setSavePrompt(null);
        setMenu(null);
        setChip("all");
        // A scope that does not hold the folder would hide everything in it.
        const where = storedScope();
        if (where && !rootKey(path).startsWith(rootKey(where))) setScope("");
      });
      setQuery(folderQuery(path));
    };
    onMount(async () => {
      let disposed = false;
      let unlisten: (() => void) | undefined;
      onCleanup(() => {
        disposed = true;
        unlisten?.();
      });
      try {
        const stop = await onUpdateAvailable(setUpdate);
        if (disposed) stop();
        else onCleanup(stop);
      } catch (err) {
        console.warn("update listener unavailable", err);
      }
      try {
        unlisten = await onSearchIn(({ path }) => searchIn(path));
        if (disposed) return unlisten();
        // A folder from the launch that came before we listened.
        const pending = await api.takePendingSearchIn().catch(() => null);
        if (pending) searchIn(pending);
      } catch (err) {
        console.warn("search-in listener unavailable", err);
      }
    });
  }

  // ---- splitter between the results and the pane ----
  const maxPreviewWidth = () => {
    const w = mainEl?.clientWidth ?? 1200;
    return Math.max(PREVIEW_MIN, Math.min(PREVIEW_MAX, w - PANEL_MIN - 64));
  };
  const clampWidth = (w: number) => Math.round(Math.max(PREVIEW_MIN, Math.min(maxPreviewWidth(), w)));
  function resizeTo(w: number, save = false) {
    setPreviewWidth(clampWidth(w));
    if (save) writeSetting("previewWidth", String(previewWidth()));
  }
  function onSplitterDown(e: PointerEvent) {
    if (e.button !== 0) return;
    const handle = e.currentTarget as HTMLElement;
    const pane = document.getElementById(PREVIEW_ID);
    if (!pane) return;
    e.preventDefault();
    handle.setPointerCapture(e.pointerId);
    const rtl = localeDir(locale()) === "rtl";
    // The pane's outer edge stays put; its width follows the pointer.
    const rect = pane.getBoundingClientRect();
    const onMove = (ev: PointerEvent) => resizeTo(rtl ? ev.clientX - rect.left : rect.right - ev.clientX);
    const onUp = () => {
      handle.removeEventListener("pointermove", onMove);
      handle.removeEventListener("pointerup", onUp);
      handle.removeEventListener("pointercancel", onUp);
      resizeTo(previewWidth(), true);
    };
    handle.addEventListener("pointermove", onMove);
    handle.addEventListener("pointerup", onUp);
    handle.addEventListener("pointercancel", onUp);
  }
  function onSplitterKey(e: KeyboardEvent) {
    const grow = localeDir(locale()) === "rtl" ? "ArrowRight" : "ArrowLeft";
    const stepPx = e.shiftKey ? 64 : 16;
    let w: number;
    switch (e.key) {
      case "ArrowLeft":
      case "ArrowRight":
        w = previewWidth() + (e.key === grow ? stepPx : -stepPx);
        break;
      case "Home": w = PREVIEW_MIN; break;
      case "End": w = maxPreviewWidth(); break;
      default: return;
    }
    e.preventDefault();
    resizeTo(w, true);
  }
  // Keep the remembered width inside the window when it shrinks.
  if (!quick) {
    const onResize = () => previewOpen() && setPreviewWidth((w) => clampWidth(w));
    window.addEventListener("resize", onResize);
    onCleanup(() => window.removeEventListener("resize", onResize));
  }

  const indexing = () => index()?.state === "indexing";
  /** Total height of the result rows (the popup's list area takes exactly that). */
  const [listHeight, setListHeight] = createSignal(0);
  const notReady = () => index()?.state !== "ready";

  // ---- quick-search popup ----
  let frame: HTMLDivElement | undefined;
  if (quick) {
    onMount(() => {
      if (frame) onCleanup(fitWindowTo(frame));
    });
    // Shown again (global shortcut, tray): ready to type over the previous query.
    onCleanup(
      onWindowFocus(() => {
        input.focus();
        input.select();
      }),
    );
  }

  const panel = (
    <section class="panel" classList={{ "quick-panel": quick }} aria-label={t("search.region")}>
      <SearchField
        ref={(el) => (input = el)}
        value={text()}
        onInput={setText}
        onKeyDown={onKeyDown}
        errorSpan={errorSpan()}
        listId={LIST_ID}
        expanded={items().length > 0}
        activeDescendant={activeDescendant()}
        describedBy={queryError() || failure() ? ERROR_ID : undefined}
        addon={
          <>
            <Show when={text().trim()}>
              <button type="button" class="save-search-btn" title={t("library.saveSearchTitle")}
                aria-label={t("library.saveSearchTitle")} aria-keyshortcuts="Control+S"
                onMouseDown={(e) => e.preventDefault()} onClick={promptSave}>
                <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75"
                  stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                  <path d="M7 4h10a1 1 0 0 1 1 1v15l-6-4-6 4V5a1 1 0 0 1 1-1Z" />
                </svg>
              </button>
            </Show>
            <ScopeSelect roots={roots()} value={scope()} onChange={setScope} />
          </>
        }
      />
      <div class="progress" classList={{ active: indexing() }} aria-hidden="true" />
      <FilterChips value={chip()} onChange={setChip} content={searchContent()} onContentChange={setSearchContent} />
      <QueryErrorBar
        id={ERROR_ID}
        text={text()}
        error={queryError()}
        failure={failure()}
        onApplySuggestion={useSuggestion}
      />

      {/* In the popup the area is as tall as its rows (--list-h), so the window grows with them. */}
      <div
        class="results-area"
        style={quick ? { "--list-h": text().trim() ? `${listHeight()}px` : "auto" } : undefined}
        onContextMenu={onResultsContextMenu}
      >
        <Switch>
          <Match when={!text().trim()}>
            <LibraryPanel
              ref={(h) => (library = h)}
              compact={quick}
              now={now()}
              onRun={runFromLibrary}
              onNotice={showNotice}
              onExit={() => input.focus()}
              fallback={<EmptyState onExample={setQuery} compact={quick} indexing={indexing()} />}
              before={quick ? undefined : <EmptyState onExample={setQuery} library indexing={indexing()} />}
            />
          </Match>
          <Match when={queryError() || failure()}>{null}</Match>
          <Match when={notReady() && !items().length}>
            <NotReady
              failed={index()?.state === "error"}
              state={index()?.state}
              message={index()?.error}
              onRetry={retryIndexing}
              onStart={() => void api.startIndexing()}
            />
          </Match>
          <Match when={response() && !items().length}>
            <NoResults
              query={text().trim()}
              filtered={chip() !== "all"}
              onClearFilter={() => setChip("all")}
              scoped={scope() !== ""}
              onClearScope={() => setScope("")}
              suggestion={didYouMean()}
              onSuggestion={setQuery}
              onSearchContent={searchContent() ? undefined : () => setSearchContent(true)}
            />
          </Match>
        </Switch>
        {/* Always mounted so aria-controls points at a real listbox. */}
        <ResultList
          ref={(h) => (list = h)}
          id={LIST_ID}
          label={t("search.resultsLabel")}
          items={items()}
          truncated={!!response()?.truncated}
          selected={selected()}
          now={now()}
          compact={quick}
          loadSnippets={api.contentSnippets}
          snippetsPaused={searchPending()}
          onHeight={setListHeight}
          onSelect={setSelected}
          onOpen={(i) => {
            setSelected(i);
            void open(i);
          }}
        />
      </div>

      {/* The popup has no status bar: notices (copied, failures) show here instead. */}
      <Show when={quick}>
        <p class="quick-notice" role="status">{notice()}</p>
      </Show>
      <HintBar quick={quick} previewKey={towardPane() === "ArrowLeft" ? "←" : "→"} />
      <Show when={menu()}>
        {(m) => <ContextMenu x={m().x} y={m().y} label={t("library.menuLabel")} items={menuItems(m().index)} onClose={closeMenu} />}
      </Show>
      <Show when={savePrompt()}>
        {(p) => (
          <PromptDialog
            title={t("library.saveTitle")}
            label={t("library.nameLabel")}
            detail={p().query}
            initial={p().query}
            confirm={t("library.saveConfirm")}
            onSubmit={(name) => saveSearch(name, p().query, p().scope).then(() => showNotice(t("library.savedNotice", { name })))}
            onClose={() => {
              setSavePrompt(null);
              input.focus();
            }}
          />
        )}
      </Show>
    </section>
  );

  if (quick) {
    return (
      <div
        ref={frame}
        class="quick-frame"
        // A click on the transparent margin around the panel counts as a click outside.
        onMouseDown={(e) => {
          if (e.target === e.currentTarget) hideWindow();
        }}
      >
        {panel}
      </div>
    );
  }

  return (
    <div class="shell">
      <div>
        <header class="topbar">
          <div class="brand">
            <img src="/athrix.svg" alt="" />
            <span class="brand-name" lang="en">{t("app.brandName")}</span>
            <span class="brand-ar" lang="ar">{t("app.brandNative")}</span>
          </div>
          <Show when={info()}>
            {(i) => (
              <bdi class="version" dir="ltr" title={t("topbar.versionTitle")}>
                {t("topbar.version", { version: i().version })}
              </bdi>
            )}
          </Show>
          <button type="button" class="drives-btn" aria-pressed={previewOpen()}
            aria-controls={previewOpen() ? PREVIEW_ID : undefined}
            title={t(previewOpen() ? "preview.hide" : "preview.show")}
            onMouseDown={(e) => e.preventDefault()} onClick={() => setPreview(!previewOpen())}>
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75"
              stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
              <rect x="3.5" y="4.5" width="17" height="15" rx="2" /><path d="M14 4.5v15" />
            </svg>
            {t("hints.preview")}
          </button>
          <button type="button" class="drives-btn" aria-haspopup="dialog" aria-keyshortcuts="Control+Comma"
            title={t("settings.openTitle")} onClick={() => openSettings()}>
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75"
              stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
              <path d="M4 7h10M18 7h2M4 17h4M12 17h8" /><circle cx="16" cy="7" r="2" /><circle cx="10" cy="17" r="2" />
            </svg>
            {t("settings.title")}
          </button>
        </header>
        <Show when={update()}>
          {(u) => (
            <div class="update-banner" role="status">
              <span class="update-banner-text">{t("update.banner")} <bdi dir="ltr">{u().version ?? ""}</bdi></span>
              <button type="button" class="btn-ghost" onClick={() => openSettings("about")}>{t("update.bannerOpen")}</button>
              <button type="button" class="btn-ghost" onClick={() => setUpdate(null)}>{t("update.bannerDismiss")}</button>
            </div>
          )}
        </Show>
      </div>

      <main ref={mainEl} class="main" classList={{ "with-preview": previewOpen() }}>
        {panel}
        <Show when={previewOpen()}>
          <div
            class="splitter"
            role="separator"
            tabIndex={0}
            aria-orientation="vertical"
            aria-controls={PREVIEW_ID}
            aria-label={t("preview.resize")}
            aria-valuemin={PREVIEW_MIN}
            aria-valuemax={maxPreviewWidth()}
            aria-valuenow={previewWidth()}
            onPointerDown={onSplitterDown}
            onKeyDown={onSplitterKey}
            onDblClick={() => resizeTo(PREVIEW_DEFAULT, true)}
          />
          <div class="preview-slot" style={{ "inline-size": `${previewWidth()}px` }}>
            <PreviewPane
              id={PREVIEW_ID}
              ref={(h) => (previewPane = h)}
              item={current()}
              query={text()}
              onOpen={() => void open(selected())}
              onReveal={() => void open(selected(), true)}
              onClose={closePreview}
              onStale={searchAgain}
            />
          </div>
        </Show>
      </main>

      <StatusBar
        index={index()}
        response={notReady() ? null : response()}
        notice={notice()}
        mock={usingMock}
        onRetry={retryIndexing}
        content={
          <ContentStatusControl
            onError={(message) => showNotice(t("content.actionFailed", { message }))}
            onOpenSettings={() => openSettings("indexing")}
          />
        }
      />

      <Show when={settingsOpen()}>
        <SettingsDialog
          initial={settingsSection()}
          info={info()}
          onClose={closeSettings}
          onNotice={showNotice}
          onShowWelcome={() => {
            setSettingsOpen(false);
            setWelcomeOpen(true);
          }}
        />
      </Show>
      <Show when={welcomeOpen()}>
        <Onboarding
          onNotice={showNotice}
          onDone={() => {
            setWelcomeOpen(false);
            input.focus();
          }}
        />
      </Show>
    </div>
  );
}

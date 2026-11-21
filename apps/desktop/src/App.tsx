import { batch, createEffect, createMemo, createResource, createSignal, Match, on, onCleanup, onMount, Show, Switch } from "solid-js";
import { locale, localeDir, t } from "./i18n";
import {
  api, errorMessage, isStaleIdError, onIndexStatus, usingMock, type IndexStatus, type QueryError, type SearchResponse,
} from "./lib/api";
import { applySuggestion, composeQuery, suggestionInput, type ChipKind } from "./lib/query";
import SettingsControls from "./components/SettingsControls";
import SearchField from "./components/SearchField";
import FilterChips from "./components/FilterChips";
import QueryErrorBar from "./components/QueryErrorBar";
import ResultList, { optionId, type ResultListHandle } from "./components/ResultList";
import { EmptyState, NoResults, NotReady } from "./components/EmptyState";
import HintBar from "./components/HintBar";
import StatusBar from "./components/StatusBar";
import ScopeSelect from "./components/ScopeSelect";
import IndexSettings from "./components/IndexSettings";
import PreviewPane, { type PreviewHandle } from "./components/PreviewPane";
import { readSetting, writeSetting } from "./lib/storage";
import { effectiveScope, setScope, storedScope } from "./lib/scope";
import { fitWindowTo, hideWindow, onWindowFocus, type WindowMode } from "./lib/window";

/** Results per query (contract `limit`). Dev-only override for scroll testing: `?limit=5000`. */
const LIMIT = (() => {
  const v = import.meta.env.DEV ? Number(new URLSearchParams(window.location.search).get("limit")) : NaN;
  return Number.isFinite(v) && v > 0 ? Math.floor(Math.min(v, 100_000)) : 200;
})();

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
  const roots = () => index()?.roots ?? [];
  const scope = createMemo(() => effectiveScope(storedScope(), index()?.roots));

  // Whole numbers: Rust reads requestId as u64. Seeded from the clock so ids keep increasing
  // across a webview reload (or HMR) while the backend stays alive.
  let lastRequest = Date.now();
  async function runSearch(query: string, where: string) {
    const requestId = ++lastRequest;
    if (!query.trim()) {
      batch(() => {
        setResponse(null);
        setFailure(null);
        setSelected(0);
      });
      return;
    }
    try {
      const r = await api.search(query, LIMIT, requestId, where || null);
      // Drop anything older than the latest request, and cancelled ones.
      if (r.requestId !== lastRequest || requestId !== lastRequest || r.cancelled) return;
      batch(() => {
        setResponse(r);
        setFailure(null);
        setSelected(0);
      });
    } catch (err) {
      if (requestId === lastRequest) {
        batch(() => {
          setResponse(null);
          setFailure(t("queryError.searchFailed", { message: errorMessage(err) }));
        });
      }
    }
  }
  // No debounce: a search takes a few ms, and stale responses are dropped by requestId.
  // Re-runs when the index becomes ready, so a query typed during indexing fills in, and when
  // the scope changes.
  createEffect(on([sent, () => index()?.state, scope], ([q, , where]) => void runSearch(q, where)));

  const items = () => response()?.items ?? [];
  // "Did you mean": the backend rewrote the query as sent; the field gets it without the chip suffix.
  const didYouMean = () => {
    const s = response()?.suggestion;
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
    switch (e.code === "NumpadEnter" ? "Enter" : e.key) {
      case "ArrowDown": move(selected() + 1); break;
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
        if (!n) return;
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
        addon={<ScopeSelect roots={roots()} value={scope()} onChange={setScope} />}
      />
      <div class="progress" classList={{ active: indexing() }} aria-hidden="true" />
      <FilterChips value={chip()} onChange={setChip} />
      <QueryErrorBar
        id={ERROR_ID}
        text={text()}
        error={queryError()}
        failure={failure()}
        onApplySuggestion={useSuggestion}
      />

      {/* In the popup the area is as tall as its rows (--rows), so the window grows with them. */}
      <div class="results-area" style={quick ? { "--rows": String(items().length) } : undefined}>
        <Switch>
          <Match when={!text().trim()}>
            <EmptyState onExample={setQuery} compact={quick} />
          </Match>
          <Match when={queryError() || failure()}>{null}</Match>
          <Match when={notReady() && !items().length}>
            <NotReady failed={index()?.state === "error"} />
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
            />
          </Match>
        </Switch>
        {/* Always mounted so aria-controls points at a real listbox. */}
        <ResultList
          ref={(h) => (list = h)}
          id={LIST_ID}
          label={t("search.resultsLabel")}
          items={items()}
          selected={selected()}
          now={now()}
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
        <IndexSettings onError={(message) => showNotice(t("drives.failed", { message }))} />
        <SettingsControls />
      </header>

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

      <StatusBar index={index()} response={notReady() ? null : response()} notice={notice()} mock={usingMock} onRetry={retryIndexing} />
    </div>
  );
}

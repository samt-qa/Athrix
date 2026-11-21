// Which window this page runs in, and the small window-level actions of the quick-search popup.
// Both windows load the same page; the popup is the window labelled "quick" in tauri.conf.json
// (see src-tauri/src/popup.rs). Outside Tauri (plain browser, mock backend) `?mode=quick` shows
// the popup layout instead.
import { isTauri } from "@tauri-apps/api/core";
import { emit } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";

export type WindowMode = "main" | "quick";

/** Label of the popup window (`QUICK_LABEL` in popup.rs). */
const QUICK_LABEL = "quick";

/** Event that tells the tray menu which language to use (`LOCALE_EVENT` in tray.rs). */
const LOCALE_EVENT = "app://locale";

function detectMode(): WindowMode {
  if (isTauri()) return getCurrentWindow().label === QUICK_LABEL ? "quick" : "main";
  return new URLSearchParams(window.location.search).get("mode") === "quick" ? "quick" : "main";
}

export const windowMode: WindowMode = detectMode();
export const isQuickWindow = windowMode === "quick";

/** Hides this window (the popup: Esc, after opening a result, a click outside the panel). */
export function hideWindow(): void {
  if (!isTauri()) return;
  getCurrentWindow().hide().catch((err) => console.error("hide failed", err));
}

/**
 * Keeps the window exactly the size of `el` (the popup frame), so the popup grows with its
 * content and no invisible window area is left to catch clicks. Returns a disposer.
 */
export function fitWindowTo(el: HTMLElement): () => void {
  if (!isTauri()) return () => {};
  let last = "";
  const fit = () => {
    const w = Math.ceil(el.offsetWidth);
    const h = Math.ceil(el.offsetHeight);
    const key = `${w}x${h}`;
    if (!w || !h || key === last) return;
    last = key;
    getCurrentWindow().setSize(new LogicalSize(w, h)).catch((err) => console.error("resize failed", err));
  };
  const observer = new ResizeObserver(fit);
  observer.observe(el);
  fit();
  return () => observer.disconnect();
}

/** Calls `handler` each time this window gains focus (the popup: each time it is shown). */
export function onWindowFocus(handler: () => void): () => void {
  if (!isTauri()) return () => {};
  let disposed = false;
  let unlisten: (() => void) | undefined;
  getCurrentWindow()
    .onFocusChanged(({ payload: focused }) => {
      if (focused) handler();
    })
    .then((u) => (disposed ? u() : (unlisten = u)))
    .catch((err) => console.error("focus listener failed", err));
  return () => {
    disposed = true;
    unlisten?.();
  };
}

/** Tells the Rust side the UI language, for the tray menu labels. */
export function announceLocale(locale: string): void {
  if (!isTauri()) return;
  emit(LOCALE_EVENT, locale).catch((err) => console.error("locale event failed", err));
}

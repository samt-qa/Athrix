// "Understood as": the one-line suggestion under the search field when the user types plain
// Arabic or English («ملفات PDF من الأسبوع الماضي أكبر من 5 ميجا») that reads as filters
// (`interpret_query`, task 6.8). Never rewrites the text silently: Tab or the button applies
// the suggestion, Esc dismisses it (remembered for that exact text). With the setting
// «تطبيق التفسير تلقائيًا» on, it applies itself after a pause and Esc undoes it.
import { createEffect, createSignal, on, onCleanup, Show } from "solid-js";
import { t } from "../i18n";
import { api, type Interpretation } from "../lib/api";
import { storedFlag } from "../lib/storage";

/** «تطبيق التفسير تلقائيًا» (default off), shared by both windows. */
export const [autoInterpret, setAutoInterpret] = storedFlag("autoInterpret", false);

/** Keys the hint takes from the search field. */
export type InterpretHintHandle = {
  /** Tab applies, Esc dismisses (or undoes an automatic apply). True when the key was used. */
  handleKey: (e: KeyboardEvent) => boolean;
};

type Props = {
  /** What is in the search field. */
  text: string;
  /** Replaces the field's text. */
  onApply: (query: string) => void;
  ref?: (handle: InterpretHintHandle) => void;
};

const DEBOUNCE_MS = 150;
/** Extra idle time before an automatic apply, so it never lands mid-word. */
const AUTO_APPLY_MS = 700;

// Texts whose suggestion was dismissed (this session; capped).
const dismissed = new Set<string>();
function dismiss(text: string) {
  if (dismissed.size > 500) dismissed.clear();
  dismissed.add(text);
}

export default function InterpretHint(props: Props) {
  const [found, setFound] = createSignal<{ text: string; it: Interpretation } | null>(null);
  // An automatic apply that Esc can undo: the text before and the query put in its place.
  const [applied, setApplied] = createSignal<{ original: string; query: string } | null>(null);
  let timer: number | undefined;
  let autoTimer: number | undefined;
  let seq = 0;

  createEffect(
    on(
      () => props.text,
      (text) => {
        window.clearTimeout(timer);
        window.clearTimeout(autoTimer);
        const a = applied();
        if (a && text !== a.query) setApplied(null);
        const f = found();
        if (f && f.text !== text) setFound(null);
        if (!text.trim() || dismissed.has(text)) return;
        const mine = ++seq;
        timer = window.setTimeout(() => {
          api.interpretQuery(text).then(
            (it) => {
              if (mine !== seq || props.text !== text) return;
              setFound(it ? { text, it } : null);
              if (it && autoInterpret()) {
                autoTimer = window.setTimeout(() => {
                  if (props.text === text) applyNow(true);
                }, AUTO_APPLY_MS);
              }
            },
            () => {
              // A failed interpretation just shows nothing.
            },
          );
        }, DEBOUNCE_MS);
      },
    ),
  );
  onCleanup(() => {
    window.clearTimeout(timer);
    window.clearTimeout(autoTimer);
  });

  const visible = () => {
    const f = found();
    return f && f.text === props.text ? f.it : null;
  };
  const undoable = () => {
    const a = applied();
    return a && a.query === props.text ? a : null;
  };

  function applyNow(auto: boolean) {
    const f = found();
    if (!f || f.text !== props.text) return;
    window.clearTimeout(autoTimer);
    // Never offered again for this text: going back to it (undo, history) keeps it plain.
    dismiss(f.text);
    setFound(null);
    if (auto) setApplied({ original: f.text, query: f.it.query });
    props.onApply(f.it.query);
  }

  function dismissNow() {
    const f = found();
    if (f) dismiss(f.text);
    window.clearTimeout(autoTimer);
    setFound(null);
  }

  function undo() {
    const a = undoable();
    if (!a) return;
    setApplied(null);
    props.onApply(a.original);
  }

  props.ref?.({
    handleKey(e) {
      const plain = !e.ctrlKey && !e.metaKey && !e.altKey && !e.shiftKey;
      if (!plain) return false;
      if (e.key === "Tab" && visible()) {
        applyNow(false);
        return true;
      }
      if (e.key === "Escape") {
        if (visible()) {
          dismissNow();
          return true;
        }
        if (undoable()) {
          undo();
          return true;
        }
      }
      return false;
    },
  });

  return (
    <div class="interpret-live" role="status">
      <Show when={visible()}>
        {(it) => (
          <div class="interpret-hint">
            <span class="interpret-label">{t("interpret.label")}</span>
            <bdi class="interpret-query" dir="ltr">{it().query}</bdi>
            <span class="interpret-sep" aria-hidden="true">—</span>
            <button
              type="button"
              class="suggestion"
              aria-label={t("interpret.applyLabel", { query: it().query })}
              aria-keyshortcuts="Tab"
              onMouseDown={(e) => e.preventDefault()}
              onClick={() => applyNow(false)}
            >
              {t("interpret.apply")}
            </button>
            <kbd aria-hidden="true">Tab</kbd>
            <button
              type="button"
              class="interpret-dismiss"
              aria-keyshortcuts="Escape"
              onMouseDown={(e) => e.preventDefault()}
              onClick={dismissNow}
            >
              {t("interpret.dismiss")}
            </button>
          </div>
        )}
      </Show>
      <Show when={!visible() && undoable()}>
        {(a) => (
          <div class="interpret-hint">
            <span class="interpret-label">{t("interpret.applied")}</span>
            <bdi class="interpret-query" dir="auto">{a().original}</bdi>
            <span class="interpret-sep" aria-hidden="true">—</span>
            <button
              type="button"
              class="suggestion"
              aria-keyshortcuts="Escape"
              onMouseDown={(e) => e.preventDefault()}
              onClick={undo}
            >
              {t("interpret.undo")}
            </button>
            <kbd aria-hidden="true">Esc</kbd>
          </div>
        )}
      </Show>
    </div>
  );
}

import { createEffect, createMemo, createSignal, For, onCleanup, onMount, Show, type JSX } from "solid-js";
import { locale, t, type MessageKey } from "../i18n";
import { applyCompletion, complete, type Completion, type Suggestion } from "../lib/autocomplete";
import { tokenize, type TokenKind } from "../lib/queryLang";
import { SearchIcon } from "./icons";

type Props = {
  ref: (el: HTMLInputElement) => void;
  value: string;
  onInput: (value: string) => void;
  /** Keys the suggestions list does not take (results navigation, open, Esc…). */
  onKeyDown: JSX.EventHandler<HTMLInputElement, KeyboardEvent>;
  /** Error span in `value` (UTF-16), tinted and underlined in the highlight layer. */
  errorSpan: { start: number; end: number } | null;
  /** The results listbox (aria-controls while no suggestions are shown). */
  listId: string;
  expanded: boolean;
  activeDescendant: string | undefined;
  describedBy: string | undefined;
  /** Shown at the inline end of the bar, outside the input's label (e.g. the scope selector). */
  addon?: JSX.Element;
};

const SUGGEST_ID = "search-suggestions";
const suggestionId = (i: number) => `${SUGGEST_ID}-${i}`;

type Segment = { text: string; cls: string };

/** Splits the query into colored segments: lexer tokens, cut again at the error span. */
function segments(value: string, err: { start: number; end: number } | null): Segment[] {
  const cls = (k: TokenKind) => (k === "text" || k === "ws" ? "" : `tok-${k}`);
  const out: Segment[] = [];
  const add = (start: number, end: number, c: string) => {
    if (end > start) out.push({ text: value.slice(start, end), cls: c });
  };
  for (const tok of tokenize(value)) {
    const c = cls(tok.kind);
    if (!err || err.end <= tok.start || err.start >= tok.end) {
      add(tok.start, tok.end, c);
      continue;
    }
    const a = Math.max(tok.start, err.start);
    const b = Math.min(tok.end, err.end);
    add(tok.start, a, c);
    add(a, b, `${c} tok-error`.trim());
    add(b, tok.end, c);
  }
  return out;
}

/**
 * The search bar (design §10 SearchBar) with syntax coloring and autocomplete.
 *
 * Coloring: the real <input> keeps the caret, selection, IME and bidi behavior but paints its
 * text transparent; a highlight layer under it repeats the same text in the same font, split
 * into colored spans (color only: no weight, spacing or isolation changes, so glyph positions
 * and the bidi reordering of mixed Arabic/Latin are identical). Both use dir="auto" on the same
 * text, and the layer's horizontal scroll follows the input's. While an IME composition is in
 * progress the input shows its own text and the layer hides, so composition is never mis-drawn.
 *
 * Autocomplete: a listbox under the field (combobox pattern). While it is open ↑/↓ move in it,
 * Tab or Enter (on a highlighted option) accept, Esc closes it; otherwise keys go to the parent.
 */
export default function SearchField(props: Props) {
  let input!: HTMLInputElement;
  let layer!: HTMLDivElement;
  let listEl: HTMLUListElement | undefined;

  const [caret, setCaret] = createSignal(0);
  const [focused, setFocused] = createSignal(false);
  const [composing, setComposing] = createSignal(false);
  const [active, setActive] = createSignal(-1);
  /** Text at which the user closed the list (Esc / accept); it stays closed until the text changes. */
  const [dismissed, setDismissed] = createSignal<string | null>(null);

  const sync = () => {
    if (layer && input) layer.scrollLeft = input.scrollLeft;
  };
  const readCaret = () => {
    if (!input) return;
    const collapsed = input.selectionStart === input.selectionEnd;
    setCaret(collapsed ? (input.selectionStart ?? -1) : -1);
    sync();
  };

  createEffect(() => {
    props.value;
    props.errorSpan;
    queueMicrotask(sync);
  });

  onMount(() => {
    // Caret moves (arrows, clicks, Home/End) also change what can be completed.
    const onSel = () => {
      if (document.activeElement === input) readCaret();
    };
    document.addEventListener("selectionchange", onSel);
    onCleanup(() => document.removeEventListener("selectionchange", onSel));
  });

  const parts = createMemo(() => {
    const e = props.errorSpan;
    const err = e && e.end > e.start && e.start < props.value.length
      ? { start: e.start, end: Math.min(e.end, props.value.length) }
      : null;
    return segments(props.value, err);
  });

  const completion = createMemo<Completion | null>(() => {
    if (!focused() || composing() || dismissed() === props.value) return null;
    const c = caret();
    if (c < 0 || c > props.value.length) return null;
    return complete(props.value, c, locale());
  });
  const open = () => !!completion();
  // A new list starts with nothing highlighted (so Enter still opens the selected result).
  createEffect(() => {
    completion();
    setActive(-1);
  });
  createEffect(() => {
    const i = active();
    if (i >= 0) listEl?.querySelector(`#${suggestionId(i)}`)?.scrollIntoView({ block: "nearest" });
  });

  function accept(s: Suggestion) {
    const c = completion();
    if (!c) return;
    const next = applyCompletion(props.value, c, s);
    input.value = next.text;
    props.onInput(next.text);
    input.focus();
    input.setSelectionRange(next.caret, next.caret);
    setCaret(next.caret);
    // A value (kind, date…) is final; a filter name chains into its value suggestions.
    if (s.kind !== "filter") setDismissed(next.text);
    queueMicrotask(sync);
  }

  const onKeyDown: JSX.EventHandler<HTMLInputElement, KeyboardEvent> = (e) => {
    const c = completion();
    if (c && !e.isComposing && !e.altKey) {
      const n = c.items.length;
      const mod = e.ctrlKey || e.metaKey;
      switch (e.key) {
        case "ArrowDown":
          if (mod || e.shiftKey) break;
          e.preventDefault();
          setActive((a) => (a + 1) % n);
          return;
        case "ArrowUp":
          if (mod || e.shiftKey) break;
          e.preventDefault();
          setActive((a) => (a <= 0 ? n - 1 : a - 1));
          return;
        case "Tab":
          if (mod || e.shiftKey) break;
          e.preventDefault();
          accept(c.items[Math.max(0, active())]);
          return;
        case "Enter":
          if (mod || e.shiftKey || active() < 0) break;
          e.preventDefault();
          accept(c.items[active()]);
          return;
        case "Escape":
          // Close the list only; the next Esc reaches the parent (clear / close the popup).
          e.preventDefault();
          setDismissed(props.value);
          return;
      }
    }
    props.onKeyDown(e);
  };

  const describe = (s: Suggestion) => t(s.detail as MessageKey, s.params);

  return (
    <div class="search-wrap">
      <div class="search">
        <label class="search-main">
          <SearchIcon />
          <div class="search-field" classList={{ composing: composing() }}>
            <div ref={layer} class="search-mirror" dir="auto" aria-hidden="true">
              <For each={parts()}>{(p) => (p.cls ? <span class={p.cls}>{p.text}</span> : p.text)}</For>
            </div>
            <input
              ref={(el) => {
                input = el;
                props.ref(el);
              }}
              dir="auto"
              autofocus
              spellcheck={false}
              autocomplete="off"
              role="combobox"
              aria-label={t("search.inputLabel")}
              aria-autocomplete="list"
              aria-controls={open() ? SUGGEST_ID : props.listId}
              aria-expanded={open() || props.expanded}
              aria-activedescendant={open() ? (active() >= 0 ? suggestionId(active()) : undefined) : props.activeDescendant}
              aria-invalid={props.errorSpan ? true : undefined}
              aria-describedby={props.describedBy}
              placeholder={t("search.placeholder")}
              value={props.value}
              onInput={(e) => {
                setDismissed(null);
                props.onInput(e.currentTarget.value);
                readCaret();
              }}
              onKeyDown={onKeyDown}
              onKeyUp={readCaret}
              onScroll={sync}
              onSelect={readCaret}
              onFocus={() => {
                setFocused(true);
                readCaret();
              }}
              onBlur={() => setFocused(false)}
              onCompositionStart={() => setComposing(true)}
              onCompositionEnd={() => {
                setComposing(false);
                readCaret();
              }}
            />
          </div>
        </label>
        {props.addon}
      </div>
      <Show when={completion()}>
        {(c) => (
          <ul ref={listEl} id={SUGGEST_ID} class="suggest" role="listbox" aria-label={t("search.suggestionsLabel")}>
            <For each={c().items}>
              {(s, i) => (
                <li
                  id={suggestionId(i())}
                  class="suggest-item"
                  role="option"
                  aria-selected={active() === i()}
                  onMouseDown={(e) => e.preventDefault()}
                  onMouseMove={() => setActive(i())}
                  onClick={() => accept(s)}
                >
                  <bdi class={`suggest-label tok-${s.kind === "filter" ? "filter" : "value"}`} dir="auto">{s.label}</bdi>
                  {/* No description when it only repeats the label (Arabic kinds: صور = صور). */}
                  <Show when={describe(s) !== s.label}>
                    <span class="suggest-detail">{describe(s)}</span>
                  </Show>
                </li>
              )}
            </For>
          </ul>
        )}
      </Show>
    </div>
  );
}

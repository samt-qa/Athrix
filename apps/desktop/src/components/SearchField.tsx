import { createEffect, Show, type JSX } from "solid-js";
import { t } from "../i18n";
import { SearchIcon } from "./icons";

type Props = {
  ref: (el: HTMLInputElement) => void;
  value: string;
  onInput: (value: string) => void;
  onKeyDown: JSX.EventHandler<HTMLInputElement, KeyboardEvent>;
  /** Error span in `value` (UTF-16), underlined through a mirror layer. */
  errorSpan: { start: number; end: number } | null;
  listId: string;
  expanded: boolean;
  activeDescendant: string | undefined;
  describedBy: string | undefined;
  /** Shown at the inline end of the bar, outside the input's label (e.g. the scope selector). */
  addon?: JSX.Element;
};

/**
 * The search bar (design §10 SearchBar). A transparent-text mirror sits under the input and
 * repeats its value so a wavy underline can mark the query error span exactly under the
 * offending characters; its horizontal scroll is kept in sync with the input.
 */
export default function SearchField(props: Props) {
  let input!: HTMLInputElement;
  let mirror!: HTMLDivElement;

  const sync = () => {
    if (mirror && input) mirror.scrollLeft = input.scrollLeft;
  };
  createEffect(() => {
    props.value;
    props.errorSpan;
    queueMicrotask(sync);
  });

  const span = () => {
    const e = props.errorSpan;
    if (!e || e.start >= props.value.length || e.end <= e.start) return null;
    const end = Math.min(e.end, props.value.length);
    return { before: props.value.slice(0, e.start), bad: props.value.slice(e.start, end), after: props.value.slice(end) };
  };

  return (
    <div class="search">
      <label class="search-main">
        <SearchIcon />
        <div class="search-field">
          <Show when={span()}>
            {(s) => (
              <div ref={mirror} class="search-mirror" dir="auto" aria-hidden="true">
                {s().before}<span class="search-error-span">{s().bad}</span>{s().after}
              </div>
            )}
          </Show>
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
            aria-controls={props.listId}
            aria-expanded={props.expanded}
            aria-activedescendant={props.activeDescendant}
            aria-invalid={props.errorSpan ? true : undefined}
            aria-describedby={props.describedBy}
            placeholder={t("search.placeholder")}
            value={props.value}
            onInput={(e) => props.onInput(e.currentTarget.value)}
            onKeyDown={props.onKeyDown}
            onScroll={sync}
            onSelect={sync}
            onKeyUp={sync}
          />
        </div>
      </label>
      {props.addon}
    </div>
  );
}

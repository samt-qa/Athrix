import { For, Show } from "solid-js";
import { t, type MessageKey } from "../i18n";

const EXAMPLES = [1, 2, 3, 4] as const;

/**
 * No query yet (design §9.3 "empty state"): a friendly line plus clickable example queries.
 * `compact` (the quick-search popup): a single hint line, so the popup stays small.
 */
export function EmptyState(props: { onExample: (query: string) => void; compact?: boolean }) {
  if (props.compact) return <p class="state-hint">{t("empty.quickHint")}</p>;
  return (
    <div class="state">
      <svg class="state-art" width="56" height="56" viewBox="0 0 56 56" fill="none" stroke="currentColor"
        stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
        <path d="M14 8h18l10 10v26a4 4 0 0 1-4 4H14a4 4 0 0 1-4-4V12a4 4 0 0 1 4-4Z" />
        <path d="M32 8v10h10" />
        <circle cx="26" cy="31" r="6" />
        <path d="m30.5 35.5 5 5" />
      </svg>
      <h2 class="state-title">{t("empty.title")}</h2>
      <p class="state-body">{t("empty.body")}</p>
      <div class="examples">
        <span class="examples-label">{t("empty.examples")}</span>
        <For each={EXAMPLES}>
          {(n) => {
            const q = () => t(`empty.ex${n}.query` as MessageKey);
            return (
              <button type="button" class="example" onMouseDown={(e) => e.preventDefault()} onClick={() => props.onExample(q())}>
                <bdi class="example-query" dir="auto">{q()}</bdi>
                <span class="example-desc">{t(`empty.ex${n}.desc` as MessageKey)}</span>
              </button>
            );
          }}
        </For>
      </div>
    </div>
  );
}

/** A "did you mean" offer: the text to put in the search field and how it was found. */
export interface DidYouMean {
  text: string;
  kind: "layout" | "fuzzy";
}

/** A query with no matches (design §9.3 "no results"). */
export function NoResults(props: {
  query: string;
  filtered: boolean;
  onClearFilter: () => void;
  /** Searching one drive only: offer the whole computer. */
  scoped?: boolean;
  onClearScope?: () => void;
  /** A rewrite of the query that finds results (from `SearchResponse.suggestion`). */
  suggestion?: DidYouMean | null;
  onSuggestion?: (text: string) => void;
}) {
  return (
    <div class="state">
      <h2 class="state-title">{t("noResults.title", { query: `⁨${props.query}⁩` /* bidi-isolated */ })}</h2>
      <Show when={props.suggestion}>
        {(s) => (
          <button
            type="button"
            class="btn-primary did-you-mean"
            onMouseDown={(e) => e.preventDefault()}
            onClick={() => props.onSuggestion?.(s().text)}
          >
            <Show when={s().kind === "layout"} fallback={t("noResults.tryFuzzy")}>
              <DidYouMeanLabel text={s().text} />
            </Show>
          </button>
        )}
      </Show>
      <p class="state-body">{t("noResults.hint")}</p>
      <div class="state-actions">
        <Show when={props.filtered}>
          <button type="button" class="btn-secondary" onMouseDown={(e) => e.preventDefault()} onClick={props.onClearFilter}>
            {t("noResults.allTypes")}
          </button>
        </Show>
        <Show when={props.scoped}>
          <button type="button" class="btn-secondary" onMouseDown={(e) => e.preventDefault()} onClick={() => props.onClearScope?.()}>
            {t("noResults.wholeComputer")}
          </button>
        </Show>
      </div>
    </div>
  );
}

/** "Did you mean: <query>?" with the query bidi-isolated (it may be in the other script). */
function DidYouMeanLabel(props: { text: string }) {
  const parts = () => {
    const template = t("noResults.didYouMean");
    const at = template.indexOf("{query}");
    return at < 0 ? [template, ""] : [template.slice(0, at), template.slice(at + "{query}".length)];
  };
  return (
    <>
      {parts()[0]}
      <bdi class="did-you-mean-query">{props.text}</bdi>
      {parts()[1]}
    </>
  );
}

/** A query typed before the first indexing finished (or after it failed). */
export function NotReady(props: { failed: boolean }) {
  return (
    <div class="state">
      <h2 class="state-title">{t(props.failed ? "notReady.errorTitle" : "notReady.title")}</h2>
      <p class="state-body">{t(props.failed ? "notReady.errorBody" : "notReady.body")}</p>
    </div>
  );
}

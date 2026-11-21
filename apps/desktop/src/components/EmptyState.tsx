import { For, Show } from "solid-js";
import { t, type MessageKey } from "../i18n";

const EXAMPLES = [1, 2, 3, 4] as const;

/** No query yet (design §9.3 "empty state"): a friendly line plus clickable example queries. */
export function EmptyState(props: { onExample: (query: string) => void }) {
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

/** A query with no matches (design §9.3 "no results"). */
export function NoResults(props: { query: string; filtered: boolean; onClearFilter: () => void }) {
  return (
    <div class="state">
      <h2 class="state-title">{t("noResults.title", { query: `⁨${props.query}⁩` /* bidi-isolated */ })}</h2>
      <p class="state-body">{t("noResults.hint")}</p>
      <Show when={props.filtered}>
        <button type="button" class="btn-secondary" onMouseDown={(e) => e.preventDefault()} onClick={props.onClearFilter}>
          {t("noResults.allTypes")}
        </button>
      </Show>
    </div>
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

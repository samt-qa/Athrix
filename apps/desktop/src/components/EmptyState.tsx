import { For, Show, type JSX } from "solid-js";
import { t, type MessageKey } from "../i18n";
import type { IndexState } from "../lib/api";
import { StateIcon, type StateIconName } from "./icons";

const EXAMPLES = [1, 2, 3, 4] as const;

/** Keeps the caret in the search input when a state's button is clicked. */
const keepFocus = (e: MouseEvent) => e.preventDefault();

/**
 * The common shape of every empty / error state (task 5.5): a calm line icon, one short title,
 * one helpful line, at most one action. `live` announces it politely when it appears.
 */
export function StateBlock(props: {
  icon: StateIconName;
  title: JSX.Element;
  body?: JSX.Element;
  action?: JSX.Element;
  live?: boolean;
  class?: string;
  children?: JSX.Element;
}) {
  return (
    <div class={`state ${props.class ?? ""}`.trim()}>
      <StateIcon name={props.icon} />
      {/* The text in its own live region: the icon and the button are not read out with it. */}
      <div class="state-text" role={props.live ? "status" : undefined}>
        <h2 class="state-title">{props.title}</h2>
        <Show when={props.body}>
          <p class="state-body">{props.body}</p>
        </Show>
      </div>
      <Show when={props.action}>
        <div class="state-actions">{props.action}</div>
      </Show>
      {props.children}
    </div>
  );
}

/**
 * No query yet (design §9.3 "empty state"): a friendly line plus clickable example queries.
 * `compact` (the quick-search popup): a single hint line, so the popup stays small.
 * `library`: the main window shows its library panel in this space instead, so only the
 * indexing note (if any) remains here. `indexing`: the first indexing is still running.
 */
export function EmptyState(props: {
  onExample: (query: string) => void;
  compact?: boolean;
  library?: boolean;
  indexing?: boolean;
}) {
  if (props.compact) {
    return <p class="state-hint">{t(props.indexing ? "empty.indexingNote" : "empty.quickHint")}</p>;
  }
  return (
    <Show
      when={!props.library}
      fallback={
        <Show when={props.indexing}>
          <p class="state-hint state-note" role="status">{t("empty.indexingNote")}</p>
        </Show>
      }
    >
      <StateBlock icon="search" class="state-welcome" title={t("empty.title")} body={t("empty.body")}>
        <Show when={props.indexing}>
          <p class="state-note" role="status">{t("empty.indexingNote")}</p>
        </Show>
        <div class="examples" role="group" aria-label={t("empty.examples")}>
          <span class="examples-label" aria-hidden="true">{t("empty.examples")}</span>
          <For each={EXAMPLES}>
            {(n) => {
              const q = () => t(`empty.ex${n}.query` as MessageKey);
              return (
                <button type="button" class="example" onMouseDown={keepFocus} onClick={() => props.onExample(q())}>
                  <bdi class="example-query" dir="auto">{q()}</bdi>
                  <span class="example-desc">{t(`empty.ex${n}.desc` as MessageKey)}</span>
                </button>
              );
            }}
          </For>
        </div>
      </StateBlock>
    </Show>
  );
}

/** A "did you mean" offer: the text to put in the search field and how it was found. */
export interface DidYouMean {
  text: string;
  kind: "layout" | "fuzzy";
}

/**
 * A query with no matches (design §9.3 "no results"). One action only, the most useful first:
 * the "did you mean" rewrite, then widening the scope, then all types, then searching file
 * contents too. The helpful line explains why that action is offered.
 */
export function NoResults(props: {
  query: string;
  filtered: boolean;
  onClearFilter: () => void;
  /** Searching one drive or folder only: offer the whole computer. */
  scoped?: boolean;
  onClearScope?: () => void;
  /** A rewrite of the query that finds results (from `SearchResponse.suggestion`). */
  suggestion?: DidYouMean | null;
  onSuggestion?: (text: string) => void;
  /** Plain words do not search file contents yet: offer to (design §9.3 "no results"). */
  onSearchContent?: () => void;
}) {
  type Offer = { body: MessageKey; action?: JSX.Element };
  const offer = (): Offer => {
    const s = props.suggestion;
    if (s) {
      return {
        body: s.kind === "layout" ? "noResults.hintLayout" : "noResults.hintFuzzy",
        action: (
          <button type="button" class="btn-primary did-you-mean" onMouseDown={keepFocus} onClick={() => props.onSuggestion?.(s.text)}>
            <Show when={s.kind === "layout"} fallback={t("noResults.tryFuzzy")}>
              <DidYouMeanLabel text={s.text} />
            </Show>
          </button>
        ),
      };
    }
    if (props.scoped && props.onClearScope) {
      return { body: "noResults.hintScoped", action: <ActionButton label={t("noResults.wholeComputer")} onClick={props.onClearScope} /> };
    }
    if (props.filtered) {
      return { body: "noResults.hintFiltered", action: <ActionButton label={t("noResults.allTypes")} onClick={props.onClearFilter} /> };
    }
    if (props.onSearchContent) {
      return { body: "noResults.hintContent", action: <ActionButton label={t("noResults.searchContent")} onClick={props.onSearchContent} /> };
    }
    return { body: "noResults.hintSpelling" };
  };
  return (
    <StateBlock
      icon="noResults"
      live
      title={t("noResults.title", { query: `⁨${props.query}⁩` /* bidi-isolated */ })}
      body={t(offer().body)}
      action={offer().action}
    />
  );
}

function ActionButton(props: { label: string; onClick: () => void }) {
  return (
    <button type="button" class="btn-secondary" onMouseDown={keepFocus} onClick={() => props.onClick()}>
      {props.label}
    </button>
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

/**
 * A query typed while there is no usable index: never built (`state` "idle"), the first
 * indexing still running, or it failed (`failed`, with the backend's `message` if given).
 * `onStart` / `onRetry` add the one action; without them the line points at the status bar.
 */
export function NotReady(props: {
  failed: boolean;
  state?: IndexState;
  message?: string | null;
  onRetry?: () => void;
  onStart?: () => void;
}) {
  const kind = () => (props.failed || props.state === "error" ? "error" : props.state === "idle" ? "idle" : "building");
  return (
    <Show
      when={kind() !== "error"}
      fallback={
        <StateBlock
          icon="error"
          live
          class="state-error"
          title={t("notReady.errorTitle")}
          body={props.onRetry ? t("notReady.errorHint") : t("notReady.errorBody")}
          action={props.onRetry ? <ActionButton label={t("index.retry")} onClick={props.onRetry} /> : undefined}
        >
          <Show when={props.message}>
            {(m) => <p class="state-detail"><bdi dir="auto">{m()}</bdi></p>}
          </Show>
        </StateBlock>
      }
    >
      <Show
        when={kind() === "idle"}
        fallback={<StateBlock icon="building" live title={t("notReady.title")} body={t("notReady.body")} />}
      >
        <StateBlock
          icon="notBuilt"
          live
          title={t("notReady.idleTitle")}
          body={t("notReady.idleBody")}
          action={props.onStart ? <ActionButton label={t("notReady.start")} onClick={props.onStart} /> : undefined}
        />
      </Show>
    </Show>
  );
}

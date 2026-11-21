import { Match, Show, Switch, type JSX } from "solid-js";
import { locale, t, tPlural } from "../i18n";
import type { IndexStatus, SearchResponse } from "../lib/api";
import { formatCount, intlLocale } from "../lib/format";
import { rootName } from "../lib/scope";

type Props = {
  index: IndexStatus | null;
  /** The response currently shown (null when there is no query). */
  response: SearchResponse | null;
  notice: string | null;
  mock: boolean;
  onRetry: () => void;
  /** Shown after the index state (the content-indexing control). */
  content?: JSX.Element;
};

function formatMs(ms: number): string {
  return new Intl.NumberFormat(intlLocale(locale()), {
    style: "unit", unit: ms >= 1000 ? "second" : "millisecond", unitDisplay: "short",
    maximumFractionDigits: ms >= 1000 ? 1 : ms < 10 ? 1 : 0,
  }).format(ms >= 1000 ? ms / 1000 : ms);
}

/** Index state, transient notices, and the result count (announced politely). */
export default function StatusBar(props: Props) {
  const count = () => {
    const r = props.response;
    if (!r || r.error) return "";
    const n = r.items.length;
    const formatted = formatCount(n, locale());
    return r.truncated ? t("results.truncated", { count: formatted }) : tPlural("results.count", n, formatted);
  };

  return (
    <footer class="statusbar">
      <span class={`index-state index-${props.index?.state ?? "idle"}`}>
        <span class="dot" aria-hidden="true" />
        <Switch fallback={t("index.idle")}>
          <Match when={props.index?.state === "indexing"}>{t("index.indexing")}</Match>
          <Match when={props.index?.state === "ready" && props.index}>
            {(s) => (
              <span title={s().elapsedMs !== null ? t("index.readyTitle", { time: formatMs(s().elapsedMs!) }) : undefined}>
                {tPlural("index.ready", s().entries, formatCount(s().entries, locale()))}
              </span>
            )}
          </Match>
          <Match when={props.index?.state === "error" && props.index}>
            {(s) => (
              <>
                {t("index.error", { message: s().error ?? "" })}
                <button type="button" class="link-btn" onClick={props.onRetry}>{t("index.retry")}</button>
              </>
            )}
          </Match>
        </Switch>
      </span>
      {props.content}
      <Show when={props.index?.roots.length ? props.index.roots : null}>
        {(roots) => (
          <span class="index-roots" title={roots().join("\n")}>
            {t("index.rootsLabel")} <bdi dir="ltr">{roots().map(rootName).join(", ")}</bdi>
          </span>
        )}
      </Show>
      <Show when={props.mock}>
        <span class="mock-badge" title={t("index.mockTitle")}>{t("index.mock")}</span>
      </Show>
      <span class="status-end">
        <span class="notice" role="status">{props.notice ?? ""}</span>
        <span class="count" aria-live="polite" aria-atomic="true">{count()}</span>
        <Show when={props.response && !props.response.error}>
          <span class="elapsed">{t("results.elapsed", { time: formatMs(props.response!.elapsedMs) })}</span>
        </Show>
      </span>
    </footer>
  );
}

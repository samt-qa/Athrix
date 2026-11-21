import { Show } from "solid-js";
import { hasMessage, t } from "../i18n";
import type { QueryError } from "../lib/api";
import { AlertIcon } from "./icons";

type Props = {
  id: string;
  /** The text the error offsets refer to (what the user typed). */
  text: string;
  error: QueryError | null;
  /** A failed command (not a parse error), already localized. */
  failure: string | null;
  onApplySuggestion: (error: QueryError, suggestion: string) => void;
};

/** Localized message for a QueryError: i18n by `kind`, falling back to the English message. */
export function queryErrorMessage(err: QueryError, text: string): string {
  const key = `queryError.${err.kind}`;
  if (!hasMessage(key)) return err.message;
  const span = text.slice(err.start, err.end).trim() || err.message;
  // First-strong isolates (FSI…PDI) keep "szie:" intact inside Arabic sentences.
  return t(key, { span: `\u2068${span}\u2069` });
}

/** The suggestion as it would appear in the query (a filter keeps its ':'). */
function suggestionText(err: QueryError, text: string): string {
  const s = err.suggestion ?? "";
  return text.slice(err.start, err.end).endsWith(":") && !s.endsWith(":") ? `${s}:` : s;
}

/** One-line error strip under the chips: message + "Did you mean …" action. */
export default function QueryErrorBar(props: Props) {
  return (
    <Show when={props.error || props.failure}>
      <div id={props.id} class="query-error" role="alert">
        <AlertIcon />
        <span class="query-error-text">
          {props.error ? queryErrorMessage(props.error, props.text) : props.failure}
        </span>
        <Show when={props.error?.suggestion && suggestionText(props.error, props.text)}>
          {(s) => (
            <span class="query-error-suggest">
              {t("queryError.didYouMean")}{" "}
              <button
                type="button"
                class="suggestion"
                aria-label={t("queryError.apply", { suggestion: s() })}
                onMouseDown={(e) => e.preventDefault()}
                onClick={() => props.onApplySuggestion(props.error!, props.error!.suggestion!)}
              >
                <bdi dir="auto">{s()}</bdi>
              </button>
              {t("queryError.questionMark")}
            </span>
          )}
        </Show>
      </div>
    </Show>
  );
}
